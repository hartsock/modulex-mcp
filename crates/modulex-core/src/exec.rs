//! The single subprocess seam — every process modulex ever spawns goes
//! through [`ExecGate::spawn`].
//!
//! The gate does three things, in order:
//!
//! 1. **Leash check**: `ToolContext::check_exec(program)` (agent-bridle)
//!    BEFORE any process exists. Out-of-grant programs are denied here.
//! 2. **Spawn with timeout**: via a [`Spawner`] (the seam tests mock), with
//!    resolved [`Secret`]s injected into the child environment only.
//! 3. **Scrub**: any resolved secret value appearing in captured
//!    stdout/stderr is replaced with `***` before the output can reach a
//!    report.
//!
//! Adding a raw `Command::new` anywhere else in this workspace is a
//! review-blocking violation (see CLAUDE.md).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agent_bridle_core::ToolContext;
use async_trait::async_trait;

use crate::credentials::Secret;

/// A subprocess request. Build with [`ExecRequest::new`] and the chained
/// setters.
#[derive(Debug)]
pub struct ExecRequest {
    /// Program to run (bare name or path; the leash checks both forms).
    pub program: String,
    /// Arguments.
    pub args: Vec<String>,
    /// Working directory.
    pub cwd: Option<PathBuf>,
    /// Environment to inject (resolved secrets).
    pub env: Vec<(String, Secret)>,
    /// Optional stdin payload (the plugin protocol writes one JSON object).
    pub stdin: Option<String>,
    /// Kill the child after this long.
    pub timeout: Duration,
}

impl ExecRequest {
    /// A request for `program` with a 30s default timeout.
    #[must_use]
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            stdin: None,
            timeout: Duration::from_secs(30),
        }
    }

    /// Set arguments.
    #[must_use]
    pub fn args(mut self, args: Vec<String>) -> Self {
        self.args = args;
        self
    }

    /// Set the working directory.
    #[must_use]
    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Inject resolved secrets into the child environment.
    #[must_use]
    pub fn env(mut self, env: Vec<(String, Secret)>) -> Self {
        self.env = env;
        self
    }

    /// Provide a stdin payload.
    #[must_use]
    pub fn stdin(mut self, stdin: impl Into<String>) -> Self {
        self.stdin = Some(stdin.into());
        self
    }

    /// Set the timeout.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// Captured subprocess output, post-scrub.
#[derive(Clone, Debug, Default)]
pub struct ExecOutput {
    /// Captured stdout (secrets scrubbed).
    pub stdout: String,
    /// Captured stderr (secrets scrubbed).
    pub stderr: String,
    /// Exit code, if the process exited normally.
    pub status: Option<i32>,
    /// True when the process was killed at the timeout.
    pub timed_out: bool,
}

impl ExecOutput {
    /// Exited normally with code 0.
    #[must_use]
    pub fn success(&self) -> bool {
        self.status == Some(0) && !self.timed_out
    }
}

/// Errors from [`ExecGate::spawn`].
#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    /// The leash denied the program before spawn.
    #[error("{0}")]
    Denied(String),
    /// The OS failed to spawn or run the process.
    #[error("spawn of {program:?} failed: {source}")]
    Io {
        /// Program that failed to spawn.
        program: String,
        /// Underlying error.
        source: std::io::Error,
    },
}

/// The mockable spawn seam. Production uses [`TokioSpawner`]; tests inject
/// canned outputs (house rule: never spawn real processes in unit tests).
#[async_trait]
pub trait Spawner: Send + Sync {
    /// Run the request to completion (or timeout) and capture output.
    async fn spawn(&self, req: &ExecRequest) -> std::io::Result<ExecOutput>;
}

/// Real subprocess execution on tokio.
#[derive(Clone, Debug, Default)]
pub struct TokioSpawner;

