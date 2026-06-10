//! Board ingestion step via the `gh` CLI: `board-ingest`.
//!
//! Pulls open GitHub issues into the store-backed knowledge board as cards —
//! the successor to the old "scan issues into board cards" workflow. Each
//! issue becomes a card with a STABLE `card_id` (`gh-<owner>-<repo>-<number>`),
//! so re-running upserts instead of duplicating. This step WRITES to the store
//! during a routine, like `url-watch`. Auth failures soft-skip with a
//! `gh auth login` hint, mirroring `github-pr-scan`.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use crate::config::StepSpec;
use crate::credentials::Secret;
use crate::exec::{ExecGate, ExecRequest};
use crate::report::{RepoResult, StepResult};
use crate::step::{resolve_step_env, RunContext, StepHandler};
use crate::store::{CardInput, CardRef};

fn is_auth_error(stderr: &str) -> bool {
    let lower = stderr.to_lowercase();
    stderr.contains("401")
        || lower.contains("unauthorized")
        || lower.contains("authentication")
        || lower.contains("gh auth login")
}

/// Outcome of the `gh issue list` query for one repo.
enum GhOutcome {
    Ok(Vec<Value>),
    Auth(String),
    Err(String),
}

async fn gh_issues(
    exec: &ExecGate,
    env: &[(String, Secret)],
    repo: &str,
    limit: i64,
    timeout: u64,
) -> GhOutcome {
    let limit = limit.to_string();
    let args = [
        "issue",
        "list",
        "-R",
        repo,
        "--state",
        "open",
        "--json",
        "number,title,url,labels",
        "--limit",
        &limit,
    ];
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

/// Does the issue carry a label whose name equals `target` (case-insensitive)?
fn has_label(labels: &[Value], target: &str) -> bool {
    labels.iter().any(|l| {
        l.get("name")
            .and_then(Value::as_str)
            .is_some_and(|n| n.eq_ignore_ascii_case(target))
    })
}

/// Map an issue's labels to a lane: highest priority label present, else the
/// configured default (falling back to `p2`).
fn lane_for(labels: &[Value], default_lane: &str) -> String {
    for lane in ["p0", "p1", "p2"] {
        if has_label(labels, lane) {
            return lane.to_string();
        }
    }
    if default_lane.is_empty() {
        "p2".to_string()
    } else {
        default_lane.to_string()
    }
}

/// `gh-<owner>-<repo>-<number>` — stable across runs so re-ingest upserts.
fn card_id_for(repo: &str, number: i64) -> String {
    format!("gh-{}-{number}", repo.replace('/', "-"))
}

/// Build the [`CardInput`] for one issue.
fn card_input(repo: &str, issue: &Value, default_lane: &str) -> Option<CardInput> {
    let number = issue.get("number").and_then(Value::as_i64)?;
    let title = issue
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let url = issue
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let labels: Vec<Value> = issue
        .get("labels")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut refs = Vec::new();
    if !url.is_empty() {
        refs.push(CardRef {
            kind: "ref".into(),
            label: "issue".into(),
            value: url.clone(),
            ordinal: 0,
        });
    }

    Some(CardInput {
        card_id: card_id_for(repo, number),
        project: repo.to_string(),
        lane: lane_for(&labels, default_lane),
        context: String::new(),
        summary: title,
        size: None,
        status: has_label(&labels, "blocked").then(|| "blocked".to_string()),
        recurs: None,
        expires: None,
        created: None,
        updated: None,
        body: String::new(),
        author: None,
        source: Some("github".to_string()),
        source_id: Some(url),
        refs,
    })
}

/// `board-ingest`: upsert open GitHub issues into the board as cards.
pub struct BoardIngest;

#[async_trait]
impl StepHandler for BoardIngest {
    fn type_name(&self) -> &'static str {
        "board-ingest"
    }

    fn description(&self) -> &'static str {
        "Pull open GitHub issues into the board as cards (idempotent upsert by card_id)"
    }

    fn data_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["repos"],
            "properties": {
                "repos": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["repo", "state", "ingested"],
                        "properties": {
                            "repo": { "type": "string" },
                            "state": { "type": "string",
                                       "enum": ["ok", "auth-failed", "error"] },
                            "ingested": { "type": "integer", "minimum": 0 },
                            "card_ids": { "type": "array", "items": { "type": "string" } },
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
        let Some(store) = &cx.store else {
            return StepResult::skip(&spec.name, &spec.step_type, "agent state store unavailable");
        };

        if cx.dry_run {
            let listing: Vec<String> = repos
                .iter()
                .map(|r| format!("[dry-run] would ingest open issues from {r} into the board"))
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
        let limit = spec.param_int("limit").unwrap_or(100);
        let default_lane = cx.config.board.default_lane.clone();

        let mut data_repos = Vec::with_capacity(repos.len());
        let mut repo_results = Vec::with_capacity(repos.len());
        let mut auth_failures = 0usize;

        for repo in &repos {
            match gh_issues(&cx.exec, &env, repo, limit, spec.timeout).await {
                GhOutcome::Auth(detail) => {
                    auth_failures += 1;
                    data_repos.push(serde_json::json!({
                        "repo": repo, "state": "auth-failed", "ingested": 0, "detail": detail,
                    }));
                    repo_results.push(RepoResult {
                        repo: repo.clone(),
                        output: String::new(),
                        success: true,
                        error: Some("Auth failed".to_string()),
                    });
                }
                GhOutcome::Err(detail) => {
                    data_repos.push(serde_json::json!({
                        "repo": repo, "state": "error", "ingested": 0, "detail": detail,
                    }));
                    repo_results.push(RepoResult::err(repo, detail));
                }
                GhOutcome::Ok(issues) => {
                    let mut card_ids = Vec::new();
                    let mut failed: Option<String> = None;
                    for issue in &issues {
                        if let Some(input) = card_input(repo, issue, &default_lane) {
                            match store.card_add(&input, cx.generation) {
                                Ok(_) => card_ids.push(input.card_id),
                                Err(e) => {
                                    failed = Some(e.to_string());
                                    break;
                                }
                            }
                        }
                    }
                    if let Some(detail) = failed {
                        data_repos.push(serde_json::json!({
                            "repo": repo, "state": "error", "ingested": card_ids.len(),
                            "card_ids": card_ids, "detail": detail,
                        }));
                        repo_results.push(RepoResult::err(repo, detail));
                    } else {
                        let n = card_ids.len();
                        data_repos.push(serde_json::json!({
                            "repo": repo, "state": "ok", "ingested": n, "card_ids": card_ids,
                        }));
                        repo_results.push(RepoResult::ok(repo, format!("ingested {n} card(s)")));
                    }
                }
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
        let mut result =
            StepResult::ok(&spec.name, &spec.step_type, lines.join("\n")).with_repos(repo_results);
        result.data = Some(serde_json::json!({ "repos": data_repos }));
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
    use crate::store::Store;

    fn cx_with(store: Option<Arc<Store>>, outputs: Vec<ExecOutput>) -> RunContext {
        let spawner = Arc::new(MockSpawner::with_outputs(outputs));
        let granted = Caveats {
            exec: Scope::only(["gh".to_string()]),
            ..Caveats::top()
        };
        let mut config = Config::default();
        config.shared.github_repos = vec!["owner/repo".into()];
        RunContext {
            config: Arc::new(config),
            dry_run: false,
            generation: 1,
            exec: gate_with(&granted, spawner),
            prior: Vec::new(),
            store,
        }
    }

    fn spec() -> StepSpec {
        toml::from_str("name=\"ingest\"\ntype=\"board-ingest\"").unwrap()
    }

    #[test]
    fn lane_for_picks_highest_priority_then_default() {
        let p1 = vec![serde_json::json!({"name": "P1"})];
        assert_eq!(lane_for(&p1, "p2"), "p1", "case-insensitive label match");
        let both = vec![
            serde_json::json!({"name": "p2"}),
            serde_json::json!({"name": "p0"}),
        ];
        assert_eq!(lane_for(&both, "p2"), "p0", "p0 outranks p2");
        assert_eq!(lane_for(&[], ""), "p2", "fallback default");
        assert_eq!(lane_for(&[], "p1"), "p1", "configured default");
    }

    #[tokio::test]
    async fn no_store_soft_skips() {
        let result = BoardIngest.run(&spec(), &cx_with(None, vec![])).await;
        assert!(result.skipped);
    }

    #[tokio::test]
    async fn auth_failure_soft_skips() {
        let cx = cx_with(
            Some(Arc::new(Store::in_memory().unwrap())),
            vec![MockSpawner::fail("HTTP 401: run: gh auth login", 1)],
        );
        let result = BoardIngest.run(&spec(), &cx).await;
        assert!(result.skipped);
        assert!(result.output.contains("gh auth login"));
    }

    // FIXTURE-SYNC: mimics `gh issue list --json number,title,url,labels`;
    // the gh JSON-list shape is exercised by live_contract::live_gh_pr_list_json_shape.
    #[tokio::test]
    async fn ingests_issues_as_cards_idempotently() {
        let issues = r#"[
            {"number":12,"title":"Fix the leak","url":"https://gh/issues/12","labels":[{"name":"p0"}]},
            {"number":20,"title":"Stuck on review","url":"https://gh/issues/20","labels":[{"name":"blocked"},{"name":"p1"}]},
            {"number":30,"title":"Someday","url":"https://gh/issues/30","labels":[]}
        ]"#;
        let store = Arc::new(Store::in_memory().unwrap());
        // Two identical list outputs so we can run twice (idempotency check).
        let cx = cx_with(
            Some(store.clone()),
            vec![MockSpawner::ok(issues), MockSpawner::ok(issues)],
        );

        let result = BoardIngest.run(&spec(), &cx).await;
        assert!(result.success);
        let data = result.data.unwrap();
        assert_eq!(data["repos"][0]["ingested"], 3);

        let cards = store.cards_query(None, None, None).unwrap();
        assert_eq!(cards.len(), 3);
        let p0 = store.card_by_card_id("gh-owner-repo-12").unwrap().unwrap();
        assert_eq!(p0.lane, "p0");
        assert_eq!(p0.summary, "Fix the leak");
        assert_eq!(p0.source.as_deref(), Some("github"));
        assert_eq!(p0.refs.len(), 1);
        let blocked = store.card_by_card_id("gh-owner-repo-20").unwrap().unwrap();
        assert_eq!(blocked.lane, "p1", "p1 label sets lane");
        assert_eq!(blocked.status.as_deref(), Some("blocked"));
        let default = store.card_by_card_id("gh-owner-repo-30").unwrap().unwrap();
        assert_eq!(default.lane, "p2", "no priority label → default");

        // Second run upserts — still 3 cards, not 6.
        let result2 = BoardIngest.run(&spec(), &cx).await;
        assert!(result2.success);
        assert_eq!(store.cards_query(None, None, None).unwrap().len(), 3);
    }
}
