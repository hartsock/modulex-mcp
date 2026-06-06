//! The JSON-RPC 2.0 stdio loop (MCP protocol `2024-11-05`).
//!
//! Pattern: newline-delimited requests on stdin, one response line per
//! request with an id, notifications get none. Dispatch is serial — MCP
//! stdio is request/response, and routine runs are the long pole anyway.

use std::sync::Arc;

use modulex_core::Engine;
use serde_json::{json, Value};

use crate::facets::FacetPolicy;
use crate::tools;

/// MCP protocol revision this server implements.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// The MCP server: an engine, the facet exposure policy, and the protocol
/// shell.
pub struct Server {
    engine: Arc<Engine>,
    policy: FacetPolicy,
    version: &'static str,
}

impl Server {
    /// A server over an engine. The facet policy resolves from
    /// `$MODULEX_TOOLS` → the engine config's `[mcp]` → the default index.
    #[must_use]
    pub fn new(engine: Engine) -> Self {
        Self::with_engine(Arc::new(engine))
    }

    /// A server sharing an engine with another surface (the Python bindings
    /// run routines and serve MCP over the SAME engine, so generations and
    /// stored reports stay continuous).
    #[must_use]
    pub fn with_engine(engine: Arc<Engine>) -> Self {
        let policy = FacetPolicy::load(&engine.config().mcp);
        Self::with_policy(engine, policy)
    }

