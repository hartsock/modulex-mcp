//! Tier 3: live-contract tests (#36) — verify the beliefs our mock fixtures
//! encode against REAL tools.
//!
//! Opt-in and deliberately host-dependent:
//!
//! ```bash
//! MODULEX_LIVE_TESTS=1 cargo test -p modulex-core --test live_contract -- --nocapture
//! # or: just live-test
//! ```
//!
//! Without the env var every test exits immediately (default `just check`
//! and PR CI stay host-independent). With it, each test runs the real CLI
//! through the real `TokioSpawner` + leash using harmless, read-only (or
//! test-created-temp) invocations, asserting ONLY the output **shape** our
//! parsers and state matchers key on. A tool absent on the host skips that
//! test with a visible notice — fine HERE, this tier's job is to be
//! host-dependent.
//!
//! **Fixture-sync rule:** every `MockSpawner` fixture mimicking a real tool
//! cites its verifying test below; change one, re-check the other.

use std::sync::Arc;

use agent_bridle_core::{Caveats, Gate, Scope, Tool, ToolContext, ToolResult};
use modulex_core::{
    Config, ExecGate, ExecRequest, RunContext, StepHandler, StepSpec, TokioSpawner,
};

fn live() -> bool {
    if std::env::var_os("MODULEX_LIVE_TESTS").is_some() {
        true
    } else {
        eprintln!("live-contract: skipped (set MODULEX_LIVE_TESTS=1 to run)");
        false
    }
}

struct LiveTool;

