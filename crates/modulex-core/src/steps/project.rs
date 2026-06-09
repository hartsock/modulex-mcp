//! Project momentum step via the `gh` CLI: `project-status`.
//!
//! Summarizes a GitHub repository's momentum for the morning report: issues
//! closed in the last 7 days (pulse), open issues bucketed by priority label
//! (`p0`/`p1`/`p2`), blocked issues, and open PRs. Auth failures soft-skip
//! with a `gh auth login` hint, mirroring `github-pr-scan`.
//!
//! Dates (the 7-day pulse window) are a display/query window — never a
//! coordination primitive.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use crate::config::StepSpec;
use crate::credentials::Secret;
use crate::exec::{ExecGate, ExecRequest};
use crate::report::{RepoResult, StepResult};
use crate::step::{resolve_step_env, RunContext, StepHandler};

fn is_auth_error(stderr: &str) -> bool {
    let lower = stderr.to_lowercase();
    stderr.contains("401")
        || lower.contains("unauthorized")
        || lower.contains("authentication")
        || lower.contains("gh auth login")
}

/// Outcome of one `gh ... --json` list query.
enum GhOutcome {
    Ok(Vec<Value>),
    Auth(String),
    Err(String),
}

/// Run one `gh` JSON-list query, classifying auth failures.
async fn gh_list(
    exec: &ExecGate,
    env: &[(String, Secret)],
    args: &[&str],
    timeout: u64,
) -> GhOutcome {
    let out = match exec
        .spawn(
            ExecRequest::new("gh")
                .args(args.iter().map(ToString::to_string).collect::<Vec<_>>())
                .env(env.to_vec())
                .timeout(Duration::from_secs(timeout)),
        )
        .await
    {
        Ok(out) => out,
        Err(e) => return GhOutcome::Err(e.to_string()),
    };
    if !out.success() {
        let err = out.stderr.trim().to_string();
        return if is_auth_error(&err) {
            GhOutcome::Auth(err)
        } else if err.is_empty() {
            GhOutcome::Err("gh error".to_string())
        } else {
            GhOutcome::Err(err)
        };
    }
    match serde_json::from_str::<Vec<Value>>(out.stdout.trim()) {
        Ok(v) => GhOutcome::Ok(v),
        Err(e) => GhOutcome::Err(format!("unexpected gh output: {e}")),
    }
}

/// True if an issue carries a label whose name equals `target` (case-insensitive).
fn has_label(issue: &Value, target: &str) -> bool {
    issue
        .get("labels")
        .and_then(Value::as_array)
        .is_some_and(|labels| {
            labels.iter().any(|l| {
                l.get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|n| n.eq_ignore_ascii_case(target))
            })
        })
}

/// `project-status`: GitHub repo momentum (pulse / p0 / p1 / p2 / blocked / PRs).
pub struct ProjectStatus;

