//! External-command steps: `script` (text contract) and `harness`
//! (JSON-on-stdout contract).
//!
//! `harness` is for AI-harness tools and other agents-in-a-box (e.g. a `pa`
//! enclave): pass flags, inject credentials via env references, expect one
//! JSON object on stdout. A `text`, `summary`, or `output` key becomes the
//! report body; the full object rides along as structured `data`.

use std::time::Duration;

use async_trait::async_trait;

use crate::config::{expand_tilde, StepSpec};
use crate::exec::{ExecError, ExecOutput, ExecRequest};
use crate::report::StepResult;
use crate::step::{resolve_step_env, RunContext, StepHandler};

/// The configured command with `~` expanded — used for BOTH the declared
/// grant ([`StepHandler::required_programs`]) and the spawn request, so the
/// leash compares like with like.
fn command_of(spec: &StepSpec) -> Option<String> {
    spec.param_str("command")
        .map(|c| expand_tilde(c).to_string_lossy().into_owned())
}

/// Build and run the configured command; shared by both step types.
async fn run_command(spec: &StepSpec, cx: &RunContext) -> Result<ExecOutput, StepResult> {
    let Some(command) = command_of(spec) else {
        return Err(StepResult::fail(
            &spec.name,
            &spec.step_type,
            "missing required param `command`",
        ));
    };
    let args = spec.param_str_list("args");

    let env = match resolve_step_env(spec, &cx.exec).await {
        Ok(env) => env,
        Err((name, error)) => {
            // A missing credential is a soft skip — the step cannot run, the
            // routine continues. The error names the variable, never a value.
            return Err(StepResult::skip(
                &spec.name,
                &spec.step_type,
                format!("credential {name} unavailable: {error}"),
            ));
        }
    };

    let request = ExecRequest::new(command)
        .args(args)
        .env(env)
        .timeout(Duration::from_secs(spec.timeout));

    match cx.exec.spawn(request).await {
        Ok(out) => Ok(out),
        Err(e @ ExecError::Denied(_)) => {
            Err(StepResult::fail(&spec.name, &spec.step_type, e.to_string()))
        }
        Err(e) => Err(StepResult::fail(&spec.name, &spec.step_type, e.to_string())),
    }
}

fn describe(spec: &StepSpec) -> String {
    let command = spec.param_str("command").unwrap_or("<missing command>");
    let args = spec.param_str_list("args").join(" ");
    if args.is_empty() {
        format!("[dry-run] would run: {command}")
    } else {
        format!("[dry-run] would run: {command} {args}")
    }
}

/// `script`: run a command, report trimmed stdout (or stderr on failure).
pub struct Script;

#[async_trait]
impl StepHandler for Script {
    fn type_name(&self) -> &'static str {
        "script"
    }

    fn required_programs(&self, spec: &StepSpec) -> Vec<String> {
        command_of(spec).into_iter().collect()
    }

    async fn run(&self, spec: &StepSpec, cx: &RunContext) -> StepResult {
        if cx.dry_run {
            return StepResult::ok(&spec.name, &spec.step_type, describe(spec));
        }
        let out = match run_command(spec, cx).await {
            Ok(out) => out,
            Err(result) => return result,
        };
        if out.timed_out {
            return StepResult::fail(&spec.name, &spec.step_type, out.stderr.trim());
        }
        if out.success() {
            StepResult::ok(&spec.name, &spec.step_type, out.stdout.trim())
        } else {
            let mut result = StepResult::fail(
                &spec.name,
                &spec.step_type,
                if out.stderr.trim().is_empty() {
                    format!("exit code {}", out.status.unwrap_or(-1))
                } else {
                    out.stderr.trim().to_string()
                },
            );
            // Keep whatever stdout the failing script produced — often the
            // useful half of a diagnostic.
            result.output = out.stdout.trim().to_string();
            result
        }
    }
}

/// `harness`: run a command whose contract is one JSON object on stdout.
pub struct Harness;

