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

/// Shared schema for the glab-backed steps: per-target raw passthrough with
/// a state enum (glab output is human text; `raw` carries it verbatim).
fn targets_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "required": ["targets"],
        "properties": {
            "targets": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["target", "state", "raw"],
                    "properties": {
                        "target": { "type": "string" },
                        "state": { "type": "string",
                                   "enum": ["ok", "none", "auth-failed", "error"] },
                        "raw": { "type": "string",
                                 "description": "verbatim CLI output (ok) or error text" }
                    }
                }
            }
        }
    })
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

    let targets: Vec<serde_json::Value> = repo_results
        .iter()
        .map(|rr| {
            let (state, raw) = match &rr.error {
                Some(e) if e.contains(AUTH_FAILED) => ("auth-failed", e.clone()),
                Some(e) => ("error", e.clone()),
                None if rr.output == "(none)" => ("none", String::new()),
                None => ("ok", rr.output.clone()),
            };
            serde_json::json!({ "target": rr.repo, "state": state, "raw": raw })
        })
        .collect();
    let mut result = StepResult::ok(&spec.name, &spec.step_type, body).with_repos(repo_results);
    result.data = Some(serde_json::json!({ "targets": targets }));
    result
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

    fn description(&self) -> &'static str {
        "Open merge requests the user authored, per project"
    }

    fn data_schema(&self) -> serde_json::Value {
        targets_schema()
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

    fn description(&self) -> &'static str {
        "Open merge requests where the user is a reviewer, per project"
    }

    fn data_schema(&self) -> serde_json::Value {
        targets_schema()
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

    fn description(&self) -> &'static str {
        "Recent merge-request activity across configured groups"
    }

    fn data_schema(&self) -> serde_json::Value {
        targets_schema()
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

    fn description(&self) -> &'static str {
        "Pending review-request count from this run, against an SLA threshold"
    }

    fn data_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["threshold_hours", "pending_targets"],
            "properties": {
                "threshold_hours": { "type": "integer" },
                "pending_targets": { "type": "integer", "minimum": 0 }
            }
        })
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
        let mut result = StepResult::ok(&spec.name, &spec.step_type, body);
        result.data = Some(serde_json::json!({
            "threshold_hours": hours, "pending_targets": pending,
        }));
        result
    }
}

// ── mr-categorize: HIGH / ACTIVE / SEEN enrichment ─────────────────────

/// Buckets for enriched MRs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MrCategory {
    /// Your MR with new activity from others — act on it.
    High,
    /// Needs attention / uncategorized.
    Active,
    /// You reacted or commented and nothing changed since.
    Seen,
}

impl MrCategory {
    fn key(self) -> &'static str {
        match self {
            MrCategory::High => "high",
            MrCategory::Active => "active",
            MrCategory::Seen => "seen",
        }
    }
}

/// Data-contract schema for `mr-categorize`.
fn mr_categorize_schema() -> serde_json::Value {
    let item = serde_json::json!({
        "type": "object",
        "required": ["project", "iid", "category"],
        "properties": {
            "project": { "type": "string" },
            "iid": { "type": "integer" },
            "title": { "type": "string" },
            "author": { "type": "string" },
            "web_url": { "type": "string" },
            "category": { "type": "string", "enum": ["high", "active", "seen"] },
            "reason": { "type": "string" }
        }
    });
    serde_json::json!({
        "type": "object",
        "required": ["high", "active", "seen"],
        "properties": {
            "high": { "type": "array", "items": item },
            "active": { "type": "array", "items": item },
            "seen": { "type": "array", "items": item }
        }
    })
}

