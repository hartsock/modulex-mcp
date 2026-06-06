//! The `url-watch` step (feature `web`) — fetch each registered watch
//! through agent-bridle's leashed web tool, hash the extracted content, and
//! report what changed since the watch was last seen.
//!
//! The fetch path is `agent-bridle-tool-web::WebFetchTool`: every request
//! (and every redirect hop) is gated against the run's **net** Caveats axis
//! and SSRF-screened (private/loopback addresses rejected, connections
//! pinned to the screened IP). This is modulex's first use of the net axis —
//! exec and net are now both leashed.
//!
//! Change detection: BLAKE3 hash of the extracted markdown, compared with
//! the hash stored at the last seen **generation** (a counter, never a
//! clock).

use async_trait::async_trait;

use crate::config::StepSpec;
use crate::report::{RepoResult, StepResult};
use crate::step::{RunContext, StepHandler};

/// What a leashed fetch produced (the slice url-watch needs).
#[derive(Clone, Debug)]
pub struct FetchResult {
    /// HTTP status.
    pub status: u16,
    /// Extracted page title.
    pub title: String,
    /// Extracted main content as markdown.
    pub markdown: String,
}

/// The mockable fetch seam (house rule: unit tests touch no network).
#[async_trait]
pub trait Fetcher: Send + Sync {
    /// Fetch `url` under the run's leash.
    async fn fetch(
        &self,
        url: &str,
        cx: &agent_bridle_core::ToolContext,
    ) -> Result<FetchResult, String>;
}

/// Production fetcher: agent-bridle-tool-web's `WebFetchTool` (net-axis
/// leash + SSRF screen + redirect re-check + DNS-rebinding pin).
pub struct BridleFetcher;

#[async_trait]
impl Fetcher for BridleFetcher {
    async fn fetch(
        &self,
        url: &str,
        cx: &agent_bridle_core::ToolContext,
    ) -> Result<FetchResult, String> {
        use agent_bridle_core::Tool;
        let tool = agent_bridle_tool_web::WebFetchTool::new();
        let result = tool
            .invoke(serde_json::json!({ "url": url }), cx)
            .await
            .map_err(|e| e.to_string())?;
        Ok(FetchResult {
            status: u16::try_from(result["status"].as_u64().unwrap_or(0)).unwrap_or(0),
            title: result["title"].as_str().unwrap_or("").to_string(),
            markdown: result["markdown"].as_str().unwrap_or("").to_string(),
        })
    }
}

/// `url-watch`: change tracking over the store's registered URLs.
pub struct UrlWatch {
    fetcher: std::sync::Arc<dyn Fetcher>,
}

impl UrlWatch {
    /// The production step (BridleFetcher).
    #[must_use]
    pub fn new() -> Self {
        Self {
            fetcher: std::sync::Arc::new(BridleFetcher),
        }
    }

    /// A step over an injected fetcher (tests).
    #[must_use]
    pub fn with_fetcher(fetcher: std::sync::Arc<dyn Fetcher>) -> Self {
        Self { fetcher }
    }
}

impl Default for UrlWatch {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl StepHandler for UrlWatch {
    fn type_name(&self) -> &'static str {
        "url-watch"
    }

