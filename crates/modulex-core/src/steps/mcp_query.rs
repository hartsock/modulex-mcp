//! The `mcp-query` step — call a tool on a registered downstream MCP server
//! and embed only its RESULT in the report (issue #7, PR D: "hide other MCPs
//! behind modulex").
//!
//! This is the credential-proxy surface. A hotseat agent points its harness
//! at modulex and never touches enterprise credentials: it asks modulex to
//! run `server.tool(args)`; modulex spawns the downstream server, injects the
//! server's credential *references* (resolved at spawn time), calls the tool,
//! and returns the tool result. The calling agent sees the result — never the
//! command line, never the credentials.
//!
//! ## Security model (adversarial-review surface)
//!
//! 1. **Leashed spawn.** The downstream command runs through
//!    [`crate::exec::ExecGate::spawn`], which calls agent-bridle's `check_exec`
//!    BEFORE any process exists. A server whose command is not in the run's
//!    exec grant is **denied, not run** — the store cannot silently widen the
//!    leash. To authorize a registered server, its command must be granted:
//!    either by a config `mcp-query` step that declares `command` inline (so
//!    it joins the declared-default grant via [`StepHandler::required_programs`])
//!    or by an explicit `[caveats] exec`.
//! 2. **Credentials by reference, never by value.** The downstream server's
//!    secrets are supplied through the step's `env = { NAME = {env|file|cmd} }`
//!    references — the same [`crate::credentials::Secret`] model as every other
//!    step. Secrets are unserializable by construction, injected only into the
//!    child environment, and scrubbed from captured output. They never reach
//!    the store, an export, the report `data`, the report markdown, or an error
//!    string.
//! 3. **Result only.** Only the JSON-RPC tool *result* is embedded. The
//!    spawned command line and resolved env are never serialized into the
//!    report.
//!
//! ## Transport
//!
//! MCP stdio is newline-delimited JSON-RPC 2.0. modulex writes the
//! handshake + call as a batch to the child's stdin
//! (`initialize` → `notifications/initialized` → `tools/call`), closes stdin,
//! and parses the responses from stdout, matching by request `id`. This rides
//! the existing single-shot [`crate::exec::Spawner`] seam, so it is mockable
//! and leashed exactly like every other subprocess.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::config::{expand_tilde, StepSpec};
use crate::exec::ExecRequest;
use crate::report::StepResult;
use crate::step::{resolve_step_env, RunContext, StepHandler};
use crate::store::McpServer;

/// JSON-RPC id of the `initialize` request.
const ID_INIT: i64 = 1;
/// JSON-RPC id of the `tools/call` request.
const ID_CALL: i64 = 2;

/// The MCP protocol version modulex speaks to downstreams (same as the
/// modulex server's own).
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Build the newline-delimited JSON-RPC batch sent to a downstream server's
/// stdin: `initialize`, `notifications/initialized`, then `tools/call`.
/// Factored out (pure) so the wire shape is unit-tested without a process.
#[must_use]
pub fn build_stdin(tool: &str, arguments: &Value) -> String {
    let initialize = json!({
        "jsonrpc": "2.0",
        "id": ID_INIT,
        "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "modulex-mcp-proxy", "version": "0.1.0" }
        }
    });
    let initialized = json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    let call = json!({
        "jsonrpc": "2.0",
        "id": ID_CALL,
        "method": "tools/call",
        "params": { "name": tool, "arguments": arguments }
    });
    format!("{initialize}\n{initialized}\n{call}\n")
}

/// Find the JSON-RPC response object with the given `id` in newline-delimited
/// stdout. Non-JSON lines and notifications (no `id`) are skipped, so a chatty
/// server that logs to stdout does not break the parse.
fn find_response(stdout: &str, id: i64) -> Option<Value> {
    stdout
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            serde_json::from_str::<Value>(line).ok()
        })
        .find(|v| v.get("id").and_then(Value::as_i64) == Some(id))
}

