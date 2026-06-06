//! Git repo-health steps: `git-tend`, `git-status`, `git-unpushed`.
//!
//! Semantics ported from gila-plugin-morning's handlers: per-repo fan-out,
//! `### repo` section per repo, soft handling of "no upstream", and a
//! `N/M repos tended` summary for git-tend. Conflicts are reported, never
//! auto-resolved.

use std::time::Duration;

use async_trait::async_trait;

use crate::config::{expand_tilde, StepSpec};
use crate::exec::{ExecError, ExecGate, ExecOutput, ExecRequest};
use crate::report::{RepoResult, StepResult};
use crate::step::{repos_for, RunContext, StepHandler};

/// Run a git subcommand in `repo`, returning the gate's output.
async fn run_git(
    exec: &ExecGate,
    repo: &str,
    args: &[&str],
    timeout: u64,
) -> Result<ExecOutput, ExecError> {
    let path = expand_tilde(repo);
    exec.spawn(
        ExecRequest::new("git")
            .args(args.iter().map(ToString::to_string).collect())
            .cwd(path)
            .timeout(Duration::from_secs(timeout)),
    )
    .await
}

/// Fan a per-repo closure across the step's repo list, sequentially when the
/// step is not parallel (the engine already parallelizes across *steps*;
/// in-step fan-out stays simple and ordered).
///
/// `per_repo` returns the human-facing [`RepoResult`] plus the typed `state`
/// enum for the data contract.
async fn fan_out<F, Fut>(spec: &StepSpec, cx: &RunContext, per_repo: F) -> StepResult
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = (RepoResult, &'static str)>,
{
    let repos = repos_for(spec, &cx.config);
    if repos.is_empty() {
        return StepResult::ok(&spec.name, &spec.step_type, "No repos configured.");
    }

    let mut repo_results = Vec::with_capacity(repos.len());
    let mut data_repos = Vec::with_capacity(repos.len());
    for repo in repos {
        let (rr, state) = per_repo(repo).await;
        data_repos.push(serde_json::json!({
            "repo": rr.repo,
            "state": state,
            "detail": rr.error.clone().unwrap_or_else(|| rr.output.clone()),
        }));
        repo_results.push(rr);
    }

    let mut lines = Vec::new();
    for rr in &repo_results {
        lines.push(format!("### {}", rr.repo));
        match &rr.error {
            Some(error) => lines.push(format!("ERROR: {error}")),
            None => lines.push(rr.output.clone()),
        }
    }
    let body = lines.join("\n");
    let mut result = StepResult::ok(&spec.name, &spec.step_type, body).with_repos(repo_results);
    let mut data = serde_json::json!({ "repos": data_repos });
    if spec.step_type == "git-tend" {
        // Prepend the tend summary line.
        let total = result.repo_results.len();
        let tended = result.repo_results.iter().filter(|r| r.success).count();
        let failed = total - tended;
        let mut summary = format!("{tended}/{total} repos tended");
        if failed > 0 {
            summary.push_str(&format!(", {failed} failed"));
        }
        result.output = format!("{summary}\n{}", result.output);
        data["summary"] = serde_json::json!({ "tended": tended, "failed": failed, "total": total });
    }
    if !cx.dry_run {
        result.data = Some(data);
    }
    result
}

/// Shared schema fragment: the per-repo data array.
fn repos_schema(states: &[&str], extra_desc: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "required": ["repos"],
        "properties": {
            "repos": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["repo", "state", "detail"],
                    "properties": {
                        "repo": { "type": "string" },
                        "state": { "type": "string", "enum": states },
                        "detail": { "type": "string", "description": extra_desc }
                    }
                }
            }
        }
    })
}

/// `git-status`: `git status --short` per repo; empty output renders `(clean)`.
pub struct GitStatus;

#[async_trait]
impl StepHandler for GitStatus {
    fn type_name(&self) -> &'static str {
        "git-status"
    }

    fn description(&self) -> &'static str {
        "Working-tree status per repo (git status --short)"
    }

    fn data_schema(&self) -> serde_json::Value {
        repos_schema(
            &["clean", "dirty", "error"],
            "short-status lines when dirty; error text on error",
        )
    }

    fn required_programs(&self, _spec: &StepSpec) -> Vec<String> {
        vec!["git".into()]
    }

    async fn run(&self, spec: &StepSpec, cx: &RunContext) -> StepResult {
        fan_out(spec, cx, |repo| async move {
            if cx.dry_run {
                return (
                    RepoResult::ok(&repo, "[dry-run] would run: git status --short"),
                    "clean",
                );
            }
            match run_git(&cx.exec, &repo, &["status", "--short"], spec.timeout).await {
                Ok(out) if out.success() => {
                    let trimmed = out.stdout.trim();
                    if trimmed.is_empty() {
                        (RepoResult::ok(&repo, "(clean)"), "clean")
                    } else {
                        (RepoResult::ok(&repo, trimmed), "dirty")
                    }
                }
                Ok(out) => (
                    RepoResult::err(&repo, nonempty(&out.stderr, "git error")),
                    "error",
                ),
                Err(e) => (RepoResult::err(&repo, e.to_string()), "error"),
            }
        })
        .await
    }
}