    fn description(&self) -> &'static str {
        "Change tracking over registered URLs (leashed fetch, content hashing)"
    }

    fn data_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["watches"],
            "properties": {
                "watches": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["url", "state"],
                        "properties": {
                            "url": { "type": "string" },
                            "note": { "type": "string" },
                            "state": { "type": "string",
                                       "enum": ["first", "unchanged", "changed", "error"] },
                            "title": { "type": "string" },
                            "http_status": { "type": "integer" },
                            "since_gen": { "type": "integer",
                                           "description": "generation of the previous fetch" },
                            "detail": { "type": "string", "description": "error text" }
                        }
                    }
                }
            }
        })
    }

    fn required_programs(&self, _spec: &StepSpec) -> Vec<String> {
        vec![] // in-proc fetch; the leash here is the NET axis, not exec
    }

    async fn run(&self, spec: &StepSpec, cx: &RunContext) -> StepResult {
        let Some(store) = &cx.store else {
            return StepResult::skip(&spec.name, &spec.step_type, "agent state store unavailable");
        };
        let watches = match store.watches() {
            Ok(watches) => watches,
            Err(e) => return StepResult::fail(&spec.name, &spec.step_type, e.to_string()),
        };
        if watches.is_empty() {
            return StepResult::ok(&spec.name, &spec.step_type, "No watches registered.");
        }
        if cx.dry_run {
            let listing: Vec<String> = watches
                .iter()
                .map(|w| format!("[dry-run] would fetch (leashed): {}", w.url))
                .collect();
            return StepResult::ok(&spec.name, &spec.step_type, listing.join("\n"));
        }

        let mut repo_results = Vec::with_capacity(watches.len());
        let mut data_watches = Vec::with_capacity(watches.len());
        for watch in &watches {
            let label = if watch.note.is_empty() {
                watch.url.clone()
            } else {
                format!("{} ({})", watch.url, watch.note)
            };
            match self.fetcher.fetch(&watch.url, cx.exec.tool_context()).await {
                Ok(fetched) => {
                    let hash = blake3::hash(fetched.markdown.as_bytes())
                        .to_hex()
                        .to_string();
                    let (state, since_gen, line) = match (&watch.last_hash, watch.last_seen_gen) {
                        (Some(previous), Some(seen)) if *previous == hash => (
                            "unchanged",
                            Some(seen),
                            format!("unchanged since gen {seen} — {}", fetched.title),
                        ),
                        (Some(_), Some(seen)) => (
                            "changed",
                            Some(seen),
                            format!(
                                "CHANGED since gen {seen} — {} (HTTP {})",
                                fetched.title, fetched.status
                            ),
                        ),
                        _ => (
                            "first",
                            None,
                            format!("first fetch — {} (HTTP {})", fetched.title, fetched.status),
                        ),
                    };
                    data_watches.push(serde_json::json!({
                        "url": watch.url, "note": watch.note, "state": state,
                        "title": fetched.title, "http_status": fetched.status,
                        "since_gen": since_gen,
                    }));
                    if let Err(e) = store.watch_seen(watch.id, &hash, cx.generation) {
                        repo_results.push(RepoResult::err(&label, e.to_string()));
                    } else {
                        repo_results.push(RepoResult::ok(&label, line));
                    }
                }
                // A denied or failed fetch is data, not a dead routine — the
                // denial reason (net leash, SSRF screen) lands in the report.
                Err(e) => {
                    data_watches.push(serde_json::json!({
                        "url": watch.url, "note": watch.note, "state": "error",
                        "detail": e,
                    }));
                    repo_results.push(RepoResult::err(&label, e));
                }
            }
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
        result.data = Some(serde_json::json!({ "watches": data_watches }));
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
    use crate::store::Store;

    struct CannedFetcher(Vec<Result<FetchResult, String>>);

    #[async_trait]
    impl Fetcher for CannedFetcher {
        async fn fetch(
            &self,
            _url: &str,
            _cx: &agent_bridle_core::ToolContext,
        ) -> Result<FetchResult, String> {
            self.0.first().cloned().unwrap_or_else(|| {
                Ok(FetchResult {
                    status: 200,
                    title: "t".into(),
                    markdown: "m".into(),
                })
            })
        }
    }

    fn cx_with(store: Arc<Store>) -> RunContext {
        RunContext {
            config: Arc::new(Config::default()),
            dry_run: false,
            generation: 7,
            exec: gate_with(&Caveats::top(), Arc::new(MockSpawner::default())),
            prior: Vec::new(),
            store: Some(store),
        }
    }

    fn spec() -> StepSpec {
        toml::from_str("name=\"watches\"\ntype=\"url-watch\"").unwrap()
    }

    fn fetched(markdown: &str) -> Result<FetchResult, String> {
        Ok(FetchResult {
            status: 200,
            title: "Release notes".into(),
            markdown: markdown.into(),
        })
    }

    #[tokio::test]
    async fn first_fetch_then_unchanged_then_changed() {
        let store = Arc::new(Store::in_memory().unwrap());
        store
            .watch_add("https://example.com/r", "releases", 1)
            .unwrap();

        // First fetch.
        let step = UrlWatch::with_fetcher(Arc::new(CannedFetcher(vec![fetched("v1")])));
        let result = step.run(&spec(), &cx_with(store.clone())).await;
        assert!(result.success);
        assert!(result.output.contains("first fetch"));

        // Same content → unchanged since gen 7.
        let step = UrlWatch::with_fetcher(Arc::new(CannedFetcher(vec![fetched("v1")])));
        let result = step.run(&spec(), &cx_with(store.clone())).await;
        assert!(result.output.contains("unchanged since gen 7"));

        // New content → CHANGED.
        let step = UrlWatch::with_fetcher(Arc::new(CannedFetcher(vec![fetched("v2")])));
        let result = step.run(&spec(), &cx_with(store.clone())).await;
        assert!(result.output.contains("CHANGED since gen 7"));
    }

    #[tokio::test]
    async fn fetch_denial_is_step_data_not_routine_death() {
        let store = Arc::new(Store::in_memory().unwrap());
        store.watch_add("https://blocked.example", "", 1).unwrap();
        let step = UrlWatch::with_fetcher(Arc::new(CannedFetcher(vec![Err(
            "network access to \"blocked.example\" is not within the granted authority".into(),
        )])));
        let result = step.run(&spec(), &cx_with(store)).await;
        assert!(!result.success, "denied fetch fails the step");
        assert!(result.output.contains("granted authority"));
    }

    #[tokio::test]
    async fn empty_watches_and_dry_run() {
        let store = Arc::new(Store::in_memory().unwrap());
        let step = UrlWatch::with_fetcher(Arc::new(CannedFetcher(vec![])));
        let result = step.run(&spec(), &cx_with(store.clone())).await;
        assert_eq!(result.output, "No watches registered.");

        store.watch_add("https://example.com", "", 1).unwrap();
        let mut cx = cx_with(store);
        cx.dry_run = true;
        let step = UrlWatch::with_fetcher(Arc::new(CannedFetcher(vec![])));
        let result = step.run(&spec(), &cx).await;
        assert!(result.output.contains("[dry-run] would fetch (leashed)"));
    }

    #[tokio::test]
    async fn real_net_leash_denies_unlisted_host_through_bridle_fetcher() {
        // The REAL BridleFetcher against a deny-all net grant: the leash
        // rejects before any network I/O happens, so this is still a
        // no-network unit test.
        let store = Arc::new(Store::in_memory().unwrap());
        store.watch_add("https://example.com", "", 1).unwrap();
        let granted = Caveats {
            net: Scope::none(),
            ..Caveats::top()
        };
        let cx = RunContext {
            config: Arc::new(Config::default()),
            dry_run: false,
            generation: 1,
            exec: gate_with(&granted, Arc::new(MockSpawner::default())),
            prior: Vec::new(),
            store: Some(store),
        };
        let result = UrlWatch::new().run(&spec(), &cx).await;
        assert!(!result.success);
        assert!(
            result.output.contains("not within the granted authority"),
            "got: {}",
            result.output
        );
    }
}