#[async_trait]
impl Spawner for TokioSpawner {
    async fn spawn(&self, req: &ExecRequest) -> std::io::Result<ExecOutput> {
        use tokio::io::AsyncWriteExt;

        let mut cmd = tokio::process::Command::new(&req.program);
        cmd.args(&req.args)
            .stdin(if req.stdin.is_some() {
                std::process::Stdio::piped()
            } else {
                std::process::Stdio::null()
            })
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = &req.cwd {
            cmd.current_dir(cwd);
        }
        for (key, secret) in &req.env {
            cmd.env(key, secret.expose());
        }

        let mut child = cmd.spawn()?;
        if let (Some(payload), Some(mut stdin)) = (&req.stdin, child.stdin.take()) {
            stdin.write_all(payload.as_bytes()).await?;
            drop(stdin); // close, so line-readers see EOF
        }

        match tokio::time::timeout(req.timeout, child.wait_with_output()).await {
            Ok(out) => {
                let out = out?;
                Ok(ExecOutput {
                    stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
                    status: out.status.code(),
                    timed_out: false,
                })
            }
            Err(_) => Ok(ExecOutput {
                stdout: String::new(),
                stderr: format!(
                    "timed out after {}s: {}",
                    req.timeout.as_secs(),
                    req.program
                ),
                status: None,
                timed_out: true,
            }),
        }
    }
}

/// The leash-enforcing spawn handle carried by every [`crate::step::RunContext`].
///
/// Cloneable: parallel steps share one gate. The [`ToolContext`] inside was
/// minted by `agent_bridle_core::Gate::authorize` for this routine run's
/// generation — there is no other way to construct one.
#[derive(Clone)]
pub struct ExecGate {
    cx: ToolContext,
    spawner: Arc<dyn Spawner>,
}

impl ExecGate {
    /// Wrap an authorized context and a spawner.
    #[must_use]
    pub fn new(cx: ToolContext, spawner: Arc<dyn Spawner>) -> Self {
        Self { cx, spawner }
    }

    /// Leash-check, spawn, scrub. The ONLY subprocess path in modulex.
    ///
    /// # Errors
    /// [`ExecError::Denied`] when the leash rejects the program;
    /// [`ExecError::Io`] when the OS spawn fails.
    pub async fn spawn(&self, req: ExecRequest) -> Result<ExecOutput, ExecError> {
        self.cx
            .check_exec(&req.program)
            .map_err(|e| ExecError::Denied(e.to_string()))?;

        let mut out = self
            .spawner
            .spawn(&req)
            .await
            .map_err(|source| ExecError::Io {
                program: req.program.clone(),
                source,
            })?;

        // Defense-in-depth: scrub any injected secret value from captured
        // output before it can reach a report. (The primary guarantee is
        // type-level: Secret is unserializable.)
        for (_, secret) in &req.env {
            let value = secret.expose();
            if !value.is_empty() {
                out.stdout = out.stdout.replace(value, "***");
                out.stderr = out.stderr.replace(value, "***");
            }
        }
        Ok(out)
    }
}

/// Is `program` resolvable: an absolute/`~` path that exists, or a bare name
/// found on `$PATH`? Drives the engine's soft-skip probe — a missing tool
/// skips its step instead of failing the routine.
#[must_use]
pub fn program_available(program: &str) -> bool {
    let expanded = crate::config::expand_tilde(program);
    if expanded.components().count() > 1 {
        return expanded.is_file();
    }
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(program).is_file())
}

#[cfg(test)]
pub(crate) mod test_support {
    //! A canned-output spawner for unit tests, plus a gate factory.

    use std::collections::VecDeque;
    use std::sync::Mutex;

    use agent_bridle_core::{Caveats, Gate, Tool, ToolResult};

    use super::*;

    /// Returns canned outputs in order; records every request's program+args.
    #[derive(Default)]
    pub struct MockSpawner {
        outputs: Mutex<VecDeque<ExecOutput>>,
        /// Recorded `(program, args)` per call, for assertions.
        pub calls: Mutex<Vec<(String, Vec<String>)>>,
    }

