//! Credential references — config names *where a secret lives*, never the
//! secret itself.
//!
//! Resolution happens per step run, immediately before spawn. The resolved
//! [`Secret`] is:
//!
//! - **unprintable**: `Debug`/`Display` render `<redacted>`;
//! - **unserializable**: there is deliberately NO `Serialize` impl, so a
//!   secret cannot end up inside a [`crate::report::Report`] — that is a
//!   compile error, not a code-review hope;
//! - **scoped to the spawn**: the value is exposed only to
//!   [`crate::exec::ExecGate::spawn`], which injects it into the child's
//!   environment and scrubs it from captured output.

use std::fmt;

use serde::Deserialize;

use crate::config::expand_tilde;
use crate::exec::ExecGate;

/// Where a credential lives. TOML shapes: `{env = "NAME"}`,
/// `{file = "path"}`, `{cmd = "command line"}`.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum CredentialRef {
    /// Read from this process's environment.
    Env {
        /// Variable name in the modulex process environment.
        env: String,
    },
    /// Read from a file (`~` expanded, surrounding whitespace trimmed).
    File {
        /// File path.
        file: String,
    },
    /// Run a command (argv0 exec-gated) and take its trimmed stdout.
    Cmd {
        /// Command line, split shell-words style.
        cmd: String,
    },
}

/// Errors from resolving a [`CredentialRef`].
#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    /// The named environment variable is unset.
    #[error("credential env var {0:?} is not set")]
    MissingEnv(String),
    /// The credential file is unreadable.
    #[error("credential file {0:?}: {1}")]
    UnreadableFile(String, String),
    /// The credential command failed or was denied by the leash.
    #[error("credential command {0:?}: {1}")]
    CommandFailed(String, String),
}

/// A resolved secret value. See the module docs for the guarantees.
///
/// `Secret` must never become serializable — that is what keeps it out of
/// reports. The following is required NOT to compile:
///
/// ```compile_fail
/// fn requires_serialize<T: serde::Serialize>() {}
/// requires_serialize::<modulex_core::Secret>();
/// ```
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    /// Wrap a raw value. Crate-public so handlers/tests can build one, but
    /// the only consumer of the inner value is the exec gate.
    #[must_use]
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// The raw value — for injection into a child process environment only.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl CredentialRef {
    /// The (tilde-expanded) program a `Cmd` reference will spawn, if any.
    /// Feeds [`crate::config::Config::declared_programs`] so the declared
    /// default grant covers credential commands too.
    #[must_use]
    pub fn declared_program(&self) -> Option<String> {
        let Self::Cmd { cmd } = self else {
            return None;
        };
        let words = shell_words::split(cmd).ok()?;
        let program = words.first()?;
        Some(expand_tilde(program).to_string_lossy().into_owned())
    }

    /// Resolve to a [`Secret`]. `Cmd` references spawn through `exec`, so
    /// they are subject to the same leash as every other subprocess.
    ///
    /// # Errors
    /// See [`CredentialError`].
    pub async fn resolve(&self, exec: &ExecGate) -> Result<Secret, CredentialError> {
        match self {
            Self::Env { env } => std::env::var(env)
                .map(Secret::new)
                .map_err(|_| CredentialError::MissingEnv(env.clone())),
            Self::File { file } => {
                let path = expand_tilde(file);
                std::fs::read_to_string(&path)
                    .map(|s| Secret::new(s.trim().to_string()))
                    .map_err(|e| CredentialError::UnreadableFile(file.clone(), e.to_string()))
            }
            Self::Cmd { cmd } => {
                let words = shell_words::split(cmd)
                    .map_err(|e| CredentialError::CommandFailed(cmd.clone(), e.to_string()))?;
                let Some((program, args)) = words.split_first() else {
                    return Err(CredentialError::CommandFailed(
                        cmd.clone(),
                        "empty command".into(),
                    ));
                };
                // Same tilde expansion as declared_program(), so the leash
                // compares like with like.
                let program = expand_tilde(program).to_string_lossy().into_owned();
                let out = exec
                    .spawn(crate::exec::ExecRequest::new(program).args(args.to_vec()))
                    .await
                    .map_err(|e| CredentialError::CommandFailed(cmd.clone(), e.to_string()))?;
                if !out.success() {
                    return Err(CredentialError::CommandFailed(
                        cmd.clone(),
                        // stderr may carry diagnostics; stdout may carry the
                        // secret — only stderr is safe to surface.
                        out.stderr.trim().to_string(),
                    ));
                }
                Ok(Secret::new(out.stdout.trim().to_string()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_debug_and_display_are_redacted() {
        let s = Secret::new("hunter2".into());
        assert_eq!(format!("{s:?}"), "<redacted>");
        assert_eq!(format!("{s}"), "<redacted>");
        assert_eq!(s.expose(), "hunter2");
    }

    #[test]
    fn declared_program_extracts_cmd_argv0_only() {
        // Regression (fresh-eyes 2026-06-05): Cmd credential programs were
        // missing from the declared default grant, so every {cmd=..}
        // reference was denied by the leash and its step silently skipped.
        let cmd = CredentialRef::Cmd {
            cmd: "pass show gitlab/token".into(),
        };
        assert_eq!(cmd.declared_program().as_deref(), Some("pass"));

        let env = CredentialRef::Env { env: "X".into() };
        assert_eq!(env.declared_program(), None);
        let file = CredentialRef::File {
            file: "~/.k".into(),
        };
        assert_eq!(file.declared_program(), None);
    }

    #[tokio::test]
    async fn cmd_resolution_is_leash_gated_and_uses_stdout() {
        use std::sync::Arc;

        use agent_bridle_core::{Caveats, Scope};

        use crate::exec::test_support::{gate_with, MockSpawner};

        let spawner = Arc::new(MockSpawner::with_outputs(vec![MockSpawner::ok(
            " tok-123 \n",
        )]));
        let granted = Caveats {
            exec: Scope::only(["pass".to_string()]),
            ..Caveats::top()
        };
        let gate = gate_with(&granted, spawner);

        let secret = CredentialRef::Cmd {
            cmd: "pass show gitlab/token".into(),
        }
        .resolve(&gate)
        .await
        .expect("granted command resolves");
        assert_eq!(secret.expose(), "tok-123");

        // An ungranted credential command is denied by the leash.
        let denied = CredentialRef::Cmd {
            cmd: "vault kv get x".into(),
        }
        .resolve(&gate)
        .await
        .unwrap_err();
        assert!(denied.to_string().contains("vault"));
    }

    // The "Secret is not serializable" guarantee is enforced by the
    // `compile_fail` doctest on the `Secret` type itself.
}