#[async_trait::async_trait]
impl Tool for LiveTool {
    fn name(&self) -> &str {
        "live-contract"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    async fn invoke(
        &self,
        _args: serde_json::Value,
        _cx: &ToolContext,
    ) -> ToolResult<serde_json::Value> {
        Ok(serde_json::Value::Null)
    }
}

/// A REAL exec gate (TokioSpawner) granted exactly `programs`.
fn live_gate(programs: &[&str]) -> ExecGate {
    let granted = Caveats {
        exec: Scope::only(programs.iter().map(ToString::to_string)),
        ..Caveats::top()
    };
    let cx = Gate::new(1)
        .authorize(&LiveTool, &granted)
        .expect("authorize");
    ExecGate::new(cx, Arc::new(TokioSpawner))
}

fn live_cx(programs: &[&str], repos: Vec<String>) -> RunContext {
    let mut config = Config::default();
    config.shared.repos = repos;
    RunContext {
        config: Arc::new(config),
        dry_run: false,
        generation: 1,
        exec: live_gate(programs),
        prior: Vec::new(),
        store: None,
    }
}

fn spec(toml_text: &str) -> StepSpec {
    toml::from_str(toml_text).expect("spec parses")
}

/// Skip-with-notice when a tool is absent on this host.
fn tool_present(gate: &ExecGate, program: &str) -> bool {
    if gate.program_available(program) {
        true
    } else {
        eprintln!("live-contract: {program} not on this host — skipping");
        false
    }
}

fn unique_dir(tag: &str) -> std::path::PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "modulex-live-{tag}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

/// Verifies the fixtures behind the git-status/git-unpushed mocks
/// (`steps/git.rs` tests + `tests/data_contract.rs`): real `git` against a
/// repo this test creates, driven through the REAL handlers — clean/dirty
/// state mapping and the no-upstream classification hold against reality.
#[tokio::test(flavor = "multi_thread")]
async fn live_git_status_and_unpushed_states() {
    if !live() {
        return;
    }
    let gate = live_gate(&["git"]);
    if !tool_present(&gate, "git") {
        return;
    }

    let repo = unique_dir("git");
    async fn run(gate: &ExecGate, repo: &std::path::Path, args: &[&str]) {
        let out = gate
            .spawn(
                ExecRequest::new("git")
                    .args(args.iter().map(ToString::to_string).collect())
                    .cwd(repo.to_path_buf()),
            )
            .await
            .expect("git spawns");
        assert!(out.success(), "git failed: {}", out.stderr);
    }
    run(&gate, &repo, &["init", "-q"]).await;
    run(&gate, &repo, &["config", "user.email", "live@test"]).await;
    run(&gate, &repo, &["config", "user.name", "live test"]).await;
    std::fs::write(repo.join("a.txt"), "one").expect("write");
    run(&gate, &repo, &["add", "a.txt"]).await;
    run(&gate, &repo, &["commit", "-q", "-m", "init"]).await;

    let repo_str = repo.to_string_lossy().into_owned();
    let cx = live_cx(&["git"], vec![repo_str.clone()]);
    let status = modulex_core::steps::git::GitStatus;
    let unpushed = modulex_core::steps::git::GitUnpushed;
    let status_spec = spec("name=\"s\"\ntype=\"git-status\"");
    let unpushed_spec = spec("name=\"u\"\ntype=\"git-unpushed\"");

    // Clean tree → state "clean".
    let result = status.run(&status_spec, &cx).await;
    assert_eq!(
        result.data.as_ref().unwrap()["repos"][0]["state"],
        "clean",
        "real git disagrees with the clean-state fixture: {result:?}"
    );

    // Dirty tree → state "dirty" with short-status detail.
    std::fs::write(repo.join("a.txt"), "two").expect("write");
    let result = status.run(&status_spec, &cx).await;
    let data = result.data.as_ref().unwrap();
    assert_eq!(data["repos"][0]["state"], "dirty");
    assert!(
        data["repos"][0]["detail"].as_str().unwrap().contains("M"),
        "expected a short-status 'M' marker: {data}"
    );

    // No upstream configured → classified "no-upstream", success stays true
    // (the `fatal:`-marker belief in steps/git.rs).
    let result = unpushed.run(&unpushed_spec, &cx).await;
    assert!(result.success, "no-upstream must not fail: {result:?}");
    assert_eq!(
        result.data.as_ref().unwrap()["repos"][0]["state"],
        "no-upstream"
    );

    std::fs::remove_dir_all(&repo).ok();
}

/// Verifies the fixture behind `parse_prs` (`steps/github.rs` tests +
/// `tests/data_contract.rs`): real `gh pr list --json` emits an array of
/// objects carrying `number`/`title`/`author.login`. Read-only against a
/// stable public repo. Skips when gh is absent or unauthenticated.
#[tokio::test(flavor = "multi_thread")]
async fn live_gh_pr_list_json_shape() {
    if !live() {
        return;
    }
    let gate = live_gate(&["gh"]);
    if !tool_present(&gate, "gh") {
        return;
    }
    let auth = gate
        .spawn(ExecRequest::new("gh").args(vec!["auth".into(), "status".into()]))
        .await
        .expect("gh spawns");
    if !auth.success() {
        eprintln!("live-contract: gh present but unauthenticated — skipping");
        return;
    }

    let out = gate
        .spawn(
            ExecRequest::new("gh").args(
                [
                    "pr",
                    "list",
                    "--repo",
                    "cli/cli",
                    "--state",
                    "open",
                    "--limit",
                    "2",
                    "--json",
                    "number,title,author,updatedAt",
                ]
                .iter()
                .map(ToString::to_string)
                .collect(),
            ),
        )
        .await
        .expect("gh spawns");
    assert!(out.success(), "gh pr list failed: {}", out.stderr);

    let prs: Vec<serde_json::Value> =
        serde_json::from_str(out.stdout.trim()).expect("gh --json emits a JSON array");
    for pr in &prs {
        assert!(pr["number"].is_u64(), "number field drifted: {pr}");
        assert!(pr["title"].is_string(), "title field drifted: {pr}");
        assert!(
            pr["author"]["login"].is_string(),
            "author.login field drifted: {pr}"
        );
    }
    eprintln!(
        "live-contract: gh --json shape verified over {} PRs",
        prs.len()
    );
}

/// Presence + version probe for glab (the auth-marker belief — `401` /
/// `unauthorized` in stderr — can only be exercised against a configured
/// GitLab host; on such hosts the morning routine itself is the canary).
#[tokio::test(flavor = "multi_thread")]
async fn live_glab_presence() {
    if !live() {
        return;
    }
    let gate = live_gate(&["glab"]);
    if !tool_present(&gate, "glab") {
        return;
    }
    let out = gate
        .spawn(ExecRequest::new("glab").args(vec!["--version".into()]))
        .await
        .expect("glab spawns");
    assert!(out.success(), "glab --version failed: {}", out.stderr);
    eprintln!("live-contract: {}", out.stdout.trim());
}

/// Verifies the harness step's JSON-on-stdout contract against a REAL
/// process (the belief behind the harness fixtures in `steps/script.rs`):
/// a script this test writes emits JSON; the real handler runs it through
/// the real spawner and extracts the typed payload.
#[tokio::test(flavor = "multi_thread")]
async fn live_harness_contract_end_to_end() {
    if !live() {
        return;
    }
    let gate = live_gate(&["sh"]);
    if !tool_present(&gate, "sh") {
        return;
    }

    let dir = unique_dir("harness");
    let script = dir.join("tool.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\nprintf '{\"summary\":\"live ok\",\"n\":7}'\n",
    )
    .expect("write script");

    let mut cx = live_cx(&["sh"], vec![]);
    cx.exec = gate;
    let harness = modulex_core::steps::script::Harness;
    let spec_toml = format!(
        "name=\"t\"\ntype=\"harness\"\ncommand=\"sh\"\nargs=[\"{}\"]",
        script.display()
    );
    let result = harness.run(&spec(&spec_toml), &cx).await;
    assert!(result.success, "harness failed: {result:?}");
    assert_eq!(result.output, "live ok");
    assert_eq!(result.data.as_ref().unwrap()["n"], 7);

    std::fs::remove_dir_all(&dir).ok();
}

/// Verifies the plugin protocol against a REAL python3 (the belief behind
/// `steps/python.rs` fixtures): request on stdin, response on stdout, typed
/// fields mapped.
#[tokio::test(flavor = "multi_thread")]
async fn live_plugin_protocol_with_real_python() {
    if !live() {
        return;
    }
    let gate = live_gate(&["python3"]);
    if !tool_present(&gate, "python3") {
        return;
    }

    let dir = unique_dir("plugin");
    let script = dir.join("plugin.py");
    std::fs::write(
        &script,
        r#"import json, sys
req = json.load(sys.stdin)
assert req["protocol"] == "modulex-plugin/1"
json.dump({"protocol": "modulex-plugin/1", "success": True,
           "output": f"gen {req['generation']}",
           "data": {"echo": req["step"]["name"]}}, sys.stdout)
"#,
    )
    .expect("write plugin");

    let mut cx = live_cx(&["python3"], vec![]);
    cx.exec = gate;
    let plugin = modulex_core::steps::python::PythonPlugin;
    let spec_toml = format!(
        "name=\"live\"\ntype=\"python\"\nscript=\"{}\"",
        script.display()
    );
    let result = plugin.run(&spec(&spec_toml), &cx).await;
    assert!(result.success, "plugin failed: {result:?}");
    assert_eq!(result.output, "gen 1");
    assert_eq!(result.data.as_ref().unwrap()["echo"], "live");

    std::fs::remove_dir_all(&dir).ok();
}
