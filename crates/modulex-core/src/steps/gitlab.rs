//! GitLab steps via the `glab` CLI: `gitlab-mr-authored`, `gitlab-mr-review`,
//! `gitlab-group-mrs`, and the derived `mr-sla-check`.
//!
//! Output is `glab`'s human text, passed through per project/group — the
//! same contract the proven Python handlers used. Auth failures
//! (401/unauthorized/forbidden) are *soft*: when every query fails auth, the
//! step skips with a `glab auth login` hint instead of failing the routine.

use std::time::Duration;

use async_trait::async_trait;
use chrono::Duration as ChronoDuration;

use crate::config::StepSpec;
use crate::credentials::Secret;
use crate::exec::{ExecGate, ExecRequest};
use crate::report::{RepoResult, StepResult};
use crate::step::{resolve_step_env, RunContext, StepHandler};

/// Marker prefix for auth failures, recognized when aggregating.
const AUTH_FAILED: &str = "Auth failed (token expired?)";

fn is_auth_error(stderr: &str) -> bool {
    let lower = stderr.to_lowercase();
    stderr.contains("401") || lower.contains("unauthorized") || lower.contains("forbidden")
}

/// Resolve the step's credential references, mapping a miss to the standard
/// soft skip. Shared by every forge step.
async fn step_env_or_skip(
    spec: &StepSpec,
    cx: &RunContext,
) -> Result<Vec<(String, Secret)>, StepResult> {
    resolve_step_env(spec, &cx.exec)
        .await
        .map_err(|(name, error)| {
            StepResult::skip(
                &spec.name,
                &spec.step_type,
                format!("credential {name} unavailable: {error}"),
            )
        })
}

/// Run one glab query; classify auth failures as recoverable.
async fn run_glab(
    exec: &ExecGate,
    label: &str,
    args: Vec<String>,
    env: Vec<(String, Secret)>,
    timeout: u64,
) -> RepoResult {
    let out = match exec
        .spawn(
            ExecRequest::new("glab")
                .args(args)
                .env(env)
                .timeout(Duration::from_secs(timeout)),
        )
        .await
    {
        Ok(out) => out,
        Err(e) => return RepoResult::err(label, e.to_string()),
    };
    if !out.success() {
        let err = out.stderr.trim().to_string();
        if is_auth_error(&err) {
            // success=true + error marker: noted, not fatal (Python parity).
            return RepoResult {
                repo: label.to_string(),
                output: String::new(),
                success: true,
                error: Some(format!("{AUTH_FAILED}: {err}")),
            };
        }
        return RepoResult::err(
            label,
            if err.is_empty() {
                "glab error".into()
            } else {
                err
            },
        );
    }
    let trimmed = out.stdout.trim();
    RepoResult::ok(
        label,
        if trimmed.is_empty() {
            "(none)"
        } else {
            trimmed
        },
    )
}

/// Aggregate per-project results with the all-auth-failed soft skip.
fn aggregate(
    spec: &StepSpec,
    cx: &RunContext,
    repo_results: Vec<RepoResult>,
    empty_message: &str,
) -> StepResult {
    let auth_failures = repo_results
        .iter()
        .filter(|rr| rr.error.as_deref().is_some_and(|e| e.contains(AUTH_FAILED)))
        .count();
    if auth_failures > 0 && auth_failures == repo_results.len() {
        return StepResult::skip(
            &spec.name,
            &spec.step_type,
            format!(
                "All GitLab queries failed — token likely expired. Run: glab auth login --hostname {}",
                cx.config.identity.gitlab_host
            ),
        );
    }

    let mut lines = Vec::new();
    for rr in &repo_results {
        if rr.error.as_deref().is_some_and(|e| e.contains(AUTH_FAILED)) {
            continue; // noted in aggregate, not per project
        }
        lines.push(format!("### {}", rr.repo));
        match &rr.error {
            Some(error) => lines.push(format!("ERROR: {error}")),
            None if rr.output == "(none)" => lines.push("(no open MRs)".to_string()),
            None => lines.push(rr.output.clone()),
        }
    }

    let any_mrs = repo_results
        .iter()
        .any(|rr| rr.error.is_none() && rr.output != "(none)");
    let body = if any_mrs || lines.iter().any(|l| l.starts_with("ERROR")) {
        lines.join("\n")
    } else {
        empty_message.to_string()
    };

    StepResult::ok(&spec.name, &spec.step_type, body).with_repos(repo_results)
}