/// Parse a glab `mr list` line: `!IID\tPROJECT!IID\tTITLE\t(target) ← (source)`.
/// Returns `(project, iid, title)` for each MR line; tolerant of trailing fields.
fn parse_mr_lines(output: &str) -> Vec<(String, i64, String)> {
    let mut out = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if !line.starts_with('!') {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 2 {
            continue;
        }
        let Some((project, iid_str)) = parts[1].rsplit_once('!') else {
            continue;
        };
        let Ok(iid) = iid_str.parse::<i64>() else {
            continue;
        };
        let title = parts.get(2).copied().unwrap_or("").trim().to_string();
        out.push((project.to_string(), iid, title));
    }
    out
}

/// Percent-encode a project path for a glab-api endpoint (`group/repo` →
/// `group%2Frepo`).
fn encode_project(project: &str) -> String {
    project.replace('/', "%2F")
}

/// Parse an ISO-8601 timestamp to epoch seconds for ORDERING only (display
/// math; never a coordination primitive).
fn parse_ts(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp())
}

fn max_opt(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (x, None) => x,
        (None, y) => y,
    }
}

/// One `glab api <endpoint>` GET; parsed JSON, or `None` on any failure.
///
/// FIXTURE-SYNC: the JSON shapes consumed by [`categorize`] mirror the GitLab
/// REST API (merge_requests detail `author.username`/`title`/`web_url`,
/// `award_emoji` `[{name, user.username, created_at}]`, `notes`
/// `[{author.username, created_at, system}]`). The `glab` presence/auth belief
/// is exercised by tests/live_contract.rs::live_glab_presence.
async fn glab_api(
    exec: &ExecGate,
    env: &[(String, Secret)],
    endpoint: &str,
    timeout: u64,
) -> Option<serde_json::Value> {
    let out = exec
        .spawn(
            ExecRequest::new("glab")
                .args(vec!["api".to_string(), endpoint.to_string()])
                .env(env.to_vec())
                .timeout(Duration::from_secs(timeout)),
        )
        .await
        .ok()?;
    if !out.success() {
        return None;
    }
    serde_json::from_str(&out.stdout).ok()
}