/// Outcome of parsing a downstream `tools/call` response: either the tool
/// result payload, or a human-readable error.
#[derive(Debug, PartialEq, Eq)]
pub enum CallOutcome {
    /// The `result` object from the `tools/call` response.
    Result(Value),
    /// A JSON-RPC error or a malformed/absent response.
    Error(String),
}

/// Extract the `tools/call` result (id [`ID_CALL`]) from a downstream's
/// stdout. Pure, so the response handling is unit-tested without a process.
#[must_use]
pub fn parse_call_response(stdout: &str) -> CallOutcome {
    let Some(response) = find_response(stdout, ID_CALL) else {
        return CallOutcome::Error(
            "downstream MCP returned no response to tools/call (handshake may have failed)".into(),
        );
    };
    if let Some(error) = response.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("downstream tool error");
        return CallOutcome::Error(format!("downstream MCP error: {message}"));
    }
    match response.get("result") {
        Some(result) => CallOutcome::Result(result.clone()),
        None => {
            CallOutcome::Error("downstream MCP response carried neither result nor error".into())
        }
    }
}

/// Render a tool result as a human-readable report body. MCP `tools/call`
/// results carry a `content` array of typed blocks; we surface text blocks and
/// fall back to compact JSON for anything else.
fn render_result(result: &Value) -> String {
    if let Some(content) = result.get("content").and_then(Value::as_array) {
        let mut chunks = Vec::new();
        for block in content {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        chunks.push(text.to_string());
                    }
                }
                _ => chunks.push(block.to_string()),
            }
        }
        if !chunks.is_empty() {
            return chunks.join("\n");
        }
    }
    result.to_string()
}

/// The (tilde-expanded) command a registered server will spawn — used for BOTH
/// the declared grant and the spawn request, so the leash compares like with
/// like. Server-name → command requires the store, so this only resolves the
/// `command` param a config step may declare inline (see the security model).
fn declared_command(spec: &StepSpec) -> Option<String> {
    spec.param_str("command")
        .map(|c| expand_tilde(c).to_string_lossy().into_owned())
}

/// `mcp-query`: call `server.tool(args)` on a registered downstream MCP.
pub struct McpQuery;

#[async_trait]
impl StepHandler for McpQuery {
    fn type_name(&self) -> &'static str {
        "mcp-query"
    }

    fn description(&self) -> &'static str {
        "Call a tool on a registered downstream MCP server (leashed stdio \
         client) and embed only its result"
    }

    fn data_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["server", "tool", "state"],
            "properties": {
                "server": { "type": "string" },
                "tool": { "type": "string" },
                "state": { "type": "string",
                           "enum": ["ok", "denied", "error", "skipped"] },
                "result": { "description": "the downstream tool result (state=ok); \
                                            never contains credentials" },
                "detail": { "type": "string", "description": "error/denial text" }
            }
        })
    }

    fn required_programs(&self, spec: &StepSpec) -> Vec<String> {
        // A config step may declare `command` inline so the downstream server's
        // program joins the declared-default exec grant. A server selected only
        // by `server` name (resolved from the store at run time) is NOT
        // auto-granted here — the store must not silently widen the leash; such
        // a server needs an explicit `[caveats] exec`. See the security model.
        declared_command(spec).into_iter().collect()
    }

    async fn run(&self, spec: &StepSpec, cx: &RunContext) -> StepResult {
        let Some(server_name) = spec.param_str("server") else {
            return StepResult::fail(
                &spec.name,
                &spec.step_type,
                "missing required param `server` (a registered MCP server name)",
            );
        };
        let Some(tool) = spec.param_str("tool") else {
            return StepResult::fail(&spec.name, &spec.step_type, "missing required param `tool`");
        };
        let arguments = spec
            .params
            .get("arguments")
            .and_then(|v| serde_json::to_value(v).ok())
            .unwrap_or_else(|| json!({}));

        let Some(store) = &cx.store else {
            return StepResult::skip(
                &spec.name,
                &spec.step_type,
                "agent state store unavailable — no MCP registry",
            );
        };
        let server = match store.mcp_server(server_name) {
            Ok(Some(server)) => server,
            Ok(None) => {
                return StepResult::fail(
                    &spec.name,
                    &spec.step_type,
                    format!("no registered MCP server named {server_name:?}"),
                )
            }
            Err(e) => return StepResult::fail(&spec.name, &spec.step_type, e.to_string()),
        };

        if cx.dry_run {
            return StepResult::ok(
                &spec.name,
                &spec.step_type,
                format!(
                    "[dry-run] would call {tool:?} on registered MCP {:?} (command {:?}, leashed)",
                    server.name, server.command
                ),
            );
        }

        self.query(spec, cx, &server, tool, &arguments).await
    }
}