/// Shared shape of the authored/review steps (only the role flag differs).
async fn mr_query_step(
    spec: &StepSpec,
    cx: &RunContext,
    flag: &str,
    empty_message: &str,
) -> StepResult {
    let projects = if spec.repos.is_empty() {
        cx.config.shared.gitlab_projects.clone()
    } else {
        spec.repos.clone()
    };
    if projects.is_empty() {
        return StepResult::ok(
            &spec.name,
            &spec.step_type,
            "No GitLab projects configured.",
        );
    }
    let username = &cx.config.identity.username;
    if username.is_empty() {
        return StepResult::skip(
            &spec.name,
            &spec.step_type,
            "No username configured in identity section.",
        );
    }

    if cx.dry_run {
        let listing: Vec<String> = projects
            .iter()
            .map(|p| format!("[dry-run] would run: glab mr list {flag}={username} -R {p}"))
            .collect();
        return StepResult::ok(&spec.name, &spec.step_type, listing.join("\n"));
    }

    let env = match step_env_or_skip(spec, cx).await {
        Ok(env) => env,
        Err(skip) => return skip,
    };
    let mut repo_results = Vec::with_capacity(projects.len());
    for project in &projects {
        let args = vec![
            "mr".to_string(),
            "list".to_string(),
            format!("{flag}={username}"),
            "-R".to_string(),
            project.clone(),
        ];
        repo_results.push(run_glab(&cx.exec, project, args, env.clone(), spec.timeout).await);
    }
    aggregate(spec, cx, repo_results, empty_message)
}

/// `gitlab-mr-authored`: open MRs the user authored, per configured project.
pub struct GitlabMrAuthored;

#[async_trait]
impl StepHandler for GitlabMrAuthored {
    fn type_name(&self) -> &'static str {
        "gitlab-mr-authored"
    }

    fn required_programs(&self, _spec: &StepSpec) -> Vec<String> {
        vec!["glab".into()]
    }

    async fn run(&self, spec: &StepSpec, cx: &RunContext) -> StepResult {
        mr_query_step(spec, cx, "--author", "(no open MRs authored)").await
    }
}

/// `gitlab-mr-review`: open MRs where the user is a reviewer.
pub struct GitlabMrReview;

#[async_trait]
impl StepHandler for GitlabMrReview {
    fn type_name(&self) -> &'static str {
        "gitlab-mr-review"
    }

    fn required_programs(&self, _spec: &StepSpec) -> Vec<String> {
        vec!["glab".into()]
    }

    async fn run(&self, spec: &StepSpec, cx: &RunContext) -> StepResult {
        mr_query_step(spec, cx, "--reviewer", "(no review requests)").await
    }
}

/// `gitlab-group-mrs`: open MR activity across configured groups
/// (`scan = "recent"` limits to the last 7 days).
pub struct GitlabGroupMrs;

#[async_trait]
impl StepHandler for GitlabGroupMrs {
    fn type_name(&self) -> &'static str {
        "gitlab-group-mrs"
    }

    fn required_programs(&self, _spec: &StepSpec) -> Vec<String> {
        vec!["glab".into()]
    }

    async fn run(&self, spec: &StepSpec, cx: &RunContext) -> StepResult {
        let groups = &cx.config.shared.gitlab_groups;
        if groups.is_empty() {
            return StepResult::ok(&spec.name, &spec.step_type, "No GitLab groups configured.");
        }

        if cx.dry_run {
            let listing: Vec<String> = groups
                .iter()
                .map(|g| {
                    format!(
                        "[dry-run] would run: glab mr list -g {} --per-page {}",
                        g.name, g.per_page
                    )
                })
                .collect();
            return StepResult::ok(&spec.name, &spec.step_type, listing.join("\n"));
        }

        let env = match step_env_or_skip(spec, cx).await {
            Ok(env) => env,
            Err(skip) => return skip,
        };
        let mut repo_results = Vec::with_capacity(groups.len());
        for group in groups {
            let mut args = vec![
                "mr".to_string(),
                "list".to_string(),
                "-g".to_string(),
                group.name.clone(),
                "--per-page".to_string(),
                group.per_page.to_string(),
            ];
            if group.scan == "recent" {
                // Display/query window — never a coordination primitive.
                let cutoff = (chrono::Local::now().date_naive() - ChronoDuration::days(7))
                    .format("%Y-%m-%dT00:00:00Z")
                    .to_string();
                args.push("--created-after".to_string());
                args.push(cutoff);
            }
            repo_results
                .push(run_glab(&cx.exec, &group.name, args, env.clone(), spec.timeout).await);
        }
        aggregate(spec, cx, repo_results, "(no group MR activity)")
    }
}