#[async_trait]
impl StepHandler for Harness {
    fn type_name(&self) -> &'static str {
        "harness"
    }

    fn required_programs(&self, spec: &StepSpec) -> Vec<String> {
        command_of(spec).into_iter().collect()
    }

    async fn run(&self, spec: &StepSpec, cx: &RunContext) -> StepResult {
        if cx.dry_run {
            return StepResult::ok(
                &spec.name,
                &spec.step_type,
                format!("{} (expecting JSON on stdout)", describe(spec)),
            );
        }
        let out = match run_command(spec, cx).await {
            Ok(out) => out,
            Err(result) => return result,
        };
        if !out.success() {
            return StepResult::fail(
                &spec.name,
                &spec.step_type,
                if out.timed_out || !out.stderr.trim().is_empty() {
                    out.stderr.trim().to_string()
                } else {
                    format!("exit code {}", out.status.unwrap_or(-1))
                },
            );
        }

        let payload: serde_json::Value = match serde_json::from_str(out.stdout.trim()) {
            Ok(value) => value,
            Err(e) => {
                return StepResult::fail(
                    &spec.name,
                    &spec.step_type,
                    format!("harness did not emit valid JSON: {e}"),
                )
            }
        };

        // A text-ish key becomes the report body; otherwise compact JSON.
        let body = ["text", "summary", "output"]
            .iter()
            .find_map(|key| payload.get(*key).and_then(|v| v.as_str()))
            .map(ToString::to_string)
            .unwrap_or_else(|| payload.to_string());

        let mut result = StepResult::ok(&spec.name, &spec.step_type, body);
        result.data = Some(payload);
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

    fn cx(outputs: Vec<ExecOutput>, allow: &[&str]) -> (RunContext, Arc<MockSpawner>) {
        let spawner = Arc::new(MockSpawner::with_outputs(outputs));
        let granted = Caveats {
            exec: Scope::only(allow.iter().map(ToString::to_string)),
            ..Caveats::top()
        };
        (
            RunContext {
                config: Arc::new(Config::default()),
                dry_run: false,
                generation: 1,
                exec: gate_with(&granted, spawner.clone()),
                prior: Vec::new(),
            },
            spawner,
        )
    }

    fn spec(toml_text: &str) -> StepSpec {
        toml::from_str(toml_text).unwrap()
    }

    #[tokio::test]
    async fn script_success_reports_trimmed_stdout() {
        let (cx, spawner) = cx(vec![MockSpawner::ok("  sunny 21C \n")], &["weather"]);
        let result = Script
            .run(
                &spec("name=\"w\"\ntype=\"script\"\ncommand=\"weather\"\nargs=[\"--brief\"]"),
                &cx,
            )
            .await;
        assert!(result.success);
        assert_eq!(result.output, "sunny 21C");
        assert_eq!(
            spawner.calls.lock().unwrap()[0],
            ("weather".to_string(), vec!["--brief".to_string()])
        );
    }

    #[tokio::test]
    async fn script_failure_keeps_stdout_and_reports_stderr() {
        let (cx, _) = cx(
            vec![ExecOutput {
                stdout: "partial".into(),
                stderr: "boom".into(),
                status: Some(2),
                timed_out: false,
            }],
            &["tool"],
        );
        let result = Script
            .run(&spec("name=\"t\"\ntype=\"script\"\ncommand=\"tool\""), &cx)
            .await;
        assert!(!result.success);
        assert_eq!(result.error.as_deref(), Some("boom"));
        assert_eq!(result.output, "partial");
    }

    #[tokio::test]
    async fn script_denied_program_fails_with_leash_reason() {
        let (cx, spawner) = cx(vec![], &["allowed-only"]);
        let result = Script
            .run(
                &spec("name=\"t\"\ntype=\"script\"\ncommand=\"forbidden\""),
                &cx,
            )
            .await;
        assert!(!result.success);
        assert!(result
            .error
            .as_deref()
            .unwrap()
            .contains("granted authority"));
        assert!(spawner.calls.lock().unwrap().is_empty(), "never spawned");
    }

    #[tokio::test]
    async fn script_missing_command_param_fails() {
        let (cx, _) = cx(vec![], &[]);
        let result = Script.run(&spec("name=\"t\"\ntype=\"script\""), &cx).await;
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap().contains("command"));
    }

    #[tokio::test]
    async fn missing_credential_soft_skips() {
        let (cx, spawner) = cx(vec![], &["tool"]);
        let result = Script
            .run(
                &spec(
                    "name=\"t\"\ntype=\"script\"\ncommand=\"tool\"\n\
                     env = { TOKEN = { env = \"MODULEX_TEST_UNSET_VAR_XYZZY\" } }",
                ),
                &cx,
            )
            .await;
        assert!(result.skipped, "missing credential is a soft skip");
        assert!(result.output.contains("TOKEN"));
        assert!(spawner.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn harness_parses_json_and_extracts_text_body() {
        let (cx, _) = cx(
            vec![MockSpawner::ok(
                r#"{"summary":"3 meetings today","count":3}"#,
            )],
            &["pa"],
        );
        let result = Harness
            .run(&spec("name=\"pa\"\ntype=\"harness\"\ncommand=\"pa\""), &cx)
            .await;
        assert!(result.success);
        assert_eq!(result.output, "3 meetings today");
        assert_eq!(result.data.unwrap()["count"], 3);
    }

    #[tokio::test]
    async fn harness_without_text_key_uses_compact_json_body() {
        let (cx, _) = cx(vec![MockSpawner::ok(r#"{"items":[1,2]}"#)], &["pa"]);
        let result = Harness
            .run(&spec("name=\"pa\"\ntype=\"harness\"\ncommand=\"pa\""), &cx)
            .await;
        assert!(result.success);
        assert_eq!(result.output, r#"{"items":[1,2]}"#);
    }

    #[tokio::test]
    async fn harness_rejects_non_json_output() {
        let (cx, _) = cx(vec![MockSpawner::ok("plain text, not json")], &["pa"]);
        let result = Harness
            .run(&spec("name=\"pa\"\ntype=\"harness\"\ncommand=\"pa\""), &cx)
            .await;
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap().contains("valid JSON"));
    }

    #[tokio::test]
    async fn dry_run_describes_without_spawning() {
        let (mut cx_value, spawner) = cx(vec![], &[]);
        cx_value.dry_run = true;
        let result = Harness
            .run(
                &spec("name=\"pa\"\ntype=\"harness\"\ncommand=\"pa\"\nargs=[\"briefing\"]"),
                &cx_value,
            )
            .await;
        assert!(result.output.contains("[dry-run] would run: pa briefing"));
        assert!(spawner.calls.lock().unwrap().is_empty());
    }
}