impl McpQuery {
    /// The leashed call against an already-resolved server record.
    async fn query(
        &self,
        spec: &StepSpec,
        cx: &RunContext,
        server: &McpServer,
        tool: &str,
        arguments: &Value,
    ) -> StepResult {
        // Credentials are resolved by REFERENCE here, immediately before spawn,
        // into unserializable Secrets — never read from or written to the store.
        let env = match resolve_step_env(spec, &cx.exec).await {
            Ok(env) => env,
            Err((name, error)) => {
                // The error names the env var, never the value.
                return self.soft(
                    spec,
                    server,
                    tool,
                    "skipped",
                    format!("credential {name} unavailable: {error}"),
                    true,
                );
            }
        };

        let stdin = build_stdin(tool, arguments);
        // The leash check (`check_exec(server.command)`) happens inside spawn,
        // BEFORE any process exists. An ungranted command is Denied here.
        let request =
            ExecRequest::new(expand_tilde(&server.command).to_string_lossy().into_owned())
                .args(server.args.clone())
                .env(env)
                .stdin(stdin)
                .timeout(Duration::from_secs(spec.timeout));

        let out = match cx.exec.spawn(request).await {
            Ok(out) => out,
            Err(crate::exec::ExecError::Denied(reason)) => {
                // Out-of-grant downstream command: denied, not run. This is the
                // proxy's exec-leash enforcement point.
                return self.soft(spec, server, tool, "denied", reason, false);
            }
            Err(e) => return self.soft(spec, server, tool, "error", e.to_string(), false),
        };

        if out.timed_out {
            return self.soft(
                spec,
                server,
                tool,
                "error",
                format!("downstream MCP {:?} timed out", server.name),
                false,
            );
        }

        match parse_call_response(&out.stdout) {
            CallOutcome::Result(result) => {
                let body = render_result(&result);
                let mut step = StepResult::ok(&spec.name, &spec.step_type, body);
                step.data = Some(json!({
                    "server": server.name,
                    "tool": tool,
                    "state": "ok",
                    "result": result,
                }));
                step
            }
            CallOutcome::Error(detail) => {
                // A downstream error is data, not a dead routine. stderr is NOT
                // surfaced (it could carry credential echoes); only the parsed
                // JSON-RPC error message is reported.
                self.soft(spec, server, tool, "error", detail, false)
            }
        }
    }