/// Apply the categorization rules to one MR's detail + reactions + comments.
fn categorize(
    mr: &serde_json::Value,
    emoji: &serde_json::Value,
    notes: &serde_json::Value,
    username: &str,
    seen_emoji: &str,
) -> (MrCategory, String) {
    let username_of = |v: &serde_json::Value, key: &str| -> String {
        v.get(key)
            .and_then(|u| u.get("username"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let mr_author = username_of(mr, "author");

    // The user's interaction time: latest of their eyes-reaction or comment.
    let mut user_interaction: Option<i64> = None;
    if let Some(items) = emoji.as_array() {
        for e in items {
            if username_of(e, "user") == username
                && e.get("name").and_then(serde_json::Value::as_str) == Some(seen_emoji)
            {
                let ts = e
                    .get("created_at")
                    .and_then(serde_json::Value::as_str)
                    .and_then(parse_ts);
                user_interaction = max_opt(user_interaction, ts);
            }
        }
    }
    if let Some(items) = notes.as_array() {
        for n in items {
            if n.get("system").and_then(serde_json::Value::as_bool) == Some(true) {
                continue;
            }
            if username_of(n, "author") == username {
                let ts = n
                    .get("created_at")
                    .and_then(serde_json::Value::as_str)
                    .and_then(parse_ts);
                user_interaction = max_opt(user_interaction, ts);
            }
        }
    }

    // Latest activity from anyone else.
    let mut latest_other: Option<i64> = None;
    if let Some(items) = notes.as_array() {
        for n in items {
            if n.get("system").and_then(serde_json::Value::as_bool) == Some(true) {
                continue;
            }
            if username_of(n, "author") != username {
                let ts = n
                    .get("created_at")
                    .and_then(serde_json::Value::as_str)
                    .and_then(parse_ts);
                latest_other = max_opt(latest_other, ts);
            }
        }
    }
    if let Some(items) = emoji.as_array() {
        for e in items {
            if username_of(e, "user") != username {
                let ts = e
                    .get("created_at")
                    .and_then(serde_json::Value::as_str)
                    .and_then(parse_ts);
                latest_other = max_opt(latest_other, ts);
            }
        }
    }

    // Rule HIGH: your MR with activity from others (unless you replied last).
    if mr_author == username {
        if let Some(other) = latest_other {
            if user_interaction.is_some_and(|ui| ui >= other) {
                return (
                    MrCategory::Seen,
                    "you commented/reacted after latest activity".into(),
                );
            }
            return (MrCategory::High, "new activity on your MR".into());
        }
    }
    // Rule SEEN: you interacted and nothing newer from others.
    if let Some(ui) = user_interaction {
        if latest_other.is_none_or(|o| o <= ui) {
            return (
                MrCategory::Seen,
                "no changes since your last interaction".into(),
            );
        }
    }
    (MrCategory::Active, String::new())
}

/// Append a rendered section for a bucket, if non-empty.
fn push_section(lines: &mut Vec<String>, title: &str, items: &[serde_json::Value]) {
    if items.is_empty() {
        return;
    }
    lines.push(format!("### {title}"));
    for it in items {
        let iid = it
            .get("iid")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        let project = it
            .get("project")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let title = it
            .get("title")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let reason = it
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let suffix = if reason.is_empty() {
            String::new()
        } else {
            format!(" — {reason}")
        };
        lines.push(format!("  !{iid} {project} — {title}{suffix}"));
    }
}

fn render_categorized(
    high: &[serde_json::Value],
    active: &[serde_json::Value],
    seen: &[serde_json::Value],
) -> String {
    if high.is_empty() && active.is_empty() && seen.is_empty() {
        return "(no MRs to categorize)".to_string();
    }
    let mut lines = Vec::new();
    push_section(&mut lines, "HIGH (needs your attention)", high);
    push_section(&mut lines, "Active", active);
    push_section(&mut lines, "Seen (no changes)", seen);
    lines.join("\n")
}

/// `mr-categorize`: enrich authored (or reviewer) MRs into HIGH / ACTIVE /
/// SEEN using per-MR reactions and comments. Params: `role` (`author`
/// default | `reviewer`), `seen_emoji` (default `eyes`).
pub struct MrCategorize;

#[async_trait]
impl StepHandler for MrCategorize {
    fn type_name(&self) -> &'static str {
        "mr-categorize"
    }

    fn description(&self) -> &'static str {
        "Authored/review MRs bucketed High / Active / Seen via reactions and comments"
    }

    fn data_schema(&self) -> serde_json::Value {
        mr_categorize_schema()
    }

    fn required_programs(&self, _spec: &StepSpec) -> Vec<String> {
        vec!["glab".into()]
    }

    async fn run(&self, spec: &StepSpec, cx: &RunContext) -> StepResult {
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
        let username = cx.config.identity.username.clone();
        if username.is_empty() {
            return StepResult::skip(
                &spec.name,
                &spec.step_type,
                "No username configured in identity section.",
            );
        }
        let flag = match spec.param_str("role") {
            Some("reviewer") => "--reviewer",
            _ => "--author",
        };
        let seen_emoji = spec.param_str("seen_emoji").unwrap_or("eyes").to_string();

        if cx.dry_run {
            return StepResult::ok(
                &spec.name,
                &spec.step_type,
                format!(
                    "[dry-run] would categorize {flag}={username} MRs across {} project(s)",
                    projects.len()
                ),
            );
        }

        let env = match step_env_or_skip(spec, cx).await {
            Ok(env) => env,
            Err(skip) => return skip,
        };

        // 1. List MRs per project.
        let mut list_results = Vec::with_capacity(projects.len());
        for project in &projects {
            let args = vec![
                "mr".to_string(),
                "list".to_string(),
                format!("{flag}={username}"),
                "-R".to_string(),
                project.clone(),
            ];
            list_results.push(run_glab(&cx.exec, project, args, env.clone(), spec.timeout).await);
        }
        let auth_failures = list_results
            .iter()
            .filter(|rr| rr.error.as_deref().is_some_and(|e| e.contains(AUTH_FAILED)))
            .count();
        if auth_failures > 0 && auth_failures == list_results.len() {
            return StepResult::skip(
                &spec.name,
                &spec.step_type,
                format!(
                    "All GitLab queries failed — token likely expired. Run: glab auth login --hostname {}",
                    cx.config.identity.gitlab_host
                ),
            );
        }

        // 2. Enrich + categorize each MR.
        let mut high = Vec::new();
        let mut active = Vec::new();
        let mut seen = Vec::new();
        for rr in &list_results {
            if rr.error.is_some() {
                continue;
            }
            for (project, iid, line_title) in parse_mr_lines(&rr.output) {
                let enc = encode_project(&project);
                let mr = glab_api(
                    &cx.exec,
                    &env,
                    &format!("projects/{enc}/merge_requests/{iid}"),
                    spec.timeout,
                )
                .await;
                let emoji = glab_api(
                    &cx.exec,
                    &env,
                    &format!("projects/{enc}/merge_requests/{iid}/award_emoji"),
                    spec.timeout,
                )
                .await
                .unwrap_or_else(|| serde_json::json!([]));
                let notes = glab_api(
                    &cx.exec,
                    &env,
                    &format!("projects/{enc}/merge_requests/{iid}/notes?sort=desc&per_page=20"),
                    spec.timeout,
                )
                .await
                .unwrap_or_else(|| serde_json::json!([]));

                let (category, reason, author, web_url, title) = match &mr {
                    Some(mr) => {
                        let (category, reason) =
                            categorize(mr, &emoji, &notes, &username, &seen_emoji);
                        let str_of = |k: &str| {
                            mr.get(k)
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("")
                                .to_string()
                        };
                        let author = mr
                            .get("author")
                            .and_then(|a| a.get("username"))
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let title = {
                            let t = str_of("title");
                            if t.is_empty() {
                                line_title.clone()
                            } else {
                                t
                            }
                        };
                        (category, reason, author, str_of("web_url"), title)
                    }
                    None => (
                        MrCategory::Active,
                        "(detail unavailable)".to_string(),
                        String::new(),
                        String::new(),
                        line_title.clone(),
                    ),
                };

                let item = serde_json::json!({
                    "project": project,
                    "iid": iid,
                    "title": title,
                    "author": author,
                    "web_url": web_url,
                    "category": category.key(),
                    "reason": reason,
                });
                match category {
                    MrCategory::High => high.push(item),
                    MrCategory::Active => active.push(item),
                    MrCategory::Seen => seen.push(item),
                }
            }
        }

        let mut result = StepResult::ok(
            &spec.name,
            &spec.step_type,
            render_categorized(&high, &active, &seen),
        );
        result.data = Some(serde_json::json!({ "high": high, "active": active, "seen": seen }));
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
                store: None,
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

    // ── mr-categorize ──────────────────────────────────────────────────

    #[test]
    fn parse_mr_lines_extracts_project_iid_title() {
        let out = "!5\tgroup/a!5\tFix the thing\t(main) ← (fix)\n\
                   not an mr line\n\
                   !12\tgroup/b!12\tAnother\t(main) ← (wip)";
        let parsed = parse_mr_lines(out);
        assert_eq!(parsed.len(), 2);
        assert_eq!(
            parsed[0],
            ("group/a".to_string(), 5, "Fix the thing".to_string())
        );
        assert_eq!(parsed[1].0, "group/b");
        assert_eq!(parsed[1].1, 12);
    }

    #[test]
    fn categorize_high_active_seen_rules() {
        let mr_yours = serde_json::json!({ "author": { "username": "someone" } });
        let mr_theirs = serde_json::json!({ "author": { "username": "other" } });
        let no_emoji = serde_json::json!([]);
        let no_notes = serde_json::json!([]);

        // HIGH: your MR, others active, you haven't replied.
        let others = serde_json::json!([
            { "author": { "username": "other" }, "system": false, "created_at": "2026-06-09T00:00:00Z" }
        ]);
        let (cat, reason) = categorize(&mr_yours, &no_emoji, &others, "someone", "eyes");
        assert_eq!(cat, MrCategory::High);
        assert_eq!(reason, "new activity on your MR");

        // SEEN: your MR, but you reacted/commented after the latest activity.
        let you_later = serde_json::json!([
            { "author": { "username": "other" }, "system": false, "created_at": "2026-06-09T00:00:00Z" },
            { "author": { "username": "someone" }, "system": false, "created_at": "2026-06-10T00:00:00Z" }
        ]);
        let (cat, _) = categorize(&mr_yours, &no_emoji, &you_later, "someone", "eyes");
        assert_eq!(cat, MrCategory::Seen);

        // SEEN via eyes reaction, nothing newer from others.
        let your_eyes = serde_json::json!([
            { "user": { "username": "someone" }, "name": "eyes", "created_at": "2026-06-11T00:00:00Z" }
        ]);
        let (cat, _) = categorize(&mr_theirs, &your_eyes, &no_notes, "someone", "eyes");
        assert_eq!(cat, MrCategory::Seen);

        // ACTIVE: someone else's MR, you never interacted.
        let (cat, _) = categorize(&mr_theirs, &no_emoji, &no_notes, "someone", "eyes");
        assert_eq!(cat, MrCategory::Active);
    }

    #[tokio::test]
    async fn mr_categorize_buckets_high_end_to_end() {
        // list (group/a) -> mr detail -> award_emoji -> notes
        let list = "!5\tgroup/a!5\tFix the thing\t(main) ← (fix)";
        let mr = r#"{"author":{"username":"someone"},"title":"Fix the thing","web_url":"https://gl/mr/5"}"#;
        let emoji = "[]";
        let notes = r#"[{"author":{"username":"other"},"system":false,"created_at":"2026-06-09T00:00:00Z"}]"#;
        let (cx, spawner) = cx_with(
            GL_CONFIG,
            vec![
                MockSpawner::ok(list),
                MockSpawner::ok(mr),
                MockSpawner::ok(emoji),
                MockSpawner::ok(notes),
            ],
        );
        // Single project so only one list call is consumed.
        let spec: StepSpec =
            toml::from_str("name=\"t\"\ntype=\"mr-categorize\"\nrepos=[\"group/a\"]").unwrap();
        let result = MrCategorize.run(&spec, &cx).await;
        assert!(result.success);
        assert!(result.output.contains("HIGH"));
        let data = result.data.unwrap();
        assert_eq!(data["high"].as_array().unwrap().len(), 1);
        assert_eq!(data["high"][0]["category"], "high");
        assert_eq!(data["high"][0]["web_url"], "https://gl/mr/5");

        // Verify it queried the right glab-api endpoints.
        let calls = spawner.calls.lock().unwrap();
        assert_eq!(
            calls[0].1,
            vec!["mr", "list", "--author=someone", "-R", "group/a"]
        );
        assert_eq!(
            calls[1].1,
            vec!["api", "projects/group%2Fa/merge_requests/5"]
        );
        assert!(calls[2].1[1].contains("award_emoji"));
        assert!(calls[3].1[1].contains("/notes?sort=desc&per_page=20"));
    }

    #[tokio::test]
    async fn mr_categorize_all_auth_failures_soft_skip() {
        let (cx, _) = cx_with(
            GL_CONFIG,
            vec![
                MockSpawner::fail("401 unauthorized", 1),
                MockSpawner::fail("forbidden", 1),
            ],
        );
        let result = MrCategorize.run(&spec("mr-categorize"), &cx).await;
        assert!(result.skipped);
        assert!(result.output.contains("glab auth login"));
    }
}
