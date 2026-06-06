//! The subprocess plugin protocol: `type = "python"` (works for any
//! language, despite the name — the contract is JSON over stdio).
//!
//! ## Protocol `modulex-plugin/1`
//!
//! The engine spawns `interpreter script` (both leash-gated), writes ONE
//! JSON object to stdin, closes it, and reads ONE JSON object from stdout.
//!
//! Request:
//! ```json
//! {
//!   "protocol": "modulex-plugin/1",
//!   "generation": 42,
//!   "dry_run": false,
//!   "step": { "name": "...", "type": "python", "timeout": 60,
//!             "repos": [], "params": { ...flattened step params... } },
//!   "shared": { "repos": [...],
//!               "identity": { "username": "...", "gitlab_host": "..." } }
//! }
//! ```
//!
//! Response (exit 0):
//! ```json
//! {
//!   "protocol": "modulex-plugin/1",
//!   "success": true, "skipped": false,
//!   "output": "markdown body", "error": null,
//!   "repo_results": [ { "repo": "..", "output": "..",
//!                       "success": true, "error": null } ],
//!   "data": { "any": "structured payload" }
//! }
//! ```
//! Every response field is optional; defaults are success=true,
//! skipped=false, output="". Non-zero exit, unparsable stdout, or timeout →
//! a failed step result carrying the stderr tail. Credentials reach the
//! plugin ONLY via injected env (`env = { NAME = {..} }`) — never in the
//! JSON.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::config::{expand_tilde, StepSpec};
use crate::exec::ExecRequest;
use crate::report::{RepoResult, StepResult};
use crate::step::{resolve_step_env, RunContext, StepHandler};

/// Protocol identifier written in every request.
pub const PROTOCOL: &str = "modulex-plugin/1";

/// Build the stdin request object.
fn request_payload(spec: &StepSpec, cx: &RunContext) -> Value {
    json!({
        "protocol": PROTOCOL,
        "generation": cx.generation,
        "dry_run": cx.dry_run,
        "step": {
            "name": spec.name,
            "type": spec.step_type,
            "timeout": spec.timeout,
            "repos": spec.repos,
            "params": serde_json::to_value(&spec.params).unwrap_or(Value::Null),
        },
        "shared": {
            "repos": cx.config.shared.repos,
            "identity": {
                "username": cx.config.identity.username,
                "gitlab_host": cx.config.identity.gitlab_host,
            },
        },
    })
}

/// Map a plugin response object onto a [`StepResult`].
fn map_response(spec: &StepSpec, response: &Value) -> StepResult {
    let success = response
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let skipped = response
        .get("skipped")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let output = response
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let error = response
        .get("error")
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let repo_results: Vec<RepoResult> = response
        .get("repo_results")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|rr| RepoResult {
                    repo: rr["repo"].as_str().unwrap_or("?").to_string(),
                    output: rr["output"].as_str().unwrap_or("").to_string(),
                    success: rr["success"].as_bool().unwrap_or(true),
                    error: rr["error"].as_str().map(ToString::to_string),
                })
                .collect()
        })
        .unwrap_or_default();

    StepResult {
        step_name: spec.name.clone(),
        step_type: spec.step_type.clone(),
        success,
        skipped,
        output,
        error,
        repo_results,
        data: response.get("data").filter(|d| !d.is_null()).cloned(),
    }
}

/// Tail of stderr for error messages (plugins can be chatty).
fn stderr_tail(stderr: &str) -> String {
    let lines: Vec<&str> = stderr.trim().lines().collect();
    let tail = lines.len().saturating_sub(5);
    lines[tail..].join("\n")
}

/// `python`: run a plugin script under the `modulex-plugin/1` contract.
pub struct PythonPlugin;

