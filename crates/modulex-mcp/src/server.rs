//! The JSON-RPC 2.0 stdio loop (MCP protocol `2024-11-05`).
//!
//! Pattern: newline-delimited requests on stdin, one response line per
//! request with an id, notifications get none. Dispatch is serial — MCP
//! stdio is request/response, and routine runs are the long pole anyway.

use std::sync::Arc;

use modulex_core::Engine;
use serde_json::{json, Value};

use crate::tools;

/// MCP protocol revision this server implements.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// The MCP server: an engine plus the protocol shell.
pub struct Server {
    engine: Arc<Engine>,
    version: &'static str,
}

impl Server {
    /// A server over an engine.
    #[must_use]
    pub fn new(engine: Engine) -> Self {
        Self::with_engine(Arc::new(engine))
    }

    /// A server sharing an engine with another surface (the Python bindings
    /// run routines and serve MCP over the SAME engine, so generations and
    /// stored reports stay continuous).
    #[must_use]
    pub fn with_engine(engine: Arc<Engine>) -> Self {
        Self {
            engine,
            version: env!("CARGO_PKG_VERSION"),
        }
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
            "tools/list" => json!({ "tools": tools::tool_specs() }),
            "tools/call" => {
                let params = req.get("params").cloned().unwrap_or(Value::Null);
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let outcome = tools::call(&self.engine, name, &args).await;
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
                "reminder_add",
                "reminder_list",
                "reminder_done",
                "countdown_add",
                "countdown_retire",
                "watch_add",
                "watch_list",
                "watch_remove",
                "store_export",
            ]
        );
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
        let types = payload["step_types"].as_array().unwrap();
        assert!(types.iter().any(|t| t == "script"));
        assert!(types.iter().any(|t| t == "harness"));
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
