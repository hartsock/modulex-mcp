//! modulex-core — a deterministic, pluggable routine engine.
//!
//! A *routine* is an ordered, configuration-defined sequence of *steps*
//! (repo health checks, deadline countdowns, external scripts, …) that
//! produces one structured [`report::Report`]. The same routine runs
//! identically from the CLI, from an MCP server, or embedded from Python —
//! determinism is the point.
//!
//! Design pillars:
//!
//! - **Reports are identified by a monotonic generation counter** (a causal
//!   coordinate, never wall-clock).
//! - **Credentials never live in config**: only references (`{env=..}`,
//!   `{file=..}`, `{cmd=..}`), resolved at spawn time into a [`credentials::Secret`]
//!   that is unprintable and unserializable by construction.
//! - **Every subprocess is leashed**: the only spawn path is
//!   [`exec::ExecGate::spawn`], which passes agent-bridle's `check_exec`
//!   before any process exists. The default grant is *deny everything except
//!   the programs the configured steps declare*.
//! - **Soft failures**: a failed or skipped step never aborts the routine;
//!   its status lives inside the report.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod board_md;
pub mod caveats;
pub mod config;
pub mod credentials;
pub mod engine;
pub mod exec;
pub mod registry;
pub mod report;
pub mod step;
pub mod steps;
pub mod store;

pub use board_md::{card_from_markdown, card_to_markdown, BoardMdError};
pub use caveats::{CaveatsSource, GrantedCaveats};
pub use config::{Config, RoutineSpec, StepSpec};
pub use credentials::{CredentialRef, Secret};
pub use engine::{Engine, EngineError, RunOptions};
pub use exec::{ExecGate, ExecOutput, ExecRequest, Spawner, TokioSpawner};
pub use registry::StepRegistry;
pub use report::{RepoResult, Report, StepResult};
pub use step::{RunContext, StepHandler};
pub use store::{McpServer, Store};

// Re-export the leash vocabulary so embedders don't need a direct
// agent-bridle-core dependency to construct grants.
pub use agent_bridle_core::{Caveats, CountBound, Scope};
