//! The ONE test allowed to spawn real subprocesses: proves the leash
//! end-to-end against a live `echo`, and proves denial for an ungranted
//! program. Everything else mocks the spawner (house rule).

use std::sync::Arc;

use agent_bridle_core::{Caveats, Gate, Scope, Tool, ToolContext, ToolResult};
use modulex_core::{ExecGate, ExecRequest, Secret, TokioSpawner};

struct TestTool;

#[async_trait::async_trait]
impl Tool for TestTool {
    fn name(&self) -> &str {
        "test"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    async fn invoke(
        &self,
        _args: serde_json::Value,
        _cx: &ToolContext,
    ) -> ToolResult<serde_json::Value> {
        Ok(serde_json::Value::Null)
    }
}

fn gate(allow: &[&str]) -> ExecGate {
    let granted = Caveats {
        exec: Scope::only(allow.iter().map(ToString::to_string)),
        ..Caveats::top()
    };
    let cx = Gate::new(1)
        .authorize(&TestTool, &granted)
        .expect("authorize");
    ExecGate::new(cx, Arc::new(TokioSpawner))
}

#[tokio::test]
async fn granted_echo_runs_and_captures_stdout() {
    let out = gate(&["echo"])
        .spawn(ExecRequest::new("echo").args(vec!["hello".into(), "leash".into()]))
        .await
        .expect("echo is granted");
    assert!(out.success());
    assert_eq!(out.stdout.trim(), "hello leash");
}

#[tokio::test]
async fn ungranted_program_is_denied_before_spawn() {
    let err = gate(&["echo"])
        .spawn(ExecRequest::new("sh").args(vec!["-c".into(), "true".into()]))
        .await
        .expect_err("sh is not granted");
    assert!(err.to_string().contains("granted authority"), "got: {err}");
}

#[tokio::test]
async fn injected_secret_reaches_child_env_and_is_scrubbed_from_output() {
    // `sh -c 'echo $TOKEN'` prints the secret; the gate must scrub it.
    let out = gate(&["sh"])
        .spawn(
            ExecRequest::new("sh")
                .args(vec!["-c".into(), "echo token=$MODULEX_TEST_TOKEN".into()])
                .env(vec![(
                    "MODULEX_TEST_TOKEN".into(),
                    Secret::new("super-sekrit".into()),
                )]),
        )
        .await
        .expect("sh is granted");
    assert!(out.success());
    assert_eq!(out.stdout.trim(), "token=***", "secret must be scrubbed");
}

#[tokio::test]
async fn timeout_kills_and_reports() {
    let out = gate(&["sleep"])
        .spawn(
            ExecRequest::new("sleep")
                .args(vec!["5".into()])
                .timeout(std::time::Duration::from_millis(200)),
        )
        .await
        .expect("spawn ok");
    assert!(out.timed_out);
    assert!(!out.success());
    assert!(out.stderr.contains("timed out"));
}