    /// A soft outcome (skip or fail) carrying typed `data` but no credentials.
    fn soft(
        &self,
        spec: &StepSpec,
        server: &McpServer,
        tool: &str,
        state: &str,
        detail: String,
        skipped: bool,
    ) -> StepResult {
        let mut step = if skipped {
            StepResult::skip(&spec.name, &spec.step_type, detail.clone())
        } else {
            StepResult::fail(&spec.name, &spec.step_type, detail.clone())
        };
        step.data = Some(json!({
            "server": server.name,
            "tool": tool,
            "state": state,
            "detail": detail,
        }));
        step
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

    /// A `tools/call` success response a well-behaved server emits.
    fn ok_response() -> String {
        let init =
            json!({"jsonrpc":"2.0","id":ID_INIT,"result":{"protocolVersion":PROTOCOL_VERSION}});
        let call = json!({
            "jsonrpc":"2.0","id":ID_CALL,
            "result":{"content":[{"type":"text","text":"42 open issues"}]}
        });
        format!("{init}\n{call}\n")
    }

    fn cx_with(
        store: Option<Arc<Store>>,
        outputs: Vec<ExecOutput>,
        allow: &[&str],
    ) -> (RunContext, Arc<MockSpawner>) {
        let spawner = Arc::new(MockSpawner::with_outputs(outputs));
        let granted = Caveats {
            exec: Scope::only(allow.iter().map(ToString::to_string)),
            ..Caveats::top()
        };
        (
            RunContext {
                config: Arc::new(Config::default()),
                dry_run: false,
                generation: 9,
                exec: gate_with(&granted, spawner.clone()),
                prior: Vec::new(),
                store,
            },
            spawner,
        )
    }

    fn store_with_server(command: &str) -> Arc<Store> {
        let store = Arc::new(Store::in_memory().unwrap());
        store
            .mcp_register("gh", command, &["serve".into()], "github", 1)
            .unwrap();
        store
    }

    fn spec(toml_text: &str) -> StepSpec {
        toml::from_str(toml_text).unwrap()
    }

    // ── pure wire-shape tests (no process) ─────────────────────────────

    #[test]
    fn build_stdin_carries_handshake_then_call() {
        let stdin = build_stdin("issues_list", &json!({"repo": "x"}));
        let lines: Vec<&str> = stdin.lines().collect();
        assert_eq!(lines.len(), 3);
        let init: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(init["method"], "initialize");
        assert_eq!(init["id"], ID_INIT);
        let note: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(note["method"], "notifications/initialized");
        assert!(note.get("id").is_none(), "notifications carry no id");
        let call: Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(call["method"], "tools/call");
        assert_eq!(call["params"]["name"], "issues_list");
        assert_eq!(call["params"]["arguments"]["repo"], "x");
    }

    #[test]
    fn parse_call_response_extracts_result() {
        match parse_call_response(&ok_response()) {
            CallOutcome::Result(r) => {
                assert_eq!(r["content"][0]["text"], "42 open issues");
            }
            other => panic!("expected result, got {other:?}"),
        }
    }

    #[test]
    fn parse_call_response_surfaces_jsonrpc_error() {
        let stdout = format!(
            "{}\n",
            json!({"jsonrpc":"2.0","id":ID_CALL,"error":{"code":-32602,"message":"bad args"}})
        );
        match parse_call_response(&stdout) {
            CallOutcome::Error(e) => assert!(e.contains("bad args")),
            other => panic!("expected error, got {other:?}"),
        }
    }

    #[test]
    fn parse_call_response_handles_missing_and_chatty_lines() {
        // No tools/call response at all (handshake failed).
        assert!(matches!(
            parse_call_response("not json\n{\"jsonrpc\":\"2.0\"}\n"),
            CallOutcome::Error(_)
        ));
        // Chatty log lines before a valid response are ignored.
        let stdout = format!("starting up...\n{}", ok_response());
        assert!(matches!(
            parse_call_response(&stdout),
            CallOutcome::Result(_)
        ));
    }

    #[test]
    fn render_result_prefers_text_blocks() {
        let r = json!({"content":[{"type":"text","text":"hello"},{"type":"text","text":"world"}]});
        assert_eq!(render_result(&r), "hello\nworld");
        // Non-content results fall back to JSON.
        assert_eq!(render_result(&json!({"k":1})), "{\"k\":1}");
    }

    // ── leashed run tests (mocked spawner) ─────────────────────────────

    #[tokio::test]
    async fn happy_path_embeds_only_the_tool_result() {
        let store = store_with_server("gh-mcp");
        let (cx, spawner) = cx_with(
            Some(store),
            vec![MockSpawner::ok(&ok_response())],
            &["gh-mcp"],
        );
        let result = McpQuery
            .run(
                &spec("name=\"q\"\ntype=\"mcp-query\"\nserver=\"gh\"\ntool=\"issues_list\""),
                &cx,
            )
            .await;
        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.output, "42 open issues");
        assert_eq!(result.data.as_ref().unwrap()["state"], "ok");
        // The downstream command WAS the one spawned, with its static argv.
        let calls = spawner.calls.lock().unwrap();
        assert_eq!(calls[0].0, "gh-mcp");
        assert_eq!(calls[0].1, vec!["serve".to_string()]);
    }