/// `git-unpushed`: `git log @{u}..HEAD --oneline` per repo. "No upstream" is
/// information, not an error.
pub struct GitUnpushed;

#[async_trait]
impl StepHandler for GitUnpushed {
    fn type_name(&self) -> &'static str {
        "git-unpushed"
    }

    fn description(&self) -> &'static str {
        "Local commits not yet pushed, per repo"
    }

    fn data_schema(&self) -> serde_json::Value {
        repos_schema(
            &["all-pushed", "unpushed", "no-upstream", "error"],
            "oneline commit list when unpushed",
        )
    }

    fn required_programs(&self, _spec: &StepSpec) -> Vec<String> {
        vec!["git".into()]
    }

    async fn run(&self, spec: &StepSpec, cx: &RunContext) -> StepResult {
        fan_out(spec, cx, |repo| async move {
            if cx.dry_run {
                return (
                    RepoResult::ok(&repo, "[dry-run] would run: git log @{u}..HEAD --oneline"),
                    "all-pushed",
                );
            }
            match run_git(
                &cx.exec,
                &repo,
                &["log", "@{u}..HEAD", "--oneline"],
                spec.timeout,
            )
            .await
            {
                Ok(out) if out.success() => {
                    let trimmed = out.stdout.trim();
                    if trimmed.is_empty() {
                        (RepoResult::ok(&repo, "(all pushed)"), "all-pushed")
                    } else {
                        (RepoResult::ok(&repo, trimmed), "unpushed")
                    }
                }
                Ok(out) => {
                    let msg = out.stderr.trim().to_string();
                    let lower = msg.to_lowercase();
                    if lower.contains("no upstream") || lower.contains("fatal") {
                        (
                            RepoResult::ok(&repo, "(no upstream configured)"),
                            "no-upstream",
                        )
                    } else {
                        (RepoResult::err(&repo, nonempty(&msg, "git error")), "error")
                    }
                }
                Err(e) => (RepoResult::err(&repo, e.to_string()), "error"),
            }
        })
        .await
    }
}

/// `git-tend`: `git fetch --all --prune` then `git pull --ff-only` per repo.
/// A non-fast-forward pull is reported as a conflict for the human — never
/// auto-resolved.
pub struct GitTend;

#[async_trait]
impl StepHandler for GitTend {
    fn type_name(&self) -> &'static str {
        "git-tend"
    }

    fn description(&self) -> &'static str {
        "Fetch + fast-forward pull per repo; conflicts reported, never resolved"
    }

    fn data_schema(&self) -> serde_json::Value {
        let mut schema = repos_schema(
            &[
                "ok",
                "no-tracking",
                "diverged",
                "fetch-failed",
                "pull-failed",
                "error",
            ],
            "last pull line on ok; failure reason otherwise",
        );
        schema["required"] = serde_json::json!(["repos", "summary"]);
        schema["properties"]["summary"] = serde_json::json!({
            "type": "object",
            "required": ["tended", "failed", "total"],
            "properties": {
                "tended": { "type": "integer" },
                "failed": { "type": "integer" },
                "total": { "type": "integer" }
            }
        });
        schema
    }

    fn required_programs(&self, _spec: &StepSpec) -> Vec<String> {
        vec!["git".into()]
    }

    async fn run(&self, spec: &StepSpec, cx: &RunContext) -> StepResult {
        fan_out(spec, cx, |repo| async move {
            if cx.dry_run {
                return (
                    RepoResult::ok(
                        &repo,
                        "[dry-run] would run: git fetch --all --prune && git pull --ff-only",
                    ),
                    "ok",
                );
            }
            let fetch = match run_git(
                &cx.exec,
                &repo,
                &["fetch", "--all", "--prune"],
                spec.timeout,
            )
            .await
            {
                Ok(out) => out,
                Err(e) => return (RepoResult::err(&repo, e.to_string()), "error"),
            };
            if !fetch.success() {
                return (
                    RepoResult::err(
                        &repo,
                        format!(
                            "fetch failed: {}",
                            nonempty(fetch.stderr.trim(), "git error")
                        ),
                    ),
                    "fetch-failed",
                );
            }
            let pull = match run_git(&cx.exec, &repo, &["pull", "--ff-only"], spec.timeout).await {
                Ok(out) => out,
                Err(e) => return (RepoResult::err(&repo, e.to_string()), "error"),
            };
            if pull.success() {
                let line = pull
                    .stdout
                    .trim()
                    .lines()
                    .last()
                    .unwrap_or("ok")
                    .to_string();
                return (RepoResult::ok(&repo, line), "ok");
            }
            let msg = pull.stderr.trim().to_lowercase();
            if msg.contains("not possible to fast-forward")
                || msg.contains("diverg")
                || msg.contains("would be overwritten")
            {
                (
                    RepoResult::err(&repo, "diverged from upstream — manual resolution needed"),
                    "diverged",
                )
            } else if msg.contains("no tracking") || msg.contains("no such ref") {
                (
                    RepoResult::ok(&repo, "(no tracking branch — fetched only)"),
                    "no-tracking",
                )
            } else {
                (
                    RepoResult::err(
                        &repo,
                        format!("pull failed: {}", nonempty(pull.stderr.trim(), "git error")),
                    ),
                    "pull-failed",
                )
            }
        })
        .await
    }
}

