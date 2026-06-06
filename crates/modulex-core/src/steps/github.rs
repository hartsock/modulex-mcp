//! GitHub step via the `gh` CLI: `github-pr-scan`.
//!
//! Uses `gh pr list --json` (stable across gh versions) and renders a
//! compact line per PR. Auth failures soft-skip with a `gh auth login`
//! hint, mirroring the GitLab steps.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use crate::config::StepSpec;
use crate::exec::ExecRequest;
use crate::report::{RepoResult, StepResult};
use crate::step::{resolve_step_env, RunContext, StepHandler};

fn is_auth_error(stderr: &str) -> bool {
    let lower = stderr.to_lowercase();
    stderr.contains("401")
        || lower.contains("unauthorized")
        || lower.contains("authentication")
        || lower.contains("gh auth login")
}

/// Render one `gh pr list --json number,title,author,updatedAt` payload.
fn render_prs(json_text: &str) -> Result<String, String> {
    let prs: Vec<Value> =
        serde_json::from_str(json_text).map_err(|e| format!("unexpected gh output: {e}"))?;
    if prs.is_empty() {
        return Ok("(no open PRs)".to_string());
    }
    let lines: Vec<String> = prs
        .iter()
        .map(|pr| {
            let number = pr["number"].as_u64().unwrap_or(0);
            let title = pr["title"].as_str().unwrap_or("(untitled)");
            let author = pr["author"]["login"].as_str().unwrap_or("?");
            format!("#{number} {title} ({author})")
        })
        .collect();
    Ok(lines.join("\n"))
}

/// `github-pr-scan`: open PRs per configured `owner/repo` slug.
pub struct GithubPrScan;

#[async_trait]
impl StepHandler for GithubPrScan {
    fn type_name(&self) -> &'static str {
        "github-pr-scan"
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
                .map(|r| format!("[dry-run] would run: gh pr list --repo {r} --state open"))
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

        let mut repo_results = Vec::with_capacity(repos.len());
        let mut auth_failures = 0usize;
        for repo in &repos {
            let args: Vec<String> = [
                "pr",
                "list",
                "--repo",
                repo,
                "--state",
                "open",
                "--json",
                "number,title,author,updatedAt",
            ]
            .iter()
            .map(ToString::to_string)
            .collect();
            let out = match cx
                .exec
                .spawn(
                    ExecRequest::new("gh")
                        .args(args)
                        .env(env.clone())
                        .timeout(Duration::from_secs(spec.timeout)),
                )
                .await
            {
                Ok(out) => out,
                Err(e) => {
                    repo_results.push(RepoResult::err(repo, e.to_string()));
                    continue;
                }
            };
            if !out.success() {
                let err = out.stderr.trim().to_string();
                if is_auth_error(&err) {
                    auth_failures += 1;
                    repo_results.push(RepoResult {
                        repo: repo.clone(),
                        output: String::new(),
                        success: true,
                        error: Some(format!("Auth failed: {err}")),
                    });
                } else {
                    repo_results.push(RepoResult::err(
                        repo,
                        if err.is_empty() {
                            "gh error".into()
                        } else {
                            err
                        },
                    ));
                }
                continue;
            }
            match render_prs(out.stdout.trim()) {
                Ok(body) => repo_results.push(RepoResult::ok(repo, body)),
                Err(e) => repo_results.push(RepoResult::err(repo, e)),
            }
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
        StepResult::ok(&spec.name, &spec.step_type, lines.join("\n")).with_repos(repo_results)
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
        toml::from_str("name=\"prs\"\ntype=\"github-pr-scan\"").unwrap()
    }

    #[tokio::test]
    async fn renders_pr_lines_from_gh_json() {
        let (cx, spawner) = cx_with(vec![MockSpawner::ok(
            r#"[{"number":7,"title":"Add leash","author":{"login":"shawn"},"updatedAt":"x"}]"#,
        )]);
        let result = GithubPrScan.run(&spec(), &cx).await;
        assert!(result.success);
        assert!(result
            .output
            .contains("### owner/repo\n#7 Add leash (shawn)"));
        let calls = spawner.calls.lock().unwrap();
        assert!(calls[0].1.iter().any(|a| a == "--json"));
    }

    #[tokio::test]
    async fn empty_pr_list_renders_placeholder() {
        let (cx, _) = cx_with(vec![MockSpawner::ok("[]")]);
        let result = GithubPrScan.run(&spec(), &cx).await;
        assert!(result.success);
        assert!(result.output.contains("(no open PRs)"));
    }

    #[tokio::test]
    async fn auth_failure_on_all_repos_soft_skips() {
        let (cx, _) = cx_with(vec![MockSpawner::fail(
            "HTTP 401: To get started with GitHub CLI, run: gh auth login",
            1,
        )]);
        let result = GithubPrScan.run(&spec(), &cx).await;
        assert!(result.skipped);
        assert!(result.output.contains("gh auth login"));
    }

    #[tokio::test]
    async fn missing_credential_soft_skips() {
        // Regression (fresh-eyes 2026-06-05): spec.env was ignored by forge
        // steps.
        let (cx, spawner) = cx_with(vec![]);
        let spec: StepSpec = toml::from_str(
            "name=\"prs\"\ntype=\"github-pr-scan\"\n\
             env = { GH_TOKEN = { env = \"MODULEX_TEST_UNSET_XYZZY\" } }",
        )
        .unwrap();
        let result = GithubPrScan.run(&spec, &cx).await;
        assert!(result.skipped);
        assert!(result.output.contains("GH_TOKEN"));
        assert!(spawner.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn malformed_json_is_a_step_failure_not_a_crash() {
        let (cx, _) = cx_with(vec![MockSpawner::ok("not json")]);
        let result = GithubPrScan.run(&spec(), &cx).await;
        assert!(!result.success);
        assert!(result.output.contains("ERROR: unexpected gh output"));
    }
}