#[async_trait]
impl StepHandler for PythonPlugin {
    fn type_name(&self) -> &'static str {
        "python"
    }

    fn description(&self) -> &'static str {
        "Run a plugin script under the modulex-plugin/1 stdio JSON contract"
    }

    fn data_schema(&self) -> serde_json::Value {
        // Passthrough: the plugin owns its `data` payload (the protocol's
        // `data` field). Empty schema = any JSON.
        serde_json::json!({
            "description": "plugin-defined payload (modulex-plugin/1 `data` field)"
        })
    }

    fn required_programs(&self, spec: &StepSpec) -> Vec<String> {
        vec![spec
            .param_str("interpreter")
            .unwrap_or("python3")
            .to_string()]
    }

    async fn run(&self, spec: &StepSpec, cx: &RunContext) -> StepResult {
        let Some(script) = spec.param_str("script") else {
            return StepResult::fail(
                &spec.name,
                &spec.step_type,
                "missing required param `script`",
            );
        };
        let script_path = expand_tilde(script);
        let interpreter = spec.param_str("interpreter").unwrap_or("python3");

        if cx.dry_run {
            return StepResult::ok(
                &spec.name,
                &spec.step_type,
                format!(
                    "[dry-run] would run: {interpreter} {} ({PROTOCOL})",
                    script_path.display()
                ),
            );
        }
        if !script_path.is_file() {
            return StepResult::skip(
                &spec.name,
                &spec.step_type,
                format!("plugin script not found: {}", script_path.display()),
            );
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

        let payload = request_payload(spec, cx).to_string();
        let request = ExecRequest::new(interpreter)
            .args(vec![script_path.to_string_lossy().into_owned()])
            .env(env)
            .stdin(payload)
            .timeout(Duration::from_secs(spec.timeout));

        let out = match cx.exec.spawn(request).await {
            Ok(out) => out,
            Err(e) => return StepResult::fail(&spec.name, &spec.step_type, e.to_string()),
        };
        if out.timed_out {
            return StepResult::fail(&spec.name, &spec.step_type, out.stderr.trim());
        }
        if !out.success() {
            return StepResult::fail(
                &spec.name,
                &spec.step_type,
                format!(
                    "plugin exited {}: {}",
                    out.status.unwrap_or(-1),
                    stderr_tail(&out.stderr)
                ),
            );
        }
        match serde_json::from_str::<Value>(out.stdout.trim()) {
            Ok(response) => map_response(spec, &response),
            Err(e) => StepResult::fail(
                &spec.name,
                &spec.step_type,
                format!("plugin did not emit valid {PROTOCOL} JSON: {e}"),
            ),
        }
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
            exec: Scope::only(["python3".to_string()]),
            ..Caveats::top()
        };
        let mut config = Config::default();
        config.identity.username = "someone".into();
        config.shared.repos = vec!["~/r".into()];
        (
            RunContext {
                config: Arc::new(config),
                dry_run: false,
                generation: 7,
                exec: gate_with(&granted, spawner.clone()),
                prior: Vec::new(),
                store: None,
            },
            spawner,
        )
    }

    /// A spec whose script points at a real file (the protocol test needs
    /// the is_file() probe to pass; /etc/hostname exists on our targets).
    fn spec() -> StepSpec {
        toml::from_str(
            "name=\"notes\"\ntype=\"python\"\nscript=\"/etc/hostname\"\nextra_param=\"x\"",
        )
        .unwrap()
    }

    #[tokio::test]
    async fn success_response_maps_all_fields() {
        let (cx, spawner) = cx_with(vec![MockSpawner::ok(
            r#"{"protocol":"modulex-plugin/1","success":true,"output":"did things",
                "repo_results":[{"repo":"a","output":"x","success":false,"error":"e"}],
                "data":{"k":1}}"#,
        )]);
        let result = PythonPlugin.run(&spec(), &cx).await;
        assert!(result.success);
        assert_eq!(result.output, "did things");
        assert_eq!(result.repo_results.len(), 1);
        assert!(!result.repo_results[0].success);
        assert_eq!(result.data.unwrap()["k"], 1);
        // python3 invoked with the script path.
        let calls = spawner.calls.lock().unwrap();
        assert_eq!(calls[0].0, "python3");
        assert_eq!(calls[0].1, vec!["/etc/hostname"]);
    }

    #[tokio::test]
    async fn request_payload_carries_protocol_generation_and_params() {
        let (cx, _) = cx_with(vec![]);
        let payload = request_payload(&spec(), &cx);
        assert_eq!(payload["protocol"], PROTOCOL);
        assert_eq!(payload["generation"], 7);
        assert_eq!(payload["step"]["params"]["extra_param"], "x");
        // Declared step fields are NOT duplicated into params.
        assert!(payload["step"]["params"].get("name").is_none());
        assert_eq!(payload["shared"]["identity"]["username"], "someone");
    }

    #[tokio::test]
    async fn nonzero_exit_fails_with_stderr_tail() {
        let (cx, _) = cx_with(vec![MockSpawner::fail(
            "line1\nline2\nline3\nline4\nline5\nTraceback: boom",
            1,
        )]);
        let result = PythonPlugin.run(&spec(), &cx).await;
        assert!(!result.success);
        let err = result.error.unwrap();
        assert!(err.contains("plugin exited 1"));
        assert!(err.contains("Traceback: boom"));
        assert!(!err.contains("line1"), "only the tail: {err}");
    }

    #[tokio::test]
    async fn bad_json_fails_softly() {
        let (cx, _) = cx_with(vec![MockSpawner::ok("not json at all")]);
        let result = PythonPlugin.run(&spec(), &cx).await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("modulex-plugin/1"));
    }

    #[tokio::test]
    async fn skipped_response_passes_through() {
        let (cx, _) = cx_with(vec![MockSpawner::ok(
            r#"{"skipped":true,"output":"nothing to do"}"#,
        )]);
        let result = PythonPlugin.run(&spec(), &cx).await;
        assert!(result.skipped);
        assert!(result.success);
        assert_eq!(result.output, "nothing to do");
    }

    #[tokio::test]
    async fn missing_script_file_soft_skips() {
        let (cx, spawner) = cx_with(vec![]);
        let spec: StepSpec =
            toml::from_str("name=\"n\"\ntype=\"python\"\nscript=\"/nonexistent/plugin.py\"")
                .unwrap();
        let result = PythonPlugin.run(&spec, &cx).await;
        assert!(result.skipped);
        assert!(spawner.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn dry_run_describes_without_spawning() {
        let (mut cx, spawner) = cx_with(vec![]);
        cx.dry_run = true;
        let result = PythonPlugin.run(&spec(), &cx).await;
        assert!(result.output.contains("[dry-run] would run: python3"));
        assert!(spawner.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn custom_interpreter_is_declared_and_used() {
        let spec: StepSpec = toml::from_str(
            "name=\"n\"\ntype=\"python\"\nscript=\"/etc/hostname\"\ninterpreter=\"python3.12\"",
        )
        .unwrap();
        assert_eq!(
            PythonPlugin.required_programs(&spec),
            vec!["python3.12".to_string()]
        );
    }
}