    /// A server with an explicit facet policy (tests; embedders).
    #[must_use]
    pub fn with_policy(engine: Arc<Engine>, policy: FacetPolicy) -> Self {
        Self {
            engine,
            policy,
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    /// The resolved facet policy (banner, introspection).
    #[must_use]
    pub fn policy(&self) -> &FacetPolicy {
        &self.policy
    }

    /// Borrow the engine (the CLI `--probe` path uses this).
    #[must_use]
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Handle one JSON-RPC request. `None` means "no response" (notification
    /// or unparsable id-less input).
    pub async fn handle(&self, req: &Value) -> Option<Value> {
        let id = req.get("id").cloned();
        let Some(method) = req.get("method").and_then(Value::as_str) else {
            // A request with an id but no method must get an answer —
            // silence would leave the client hanging until its timeout.
            return id.map(|id| {
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32600, "message": "invalid request: missing method" }
                })
            });
        };

        let result: Value = match method {
            "initialize" => json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "modulex-mcp", "version": self.version }
            }),
            // Notification: acknowledged by silence.
            "notifications/initialized" => return None,
            "ping" => json!({}),
            "tools/list" => json!({ "tools": tools::registry().specs_json(&self.policy) }),
            "tools/call" => {
                let params = req.get("params").cloned().unwrap_or(Value::Null);
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let outcome = tools::registry()
                    .call(&self.engine, &self.policy, name, &args)
                    .await;
                let mut result = json!({
                    "content": [{ "type": "text", "text": outcome.text }]
                });
                if outcome.is_error {
                    result["isError"] = json!(true);
                }
                result
            }
            other => {
                return id.map(|id| {
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32601, "message": format!("method not found: {other}") }
                    })
                });
            }
        };

        id.map(|id| json!({ "jsonrpc": "2.0", "id": id, "result": result }))
    }

    /// Run the stdio loop until EOF. Lines that are not valid JSON get a
    /// parse-error response; everything else flows through [`Self::handle`].
    ///
    /// # Errors
    /// Only on stdin/stdout I/O failure — protocol-level problems are
    /// answered in-band.
    pub async fn run_stdio(&self) -> anyhow::Result<()> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let stdin = BufReader::new(tokio::io::stdin());
        let mut stdout = tokio::io::stdout();
        let mut lines = stdin.lines();

        while let Some(line) = lines.next_line().await? {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let response = match serde_json::from_str::<Value>(line) {
                Ok(req) => self.handle(&req).await,
                Err(e) => Some(json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": { "code": -32700, "message": format!("parse error: {e}") }
                })),
            };
            if let Some(response) = response {
                stdout.write_all(format!("{response}\n").as_bytes()).await?;
                stdout.flush().await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use modulex_core::{steps::builtin_registry, Config, Engine, GrantedCaveats};
    use serde_json::json;

    use super::*;

    const TEST_CONFIG: &str = r#"
[routines.morning]
description = "test morning"

[[routines.morning.steps]]
name = "greeting"
type = "script"
command = "sh"
args = ["-c", "echo hi"]

[[routines.morning.steps]]
name = "deadlines"
type = "deadline-calc"

[[routines.morning.steps]]
name = "agenda"
type = "reminders"
"#;

    /// A server over a mock spawner (no real processes) and an in-memory
    /// store (no filesystem).
    fn server(outputs: Vec<modulex_core::ExecOutput>) -> Server {
        let config = Config::from_toml(TEST_CONFIG).unwrap();
        let registry = builtin_registry();
        let declared = config.declared_programs(&registry);
        let granted = GrantedCaveats::resolve(None, None, declared)
            .unwrap()
            .caveats;
        let spawner = Arc::new(modulex_core::exec::test_support::MockSpawner::with_outputs(
            outputs,
        ));
        let store = Arc::new(modulex_core::Store::in_memory().unwrap());
        Server::new(Engine::with_spawner(config, registry, granted, spawner).with_store(store))
    }

    fn ok_out(stdout: &str) -> modulex_core::ExecOutput {
        modulex_core::ExecOutput {
            stdout: stdout.into(),
            status: Some(0),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn initialize_advertises_tools_capability() {
        let s = server(vec![]);
        let resp = s
            .handle(&json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }))
            .await
            .unwrap();
        assert_eq!(resp["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert!(resp["result"]["capabilities"]["tools"].is_object());
        assert_eq!(resp["result"]["serverInfo"]["name"], "modulex-mcp");
    }

    #[tokio::test]
    async fn initialized_notification_gets_no_response() {
        let s = server(vec![]);
        assert!(s
            .handle(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .await
            .is_none());
    }

    #[tokio::test]
    async fn tools_list_names_the_full_surface() {
        let s = server(vec![]);
        let resp = s
            .handle(&json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }))
            .await
            .unwrap();
        let names: Vec<&str> = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec![
                "routine_run",
                "routine_list",
                "step_run",
                "report_get",
                "steps_list",
                "store_put",
                "store_query",
                "store_close",
                "tool_search",
                "tool_describe",
                "tool_invoke",
                "routine_eval",
            ],
            "the DEFAULT index (progressive disclosure) — classic store tools \
             are discoverable + callable but unlisted"
        );
    }

    #[tokio::test]
    async fn unlisted_tools_remain_callable_and_discoverable() {
        // Listing is context cost, not capability: reminder_add is in the
        // non-default store-classic facet — absent from tools/list, but
        // direct tools/call works, tool_search finds it, tool_describe
        // discloses its schema, and tool_invoke dispatches it.
        let s = server(vec![]);
        let resp = s
            .handle(&json!({
                "jsonrpc": "2.0", "id": 60, "method": "tools/call",
                "params": { "name": "tool_search", "arguments": { "query": "reminder add" } }
            }))
            .await
            .unwrap();
        let found: Value =
            serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert!(found["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "reminder_add" && t["mutates"] == true));

        let resp = s
            .handle(&json!({
                "jsonrpc": "2.0", "id": 61, "method": "tools/call",
                "params": { "name": "tool_describe", "arguments": { "name": "reminder_add" } }
            }))
            .await
            .unwrap();
        let spec: Value =
            serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(spec["facet"], "store-classic");
        assert!(spec["inputSchema"]["properties"]["text"].is_object());

        let resp = s
            .handle(&json!({
                "jsonrpc": "2.0", "id": 62, "method": "tools/call",
                "params": { "name": "tool_invoke",
                            "arguments": { "name": "reminder_add",
                                           "arguments": { "text": "via invoke" } } }
            }))
            .await
            .unwrap();
        assert!(resp["result"].get("isError").is_none(), "{resp}");

        // …and the record landed (store trio query view).
        let resp = s
            .handle(&json!({
                "jsonrpc": "2.0", "id": 63, "method": "tools/call",
                "params": { "name": "store_query", "arguments": { "kind": "reminder" } }
            }))
            .await
            .unwrap();
        let open: Value =
            serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(open[0]["text"], "via invoke");
    }

    #[tokio::test]
    async fn store_trio_round_trips_every_kind() {
        let s = server(vec![]);
        for (kind, record, close_works) in [
            ("reminder", json!({ "text": "r1" }), true),
            (
                "countdown",
                json!({ "label": "c1", "start_date": "2026-06-01",
                                   "end_date": "2999-01-01" }),
                true,
            ),
            ("watch", json!({ "url": "https://example.com" }), true),
        ] {
            let resp = s
                .handle(&json!({
                    "jsonrpc": "2.0", "id": 70, "method": "tools/call",
                    "params": { "name": "store_put",
                                "arguments": { "kind": kind, "record": record } }
                }))
                .await
                .unwrap();
            assert!(resp["result"].get("isError").is_none(), "{kind}: {resp}");
            let put: Value =
                serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap())
                    .unwrap();
            let id = put["id"].as_i64().unwrap();

            let resp = s
                .handle(&json!({
                    "jsonrpc": "2.0", "id": 71, "method": "tools/call",
                    "params": { "name": "store_close",
                                "arguments": { "kind": kind, "id": id } }
                }))
                .await
                .unwrap();
            assert_eq!(
                resp["result"].get("isError").is_none(),
                close_works,
                "{kind} close: {resp}"
            );
        }

        // Unknown kind is a tool error.
        let resp = s
            .handle(&json!({
                "jsonrpc": "2.0", "id": 72, "method": "tools/call",
                "params": { "name": "store_put",
                            "arguments": { "kind": "bogus", "record": {} } }
            }))
            .await
            .unwrap();
        assert_eq!(resp["result"]["isError"], true);
    }

    #[tokio::test]
    async fn routine_eval_runs_inline_steps_under_the_same_leash() {
        let s = server(vec![ok_out("inline says hi\n")]);
        // A two-step inline routine: a granted script + a pure date step.
        let resp = s
            .handle(&json!({
                "jsonrpc": "2.0", "id": 90, "method": "tools/call",
                "params": { "name": "routine_eval", "arguments": {
                    "steps": [
                        { "name": "greet", "type": "script", "command": "sh",
                          "args": ["-c", "echo hi"] },
                        { "name": "dl", "type": "deadline-calc" }
                    ],
                    "format": "data"
                } }
            }))
            .await
            .unwrap();
        assert!(resp["result"].get("isError").is_none(), "{resp}");
        let report: Value =
            serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(report["routine"], "eval");
        assert_eq!(report["generation"], 1, "evals are generation-stamped runs");
        assert_eq!(report["steps"][0]["data"]["exit_code"], 0);
        assert_eq!(report["steps"][1]["type"], "deadline-calc");

        // The stored report is fetchable like any run.
        let resp = s
            .handle(&json!({
                "jsonrpc": "2.0", "id": 91, "method": "tools/call",
                "params": { "name": "report_get",
                            "arguments": { "generation": 1, "format": "json" } }
            }))
            .await
            .unwrap();
        let stored: Value =
            serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(stored["routine"], "eval");
    }

    #[tokio::test]
    async fn routine_eval_cannot_widen_authority() {
        // The engine's grant was fixed at build (declared programs of the
        // CONFIG). An inline step naming an ungranted program is denied by
        // the leash — a soft step failure carrying the denial reason.
        let s = server(vec![]);
        let resp = s
            .handle(&json!({
                "jsonrpc": "2.0", "id": 92, "method": "tools/call",
                "params": { "name": "routine_eval", "arguments": {
                    "steps": [ { "name": "sneak", "type": "script",
                                 "command": "curl", "args": ["http://evil"] } ],
                    "format": "data"
                } }
            }))
            .await
            .unwrap();
        assert!(
            resp["result"].get("isError").is_none(),
            "soft failure, not fault"
        );
        let report: Value =
            serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(report["success"], false);
        assert!(report["steps"][0]["error"]
            .as_str()
            .unwrap()
            .contains("granted authority"));
    }

    #[tokio::test]
    async fn routine_eval_validates_its_input() {
        let s = server(vec![]);
        for (args, marker) in [
            (json!({}), "requires a `steps` array"),
            (json!({ "steps": [] }), "must not be empty"),
            (
                json!({ "steps": [ { "type": "script" } ] }), // missing name
                "not a valid step spec",
            ),
        ] {
            let resp = s
                .handle(&json!({
                    "jsonrpc": "2.0", "id": 93, "method": "tools/call",
                    "params": { "name": "routine_eval", "arguments": args }
                }))
                .await
                .unwrap();
            assert_eq!(resp["result"]["isError"], true);
            assert!(
                resp["result"]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains(marker),
                "expected {marker:?} in {resp}"
            );
        }
    }

    #[tokio::test]
    async fn denied_facets_are_dead_everywhere() {
        use modulex_core::config::McpConfig;
        // Build a server whose policy denies store-classic entirely.
        let config = Config::from_toml(TEST_CONFIG).unwrap();
        let registry = builtin_registry();
        let declared = config.declared_programs(&registry);
        let granted = GrantedCaveats::resolve(None, None, declared)
            .unwrap()
            .caveats;
        let spawner = Arc::new(modulex_core::exec::test_support::MockSpawner::with_outputs(
            vec![],
        ));
        let store = Arc::new(modulex_core::Store::in_memory().unwrap());
        let engine =
            Arc::new(Engine::with_spawner(config, registry, granted, spawner).with_store(store));
        let policy = crate::facets::FacetPolicy::resolve(
            None,
            &McpConfig {
                expose: vec![],
                deny: vec!["store-classic".into()],
            },
        );
        let s = Server::with_policy(engine, policy);

        // Not callable directly…
        let resp = s
            .handle(&json!({
                "jsonrpc": "2.0", "id": 80, "method": "tools/call",
                "params": { "name": "reminder_add", "arguments": { "text": "x" } }
            }))
            .await
            .unwrap();
        assert_eq!(resp["result"]["isError"], true);
        // …not via invoke…
        let resp = s
            .handle(&json!({
                "jsonrpc": "2.0", "id": 81, "method": "tools/call",
                "params": { "name": "tool_invoke",
                            "arguments": { "name": "reminder_add",
                                           "arguments": { "text": "x" } } }
            }))
            .await
            .unwrap();
        assert_eq!(resp["result"]["isError"], true);
        // …and invisible to search.
        let resp = s
            .handle(&json!({
                "jsonrpc": "2.0", "id": 82, "method": "tools/call",
                "params": { "name": "tool_search", "arguments": { "query": "reminder_add" } }
            }))
            .await
            .unwrap();
        let found: Value =
            serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert!(found["tools"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn reminder_lifecycle_over_mcp_stamps_generations() {
        let s = server(vec![ok_out("hi\n")]);
        // Run once so the current generation is 1.
        s.handle(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "routine_run", "arguments": { "routine": "morning" } }
        }))
        .await
        .unwrap();

        // Register a reminder — stamped "after run 1".
        let resp = s
            .handle(&json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": { "name": "reminder_add",
                            "arguments": { "text": "rotate the token", "due": "2026-06-10" } }
            }))
            .await
            .unwrap();
        assert!(resp["result"].get("isError").is_none());
        let created: Value =
            serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(created["created_gen"], 1);
        let id = created["id"].as_i64().unwrap();

        // It lists as open…
        let resp = s
            .handle(&json!({
                "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": { "name": "reminder_list" }
            }))
            .await
            .unwrap();
        let open: Value =
            serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(open[0]["text"], "rotate the token");

        // …surfaces in the reminders step of the NEXT run…
        let resp = s
            .handle(&json!({
                "jsonrpc": "2.0", "id": 4, "method": "tools/call",
                "params": { "name": "step_run",
                            "arguments": { "routine": "morning", "step": "agenda" } }
            }))
            .await
            .unwrap();
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("rotate the token"), "got: {text}");

        // …and double-done is an error.
        let resp = s
            .handle(&json!({
                "jsonrpc": "2.0", "id": 5, "method": "tools/call",
                "params": { "name": "reminder_done", "arguments": { "id": id } }
            }))
            .await
            .unwrap();
        assert!(resp["result"].get("isError").is_none());
        let resp = s
            .handle(&json!({
                "jsonrpc": "2.0", "id": 6, "method": "tools/call",
                "params": { "name": "reminder_done", "arguments": { "id": id } }
            }))
            .await
            .unwrap();
        assert_eq!(resp["result"]["isError"], true);
    }

    #[tokio::test]
    async fn watch_and_countdown_tools_round_trip() {
        let s = server(vec![]);
        let resp = s
            .handle(&json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "name": "watch_add",
                            "arguments": { "url": "https://example.com/releases",
                                           "note": "new versions" } }
            }))
            .await
            .unwrap();
        assert!(resp["result"].get("isError").is_none());

        // Non-http URLs are rejected before the store.
        let resp = s
            .handle(&json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": { "name": "watch_add", "arguments": { "url": "file:///etc/passwd" } }
            }))
            .await
            .unwrap();
        assert_eq!(resp["result"]["isError"], true);

        let resp = s
            .handle(&json!({
                "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": { "name": "countdown_add",
                            "arguments": { "label": "ramp", "start_date": "2026-06-01",
                                           "end_date": "2026-07-15" } }
            }))
            .await
            .unwrap();
        assert!(resp["result"].get("isError").is_none());

        // Export carries everything as plain JSON (sovereignty).
        let resp = s
            .handle(&json!({
                "jsonrpc": "2.0", "id": 4, "method": "tools/call",
                "params": { "name": "store_export" }
            }))
            .await
            .unwrap();
        let dump: Value =
            serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(dump["watches"][0]["url"], "https://example.com/releases");
        assert_eq!(dump["countdowns"][0]["label"], "ramp");
    }

    #[tokio::test]
    async fn routine_run_returns_report_text_without_is_error() {
        let s = server(vec![ok_out("hi\n")]);
        let resp = s
            .handle(&json!({
                "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": { "name": "routine_run", "arguments": { "routine": "morning" } }
            }))
            .await
            .unwrap();
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("# morning — gen 1"));
        assert!(text.contains("## greeting\nhi"));
        assert!(resp["result"].get("isError").is_none());
    }

    #[tokio::test]
    async fn per_step_failure_is_data_not_is_error() {
        // The script fails — the report records it, the tool call succeeds.
        let s = server(vec![modulex_core::ExecOutput {
            stderr: "boom".into(),
            status: Some(1),
            ..Default::default()
        }]);
        let resp = s
            .handle(&json!({
                "jsonrpc": "2.0", "id": 4, "method": "tools/call",
                "params": { "name": "routine_run",
                            "arguments": { "routine": "morning", "format": "json" } }
            }))
            .await
            .unwrap();
        assert!(resp["result"].get("isError").is_none(), "soft failure");
        let report: Value =
            serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(report["success"], false);
        assert_eq!(report["step_results"][0]["error"], "boom");
    }

    #[tokio::test]
    async fn format_data_returns_structured_payloads_only() {
        // The agent-native view (data contract): typed payloads, no prose.
        let s = server(vec![ok_out("hi\n")]);
        let resp = s
            .handle(&json!({
                "jsonrpc": "2.0", "id": 7, "method": "tools/call",
                "params": { "name": "routine_run",
                            "arguments": { "routine": "morning", "format": "data" } }
            }))
            .await
            .unwrap();
        let report: Value =
            serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(report["generation"], 1);
        let steps = report["steps"].as_array().unwrap();
        // script step carries its typed exit_code, not markdown
        let greeting = &steps[0];
        assert_eq!(greeting["type"], "script");
        assert_eq!(greeting["data"]["exit_code"], 0);
        assert!(greeting.get("output").is_none(), "no prose in data view");
        // deadline step carries the typed empty contract
        let deadlines = &steps[1];
        assert_eq!(deadlines["data"]["deadlines"], json!([]));
    }

    #[tokio::test]
    async fn unknown_routine_is_an_engine_fault_with_is_error() {
        let s = server(vec![]);
        let resp = s
            .handle(&json!({
                "jsonrpc": "2.0", "id": 5, "method": "tools/call",
                "params": { "name": "routine_run", "arguments": { "routine": "nope" } }
            }))
            .await
            .unwrap();
        assert_eq!(resp["result"]["isError"], true);
        assert!(resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("unknown routine"));
    }

    #[tokio::test]
    async fn report_get_by_generation_and_latest() {
        let s = server(vec![ok_out("one"), ok_out("two")]);
        for id in [10, 11] {
            s.handle(&json!({
                "jsonrpc": "2.0", "id": id, "method": "tools/call",
                "params": { "name": "routine_run", "arguments": { "routine": "morning" } }
            }))
            .await
            .unwrap();
        }

        let latest = s
            .handle(&json!({
                "jsonrpc": "2.0", "id": 12, "method": "tools/call",
                "params": { "name": "report_get", "arguments": { "format": "json" } }
            }))
            .await
            .unwrap();
        let report: Value =
            serde_json::from_str(latest["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(report["generation"], 2);

        let first = s
            .handle(&json!({
                "jsonrpc": "2.0", "id": 13, "method": "tools/call",
                "params": { "name": "report_get",
                            "arguments": { "generation": 1, "format": "json" } }
            }))
            .await
            .unwrap();
        let report: Value =
            serde_json::from_str(first["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(report["generation"], 1);

        let missing = s
            .handle(&json!({
                "jsonrpc": "2.0", "id": 14, "method": "tools/call",
                "params": { "name": "report_get", "arguments": { "generation": 99 } }
            }))
            .await
            .unwrap();
        assert_eq!(missing["result"]["isError"], true);
    }

    #[tokio::test]
    async fn routine_list_and_steps_list_describe_the_surface() {
        let s = server(vec![]);
        let resp = s
            .handle(&json!({
                "jsonrpc": "2.0", "id": 20, "method": "tools/call",
                "params": { "name": "routine_list" }
            }))
            .await
            .unwrap();
        let payload: Value =
            serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(payload["routines"][0]["name"], "morning");
        assert_eq!(payload["routines"][0]["steps"], 3);

        let resp = s
            .handle(&json!({
                "jsonrpc": "2.0", "id": 21, "method": "tools/call",
                "params": { "name": "steps_list" }
            }))
            .await
            .unwrap();
        let payload: Value =
            serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        let steps = payload["steps"].as_array().unwrap();
        let script = steps
            .iter()
            .find(|s| s["type"] == "script")
            .expect("script registered");
        assert!(script["description"].as_str().unwrap().contains("command"));
        assert!(script["data_schema"]["properties"]["exit_code"].is_object());
        assert!(steps.iter().any(|s| s["type"] == "harness"));
    }

    #[tokio::test]
    async fn missing_method_with_id_gets_invalid_request_not_silence() {
        // Regression (fresh-eyes 2026-06-05): silence here would hang an MCP
        // client until its request timeout.
        let s = server(vec![]);
        let resp = s
            .handle(&json!({ "jsonrpc": "2.0", "id": 9 }))
            .await
            .unwrap();
        assert_eq!(resp["error"]["code"], -32600);

        // ...but an id-less, method-less blob is unanswerable noise.
        assert!(s.handle(&json!({ "jsonrpc": "2.0" })).await.is_none());
    }

    #[tokio::test]
    async fn unknown_method_is_a_jsonrpc_error() {
        let s = server(vec![]);
        let resp = s
            .handle(&json!({ "jsonrpc": "2.0", "id": 30, "method": "bogus/method" }))
            .await
            .unwrap();
        assert_eq!(resp["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn unknown_tool_is_a_tool_error() {
        let s = server(vec![]);
        let resp = s
            .handle(&json!({
                "jsonrpc": "2.0", "id": 31, "method": "tools/call",
                "params": { "name": "bogus_tool" }
            }))
            .await
            .unwrap();
        assert_eq!(resp["result"]["isError"], true);
    }

    #[tokio::test]
    async fn dry_run_filters_pass_through() {
        let s = server(vec![]);
        let resp = s
            .handle(&json!({
                "jsonrpc": "2.0", "id": 40, "method": "tools/call",
                "params": { "name": "routine_run",
                            "arguments": { "routine": "morning", "dry_run": true,
                                           "only": ["deadlines"], "format": "json" } }
            }))
            .await
            .unwrap();
        let report: Value =
            serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(report["dry_run"], true);
        assert_eq!(report["step_results"].as_array().unwrap().len(), 1);
        assert_eq!(report["step_results"][0]["step_name"], "deadlines");
    }

    #[tokio::test]
    async fn step_run_validates_arguments() {
        let s = server(vec![]);
        let resp = s
            .handle(&json!({
                "jsonrpc": "2.0", "id": 50, "method": "tools/call",
                "params": { "name": "step_run", "arguments": { "routine": "morning" } }
            }))
            .await
            .unwrap();
        assert_eq!(resp["result"]["isError"], true);

        let resp = s
            .handle(&json!({
                "jsonrpc": "2.0", "id": 51, "method": "tools/call",
                "params": { "name": "step_run",
                            "arguments": { "routine": "morning", "step": "deadlines",
                                           "dry_run": true } }
            }))
            .await
            .unwrap();
        assert!(resp["result"].get("isError").is_none());
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("## deadlines"));
    }
}