/// `mr-sla-check`: derived step — summarizes the review queue from prior
/// step results against a response-hours threshold (full per-comment SLA
/// timelines are an agent-side job; this step keeps the report honest).
pub struct MrSlaCheck;

#[async_trait]
impl StepHandler for MrSlaCheck {
    fn type_name(&self) -> &'static str {
        "mr-sla-check"
    }

    fn required_programs(&self, _spec: &StepSpec) -> Vec<String> {
        vec![] // reads prior results; spawns nothing
    }

    async fn run(&self, spec: &StepSpec, cx: &RunContext) -> StepResult {
        let hours = spec.param_int("response_hours").unwrap_or(24);
        if cx.dry_run {
            return StepResult::ok(
                &spec.name,
                &spec.step_type,
                format!("[dry-run] would check MR SLA ({hours}h threshold)"),
            );
        }

        // Count projects with live review requests from the prior
        // gitlab-mr-review result in this run.
        let pending: usize = cx
            .prior
            .iter()
            .filter(|r| r.step_type == "gitlab-mr-review")
            .flat_map(|r| r.repo_results.iter())
            .filter(|rr| rr.error.is_none() && rr.output != "(none)")
            .count();

        let body = if pending == 0 {
            format!("SLA threshold {hours}h — no pending review requests in this run.")
        } else {
            format!(
                "SLA threshold {hours}h — {pending} project(s) with open review requests \
                 (see the review-queue section above). Anything older than {hours}h needs \
                 a response today."
            )
        };
        StepResult::ok(&spec.name, &spec.step_type, body)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use agent_bridle_core::{Caveats, Scope};

    use super::*;
    use crate::config::Config;
    use crate::exec::test_support::{gate_with, MockSpawner};
    use crate::exec::ExecOutput;

    fn cx_with(config_toml: &str, outputs: Vec<ExecOutput>) -> (RunContext, Arc<MockSpawner>) {
        let spawner = Arc::new(MockSpawner::with_outputs(outputs));
        let granted = Caveats {
            exec: Scope::only(["glab".to_string()]),
            ..Caveats::top()
        };
        (
            RunContext {
                config: Arc::new(Config::from_toml(config_toml).unwrap()),
                dry_run: false,
                generation: 1,
                exec: gate_with(&granted, spawner.clone()),
                prior: Vec::new(),
            },
            spawner,
        )
    }

    const GL_CONFIG: &str = r#"
[identity]
username = "someone"
gitlab_host = "gitlab.example.com"

[shared]
gitlab_projects = ["group/a", "group/b"]

[[shared.gitlab_groups]]
name = "group"
scan = "all"
"#;

    fn spec(step_type: &str) -> StepSpec {
        toml::from_str(&format!("name=\"t\"\ntype=\"{step_type}\"")).unwrap()
    }

    #[tokio::test]
    async fn review_step_passes_reviewer_flag_and_renders_sections() {
        let (cx, spawner) = cx_with(
            GL_CONFIG,
            vec![
                MockSpawner::ok("!123 fix the thing"),
                MockSpawner::ok(""), // (none)
            ],
        );
        let result = GitlabMrReview.run(&spec("gitlab-mr-review"), &cx).await;
        assert!(result.success);
        assert!(result.output.contains("### group/a\n!123 fix the thing"));
        let calls = spawner.calls.lock().unwrap();
        assert_eq!(
            calls[0].1,
            vec!["mr", "list", "--reviewer=someone", "-R", "group/a"]
        );
    }

    #[tokio::test]
    async fn all_auth_failures_soft_skip_with_login_hint() {
        let (cx, _) = cx_with(
            GL_CONFIG,
            vec![
                MockSpawner::fail("HTTP 401 unauthorized", 1),
                MockSpawner::fail("forbidden", 1),
            ],
        );
        let result = GitlabMrReview.run(&spec("gitlab-mr-review"), &cx).await;
        assert!(result.skipped, "all-auth-failed must soft skip");
        assert!(result
            .output
            .contains("glab auth login --hostname gitlab.example.com"));
    }

    #[tokio::test]
    async fn partial_auth_failure_keeps_other_results() {
        let (cx, _) = cx_with(
            GL_CONFIG,
            vec![
                MockSpawner::fail("401 unauthorized", 1),
                MockSpawner::ok("!7 open thing"),
            ],
        );
        let result = GitlabMrReview.run(&spec("gitlab-mr-review"), &cx).await;
        assert!(!result.skipped);
        assert!(result.success);
        assert!(result.output.contains("!7 open thing"));
        assert!(!result.output.contains("401"), "auth noise filtered");
    }

    #[tokio::test]
    async fn missing_username_soft_skips() {
        let (cx, _) = cx_with("[shared]\ngitlab_projects = [\"g/a\"]", vec![]);
        let result = GitlabMrAuthored.run(&spec("gitlab-mr-authored"), &cx).await;
        assert!(result.skipped);
        assert!(result.output.contains("username"));
    }

    #[tokio::test]
    async fn group_scan_all_omits_created_after() {
        let (cx, spawner) = cx_with(GL_CONFIG, vec![MockSpawner::ok("!1 mr")]);
        let result = GitlabGroupMrs.run(&spec("gitlab-group-mrs"), &cx).await;
        assert!(result.success);
        let calls = spawner.calls.lock().unwrap();
        assert!(!calls[0].1.iter().any(|a| a == "--created-after"));
        assert!(calls[0].1.iter().any(|a| a == "--per-page"));
    }

    #[tokio::test]
    async fn group_scan_recent_adds_created_after() {
        let (cx, spawner) = cx_with(
            r#"
[identity]
username = "u"
[[shared.gitlab_groups]]
name = "g"
scan = "recent"
"#,
            vec![MockSpawner::ok("")],
        );
        GitlabGroupMrs.run(&spec("gitlab-group-mrs"), &cx).await;
        let calls = spawner.calls.lock().unwrap();
        let args = &calls[0].1;
        let pos = args.iter().position(|a| a == "--created-after").unwrap();
        assert!(args[pos + 1].ends_with("T00:00:00Z"));
    }

    #[tokio::test]
    async fn missing_credential_soft_skips_the_forge_step() {
        // Regression (fresh-eyes 2026-06-05): forge steps ignored spec.env —
        // credential references on gitlab/github steps were silently dropped.
        let (cx, spawner) = cx_with(GL_CONFIG, vec![]);
        let spec: StepSpec = toml::from_str(
            "name=\"rq\"\ntype=\"gitlab-mr-review\"\n\
             env = { GITLAB_TOKEN = { env = \"MODULEX_TEST_UNSET_XYZZY\" } }",
        )
        .unwrap();
        let result = GitlabMrReview.run(&spec, &cx).await;
        assert!(result.skipped);
        assert!(result.output.contains("GITLAB_TOKEN"));
        assert!(spawner.calls.lock().unwrap().is_empty(), "never spawned");
    }

    #[tokio::test]
    async fn sla_check_counts_pending_reviews_from_prior() {
        let (mut cx, _) = cx_with(GL_CONFIG, vec![]);
        cx.prior = vec![
            StepResult::ok("review queue", "gitlab-mr-review", "x").with_repos(vec![
                RepoResult::ok("group/a", "!1 thing"),
                RepoResult::ok("group/b", "(none)"),
            ]),
        ];
        let result = MrSlaCheck.run(&spec("mr-sla-check"), &cx).await;
        assert!(result.output.contains("1 project(s)"));
        assert!(result.output.contains("24h"));
    }

    #[tokio::test]
    async fn sla_check_honors_response_hours_param() {
        let (cx, _) = cx_with(GL_CONFIG, vec![]);
        let spec: StepSpec =
            toml::from_str("name=\"t\"\ntype=\"mr-sla-check\"\nresponse_hours = 48").unwrap();
        let result = MrSlaCheck.run(&spec, &cx).await;
        assert!(result.output.contains("48h"));
        assert!(result.output.contains("no pending review requests"));
    }
}