fn nonempty(value: &str, fallback: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use agent_bridle_core::{Caveats, Scope};

    use super::*;
    use crate::config::Config;
    use crate::exec::test_support::{gate_with, MockSpawner};

    fn cx_with(
        repos: &[&str],
        outputs: Vec<ExecOutput>,
        spawner_out: &mut Arc<MockSpawner>,
    ) -> RunContext {
        let spawner = Arc::new(MockSpawner::with_outputs(outputs));
        *spawner_out = spawner.clone();
        let granted = Caveats {
            exec: Scope::only(["git".to_string()]),
            ..Caveats::top()
        };
        let mut config = Config::default();
        config.shared.repos = repos.iter().map(ToString::to_string).collect();
        RunContext {
            config: Arc::new(config),
            dry_run: false,
            generation: 1,
            exec: gate_with(&granted, spawner),
            prior: Vec::new(),
            store: None,
        }
    }

    fn spec(step_type: &str) -> StepSpec {
        let toml = format!("name = \"t\"\ntype = \"{step_type}\"");
        toml::from_str(&toml).unwrap()
    }

    #[tokio::test]
    async fn git_status_renders_clean_and_dirty_repos() {
        let mut spawner = Arc::new(MockSpawner::default());
        let cx = cx_with(
            &["/r/clean", "/r/dirty"],
            vec![MockSpawner::ok("  \n"), MockSpawner::ok(" M src/lib.rs")],
            &mut spawner,
        );
        let result = GitStatus.run(&spec("git-status"), &cx).await;
        assert!(result.success);
        assert!(result.output.contains("### /r/clean\n(clean)"));
        assert!(result.output.contains("### /r/dirty\nM src/lib.rs"));
        // Both calls were `git status --short`.
        let calls = spawner.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1, vec!["status", "--short"]);
    }

    #[tokio::test]
    async fn git_unpushed_treats_no_upstream_as_info() {
        let mut spawner = Arc::new(MockSpawner::default());
        let cx = cx_with(
            &["/r/a"],
            vec![MockSpawner::fail(
                "fatal: no upstream configured for branch",
                128,
            )],
            &mut spawner,
        );
        let result = GitUnpushed.run(&spec("git-unpushed"), &cx).await;
        assert!(result.success, "no upstream is not an error");
        assert!(result.output.contains("(no upstream configured)"));
    }

    #[tokio::test]
    async fn git_tend_reports_divergence_without_resolving() {
        let mut spawner = Arc::new(MockSpawner::default());
        let cx = cx_with(
            &["/r/a"],
            vec![
                MockSpawner::ok("Fetching origin"), // fetch ok
                MockSpawner::fail("fatal: Not possible to fast-forward, aborting.", 128),
            ],
            &mut spawner,
        );
        let result = GitTend.run(&spec("git-tend"), &cx).await;
        assert!(!result.success);
        assert!(result.output.starts_with("0/1 repos tended, 1 failed"));
        assert!(result.output.contains("diverged from upstream"));
    }

    #[tokio::test]
    async fn git_tend_success_summary_counts() {
        let mut spawner = Arc::new(MockSpawner::default());
        let cx = cx_with(
            &["/r/a"],
            vec![
                MockSpawner::ok(""),                      // fetch
                MockSpawner::ok("Already up to date.\n"), // pull
            ],
            &mut spawner,
        );
        let result = GitTend.run(&spec("git-tend"), &cx).await;
        assert!(result.success);
        assert!(result.output.starts_with("1/1 repos tended"));
        assert!(result.output.contains("Already up to date."));
    }

    #[tokio::test]
    async fn dry_run_spawns_nothing() {
        let mut spawner = Arc::new(MockSpawner::default());
        let mut cx = cx_with(&["/r/a"], vec![], &mut spawner);
        cx.dry_run = true;
        let result = GitTend.run(&spec("git-tend"), &cx).await;
        assert!(result.output.contains("[dry-run]"));
        assert!(spawner.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn no_repos_configured_is_informational() {
        let mut spawner = Arc::new(MockSpawner::default());
        let cx = cx_with(&[], vec![], &mut spawner);
        let result = GitStatus.run(&spec("git-status"), &cx).await;
        assert!(result.success);
        assert_eq!(result.output, "No repos configured.");
    }
}