    impl MockSpawner {
        pub fn with_outputs(outputs: Vec<ExecOutput>) -> Self {
            Self {
                outputs: Mutex::new(outputs.into()),
                calls: Mutex::new(Vec::new()),
            }
        }

        pub fn ok(stdout: &str) -> ExecOutput {
            ExecOutput {
                stdout: stdout.to_string(),
                status: Some(0),
                ..ExecOutput::default()
            }
        }

        pub fn fail(stderr: &str, code: i32) -> ExecOutput {
            ExecOutput {
                stderr: stderr.to_string(),
                status: Some(code),
                ..ExecOutput::default()
            }
        }
    }

    #[async_trait]
    impl Spawner for MockSpawner {
        async fn spawn(&self, req: &ExecRequest) -> std::io::Result<ExecOutput> {
            self.calls
                .lock()
                .unwrap()
                .push((req.program.clone(), req.args.clone()));
            Ok(self
                .outputs
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| MockSpawner::ok("")))
        }
    }

    struct AnyTool;
    #[async_trait]
    impl Tool for AnyTool {
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

    /// Mint a gate the only legitimate way, over the given spawner.
    pub fn gate_with(granted: &Caveats, spawner: Arc<dyn Spawner>) -> ExecGate {
        let gate = Gate::new(0);
        let cx = gate.authorize(&AnyTool, granted).expect("authorize");
        ExecGate::new(cx, spawner)
    }
}

#[cfg(test)]
mod tests {
    use agent_bridle_core::{Caveats, Scope};

    use super::test_support::{gate_with, MockSpawner};
    use super::*;

    fn granted_only(programs: &[&str]) -> Caveats {
        Caveats {
            exec: Scope::only(programs.iter().map(ToString::to_string)),
            ..Caveats::top()
        }
    }

    #[tokio::test]
    async fn denies_ungranted_program_before_spawn() {
        let spawner = Arc::new(MockSpawner::default());
        let gate = gate_with(&granted_only(&["git"]), spawner.clone());

        let err = gate.spawn(ExecRequest::new("rm")).await.unwrap_err();
        assert!(matches!(err, ExecError::Denied(_)));
        // The mock was never reached — denial happens BEFORE spawn.
        assert!(spawner.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn allows_granted_program_and_passes_args() {
        let spawner = Arc::new(MockSpawner::with_outputs(vec![MockSpawner::ok("hello")]));
        let gate = gate_with(&granted_only(&["git"]), spawner.clone());

        let out = gate
            .spawn(ExecRequest::new("git").args(vec!["status".into()]))
            .await
            .unwrap();
        assert!(out.success());
        assert_eq!(out.stdout, "hello");
        assert_eq!(
            spawner.calls.lock().unwrap()[0],
            ("git".to_string(), vec!["status".to_string()])
        );
    }

    #[tokio::test]
    async fn scrubs_secret_values_from_captured_output() {
        let spawner = Arc::new(MockSpawner::with_outputs(vec![ExecOutput {
            stdout: "token=s3cr3t-value done".into(),
            stderr: "warn: s3cr3t-value expired".into(),
            status: Some(0),
            timed_out: false,
        }]));
        let gate = gate_with(&granted_only(&["curl"]), spawner);

        let out = gate
            .spawn(
                ExecRequest::new("curl")
                    .env(vec![("TOKEN".into(), Secret::new("s3cr3t-value".into()))]),
            )
            .await
            .unwrap();
        assert_eq!(out.stdout, "token=*** done");
        assert_eq!(out.stderr, "warn: *** expired");
    }

    #[test]
    fn program_available_finds_path_binaries_and_rejects_missing() {
        // `sh` exists on every CI/dev box we target.
        assert!(program_available("sh"));
        assert!(!program_available("definitely-not-a-real-binary-xyzzy"));
        assert!(!program_available("/nonexistent/absolute/path"));
    }
}