    #[tokio::test]
    async fn ungranted_downstream_command_is_denied_not_run() {
        // The server's command is NOT in the exec grant.
        let store = store_with_server("forbidden-mcp");
        let (cx, spawner) = cx_with(
            Some(store),
            vec![MockSpawner::ok(&ok_response())],
            &["something-else"],
        );
        let result = McpQuery
            .run(
                &spec("name=\"q\"\ntype=\"mcp-query\"\nserver=\"gh\"\ntool=\"x\""),
                &cx,
            )
            .await;
        assert!(!result.success);
        assert_eq!(result.data.as_ref().unwrap()["state"], "denied");
        assert!(
            spawner.calls.lock().unwrap().is_empty(),
            "denial happens BEFORE any spawn"
        );
    }

    #[tokio::test]
    async fn unknown_server_fails_cleanly() {
        let store = Arc::new(Store::in_memory().unwrap());
        let (cx, _) = cx_with(Some(store), vec![], &["whatever"]);
        let result = McpQuery
            .run(
                &spec("name=\"q\"\ntype=\"mcp-query\"\nserver=\"ghost\"\ntool=\"x\""),
                &cx,
            )
            .await;
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap().contains("ghost"));
    }

    #[tokio::test]
    async fn timed_out_downstream_is_soft_error_not_a_hung_routine() {
        let store = store_with_server("gh-mcp");
        let timeout = ExecOutput {
            timed_out: true,
            ..Default::default()
        };
        let (cx, _) = cx_with(Some(store), vec![timeout], &["gh-mcp"]);
        let result = McpQuery
            .run(
                &spec("name=\"q\"\ntype=\"mcp-query\"\nserver=\"gh\"\ntool=\"x\""),
                &cx,
            )
            .await;
        assert!(!result.success);
        assert_eq!(result.data.as_ref().unwrap()["state"], "error");
        assert!(result.error.as_deref().unwrap().contains("timed out"));
    }

    #[tokio::test]
    async fn missing_store_soft_skips() {
        let (cx, _) = cx_with(None, vec![], &["gh-mcp"]);
        let result = McpQuery
            .run(
                &spec("name=\"q\"\ntype=\"mcp-query\"\nserver=\"gh\"\ntool=\"x\""),
                &cx,
            )
            .await;
        assert!(result.skipped);
    }

    #[tokio::test]
    async fn missing_credential_soft_skips_naming_var_not_value() {
        let store = store_with_server("gh-mcp");
        let (cx, spawner) = cx_with(Some(store), vec![], &["gh-mcp"]);
        let result = McpQuery
            .run(
                &spec(
                    "name=\"q\"\ntype=\"mcp-query\"\nserver=\"gh\"\ntool=\"x\"\n\
                     env = { GITHUB_TOKEN = { env = \"MODULEX_TEST_UNSET_VAR_XYZZY\" } }",
                ),
                &cx,
            )
            .await;
        assert!(result.skipped);
        assert!(result.output.contains("GITHUB_TOKEN"));
        assert!(
            !result.output.contains("MODULEX_TEST_UNSET_VAR_XYZZY")
                || result.output.contains("not set"),
            "names the var, not a value"
        );
        assert!(spawner.calls.lock().unwrap().is_empty(), "never spawned");
    }

    #[tokio::test]
    async fn downstream_error_is_soft_data_not_dead_routine() {
        let store = store_with_server("gh-mcp");
        let err = format!(
            "{}\n",
            json!({"jsonrpc":"2.0","id":ID_CALL,"error":{"message":"rate limited"}})
        );
        let (cx, _) = cx_with(Some(store), vec![MockSpawner::ok(&err)], &["gh-mcp"]);
        let result = McpQuery
            .run(
                &spec("name=\"q\"\ntype=\"mcp-query\"\nserver=\"gh\"\ntool=\"x\""),
                &cx,
            )
            .await;
        assert!(!result.success);
        assert_eq!(result.data.as_ref().unwrap()["state"], "error");
        assert!(result.error.as_deref().unwrap().contains("rate limited"));
    }

    #[tokio::test]
    async fn resolved_credential_never_leaks_into_the_report() {
        // The credential-proxy guarantee: a server's secret, resolved by
        // reference at spawn time, must never appear in the report data,
        // markdown, or error text — only the tool RESULT is embedded.
        const SECRET: &str = "ghp_supersecrettoken";
        let store = store_with_server("gh-mcp");
        // First mock output answers the {cmd} credential resolution (stdout =
        // the secret); the second answers the downstream tools/call.
        let outputs = vec![MockSpawner::ok(SECRET), MockSpawner::ok(&ok_response())];
        // The credential command (`secret-tool`) and the server command must
        // both be granted.
        let (cx, _) = cx_with(Some(store), outputs, &["gh-mcp", "secret-tool"]);
        let result = McpQuery
            .run(
                &spec(
                    "name=\"q\"\ntype=\"mcp-query\"\nserver=\"gh\"\ntool=\"x\"\n\
                     env = { GITHUB_TOKEN = { cmd = \"secret-tool lookup gh\" } }",
                ),
                &cx,
            )
            .await;
        assert!(result.success, "{:?}", result.error);
        // Serialize the whole step result — the only thing an agent sees.
        let serialized = serde_json::to_string(&result).unwrap();
        assert!(
            !serialized.contains(SECRET),
            "credential value leaked into the report: {serialized}"
        );
        assert!(!result.output.contains(SECRET));
    }

    #[tokio::test]
    async fn dry_run_describes_without_spawning() {
        let store = store_with_server("gh-mcp");
        let (mut cx, spawner) = cx_with(Some(store), vec![], &["gh-mcp"]);
        cx.dry_run = true;
        let result = McpQuery
            .run(
                &spec("name=\"q\"\ntype=\"mcp-query\"\nserver=\"gh\"\ntool=\"issues_list\""),
                &cx,
            )
            .await;
        assert!(result.output.contains("[dry-run]"));
        assert!(result.output.contains("issues_list"));
        assert!(spawner.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn required_programs_declares_inline_command_only() {
        // Inline `command` joins the declared grant.
        let with_cmd =
            spec("name=\"q\"\ntype=\"mcp-query\"\nserver=\"gh\"\ntool=\"x\"\ncommand=\"gh-mcp\"");
        assert_eq!(
            McpQuery.required_programs(&with_cmd),
            vec!["gh-mcp".to_string()]
        );
        // Server-only: the store can't silently widen the grant.
        let server_only = spec("name=\"q\"\ntype=\"mcp-query\"\nserver=\"gh\"\ntool=\"x\"");
        assert!(McpQuery.required_programs(&server_only).is_empty());
    }

    #[tokio::test]
    async fn missing_params_fail_cleanly() {
        let store = store_with_server("gh-mcp");
        let (cx, _) = cx_with(Some(store), vec![], &["gh-mcp"]);
        let no_server = McpQuery
            .run(&spec("name=\"q\"\ntype=\"mcp-query\"\ntool=\"x\""), &cx)
            .await;
        assert!(no_server.error.as_deref().unwrap().contains("server"));
        let no_tool = McpQuery
            .run(&spec("name=\"q\"\ntype=\"mcp-query\"\nserver=\"gh\""), &cx)
            .await;
        assert!(no_tool.error.as_deref().unwrap().contains("tool"));
    }
}