#[async_trait]
impl StepHandler for ProjectStatus {
    fn type_name(&self) -> &'static str {
        "project-status"
    }

    fn description(&self) -> &'static str {
        "GitHub project momentum: closes this week, open p0/p1/p2, blocked, open PRs"
    }

    fn data_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["projects"],
            "properties": {
                "projects": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["project", "state"],
                        "properties": {
                            "project": { "type": "string" },
                            "state": { "type": "string",
                                       "enum": ["ok", "auth-failed", "error"] },
                            "pulse_closed_7d": { "type": "integer", "minimum": 0 },
                            "active_p0": { "type": "integer", "minimum": 0 },
                            "up_next_p1": { "type": "integer", "minimum": 0 },
                            "backlog_p2": { "type": "integer", "minimum": 0 },
                            "blocked": { "type": "integer", "minimum": 0 },
                            "open_prs": { "type": "integer", "minimum": 0 },
                            "detail": { "type": "string" }
                        }
                    }
                }
            }
        })
    }

    fn required_programs(&self, _spec: &StepSpec) -> Vec<String> {
        vec!["gh".into()]
    }

    async fn run(&self, spec: &StepSpec, cx: &RunContext) -> StepResult {
        let repos = if spec.repos.is_empty() {
            cx.config.shared.github_repos.clone()
        } else {
            spec.repos.clone()
        };
        if repos.is_empty() {
            return StepResult::ok(&spec.name, &spec.step_type, "No GitHub repos configured.");
        }

        if cx.dry_run {
            let listing: Vec<String> = repos
                .iter()
                .map(|r| format!("[dry-run] would summarize momentum for {r} via gh"))
                .collect();
            return StepResult::ok(&spec.name, &spec.step_type, listing.join("\n"));
        }

        let env = match resolve_step_env(spec, &cx.exec).await {
            Ok(env) => env,
            Err((name, error)) => {
                return StepResult::skip(
                    &spec.name,
                    &spec.step_type,
                    format!("credential {name} unavailable: {error}"),
                );
            }
        };

        // Pulse window: closes in the last 7 days (display/query window only).
        let cutoff = (chrono::Local::now().date_naive() - chrono::Duration::days(7))
            .format("%Y-%m-%d")
            .to_string();
        let closed_search = format!("closed:>={cutoff}");

        let mut data_projects = Vec::with_capacity(repos.len());
        let mut repo_results = Vec::with_capacity(repos.len());
        let mut auth_failures = 0usize;

        for repo in &repos {
            let issues = gh_list(
                &cx.exec,
                &env,
                &[
                    "issue",
                    "list",
                    "-R",
                    repo,
                    "--state",
                    "open",
                    "--json",
                    "number,title,url,labels",
                    "--limit",
                    "200",
                ],
                spec.timeout,
            )
            .await;
            let open_issues = match issues {
                GhOutcome::Ok(v) => v,
                GhOutcome::Auth(detail) => {
                    auth_failures += 1;
                    data_projects.push(serde_json::json!({
                        "project": repo, "state": "auth-failed", "detail": detail,
                    }));
                    repo_results.push(RepoResult {
                        repo: repo.clone(),
                        output: String::new(),
                        success: true,
                        error: Some("Auth failed".to_string()),
                    });
                    continue;
                }
                GhOutcome::Err(detail) => {
                    data_projects.push(serde_json::json!({
                        "project": repo, "state": "error", "detail": detail,
                    }));
                    repo_results.push(RepoResult::err(repo, detail));
                    continue;
                }
            };

            let open_prs = match gh_list(
                &cx.exec,
                &env,
                &[
                    "pr", "list", "-R", repo, "--state", "open", "--json", "number", "--limit",
                    "100",
                ],
                spec.timeout,
            )
            .await
            {
                GhOutcome::Ok(v) => v.len(),
                _ => 0,
            };
            let pulse_closed_7d = match gh_list(
                &cx.exec,
                &env,
                &[
                    "issue",
                    "list",
                    "-R",
                    repo,
                    "--state",
                    "closed",
                    "--search",
                    &closed_search,
                    "--json",
                    "number",
                    "--limit",
                    "100",
                ],
                spec.timeout,
            )
            .await
            {
                GhOutcome::Ok(v) => v.len(),
                _ => 0,
            };

            let active_p0 = open_issues.iter().filter(|i| has_label(i, "p0")).count();
            let up_next_p1 = open_issues.iter().filter(|i| has_label(i, "p1")).count();
            let backlog_p2 = open_issues.iter().filter(|i| has_label(i, "p2")).count();
            let blocked = open_issues
                .iter()
                .filter(|i| has_label(i, "blocked"))
                .count();

            data_projects.push(serde_json::json!({
                "project": repo,
                "state": "ok",
                "pulse_closed_7d": pulse_closed_7d,
                "active_p0": active_p0,
                "up_next_p1": up_next_p1,
                "backlog_p2": backlog_p2,
                "blocked": blocked,
                "open_prs": open_prs,
            }));
            repo_results.push(RepoResult::ok(
                repo,
                format!(
                    "📈 {pulse_closed_7d} closed (7d) · 🔥 {active_p0} p0 · 🎯 {up_next_p1} p1 · \
                     📦 {backlog_p2} p2 · 🚧 {blocked} blocked · 🔀 {open_prs} open PR(s)"
                ),
            ));
        }

        if auth_failures > 0 && auth_failures == repo_results.len() {
            return StepResult::skip(
                &spec.name,
                &spec.step_type,
                "All GitHub queries failed auth — run: gh auth login",
            );
        }

        let mut lines = Vec::new();
        for rr in &repo_results {
            lines.push(format!("### {}", rr.repo));
            match &rr.error {
                Some(error) => lines.push(format!("ERROR: {error}")),
                None => lines.push(rr.output.clone()),
            }
        }
        let mut result =
            StepResult::ok(&spec.name, &spec.step_type, lines.join("\n")).with_repos(repo_results);
        result.data = Some(serde_json::json!({ "projects": data_projects }));
        result
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

    fn cx_with(outputs: Vec<ExecOutput>) -> (RunContext, Arc<MockSpawner>) {
        let spawner = Arc::new(MockSpawner::with_outputs(outputs));
        let granted = Caveats {
            exec: Scope::only(["gh".to_string()]),
            ..Caveats::top()
        };
        let mut config = Config::default();
        config.shared.github_repos = vec!["owner/repo".into()];
        (
            RunContext {
                config: Arc::new(config),
                dry_run: false,
                generation: 1,
                exec: gate_with(&granted, spawner.clone()),
                prior: Vec::new(),
                store: None,
            },
            spawner,
        )
    }

    fn spec() -> StepSpec {
        toml::from_str("name=\"ps\"\ntype=\"project-status\"").unwrap()
    }

    // FIXTURE-SYNC: the JSON below mimics `gh issue/pr list --json` output;
    // the gh JSON-list shape is exercised by live_contract::live_gh_pr_list_json_shape.
    #[tokio::test]
    async fn summarizes_momentum_from_gh_json() {
        // issues (open), then prs, then closed (pulse) — in call order.
        let issues = r#"[
            {"number":1,"title":"urgent","url":"u","labels":[{"name":"p0"}]},
            {"number":2,"title":"soon","url":"u","labels":[{"name":"p1"}]},
            {"number":3,"title":"later","url":"u","labels":[{"name":"p2"}]},
            {"number":4,"title":"stuck","url":"u","labels":[{"name":"blocked"},{"name":"p1"}]}
        ]"#;
        let prs = r#"[{"number":9},{"number":10}]"#;
        let closed = r#"[{"number":7},{"number":8},{"number":11}]"#;
        let (cx, spawner) = cx_with(vec![
            MockSpawner::ok(issues),
            MockSpawner::ok(prs),
            MockSpawner::ok(closed),
        ]);
        let result = ProjectStatus.run(&spec(), &cx).await;
        assert!(result.success);
        let data = result.data.unwrap();
        let p = &data["projects"][0];
        assert_eq!(p["state"], "ok");
        assert_eq!(p["pulse_closed_7d"], 3);
        assert_eq!(p["active_p0"], 1);
        assert_eq!(p["up_next_p1"], 2, "two issues carry p1");
        assert_eq!(p["backlog_p2"], 1);
        assert_eq!(p["blocked"], 1);
        assert_eq!(p["open_prs"], 2);
        assert!(result.output.contains("3 closed (7d)"));

        // first call queries open issues with labels.
        let calls = spawner.calls.lock().unwrap();
        assert_eq!(calls[0].1[0], "issue");
        assert!(calls[0]
            .1
            .iter()
            .any(|a| a == "labels" || a == "number,title,url,labels"));
        assert!(calls[2].1.iter().any(|a| a.starts_with("closed:>=")));
    }

    #[tokio::test]
    async fn auth_failure_soft_skips() {
        let (cx, _) = cx_with(vec![MockSpawner::fail("HTTP 401: run: gh auth login", 1)]);
        let result = ProjectStatus.run(&spec(), &cx).await;
        assert!(result.skipped);
        assert!(result.output.contains("gh auth login"));
    }

    #[tokio::test]
    async fn no_repos_is_ok_noop() {
        let spawner = Arc::new(MockSpawner::with_outputs(vec![]));
        let granted = Caveats {
            exec: Scope::only(["gh".to_string()]),
            ..Caveats::top()
        };
        let cx = RunContext {
            config: Arc::new(Config::default()),
            dry_run: false,
            generation: 1,
            exec: gate_with(&granted, spawner),
            prior: Vec::new(),
            store: None,
        };
        let result = ProjectStatus.run(&spec(), &cx).await;
        assert!(result.success);
        assert!(result.output.contains("No GitHub repos"));
    }
}
