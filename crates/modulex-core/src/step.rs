//! The step contract: [`StepHandler`] + [`RunContext`].

use std::sync::Arc;

use async_trait::async_trait;

use crate::config::{Config, StepSpec};
use crate::exec::ExecGate;
use crate::report::StepResult;

/// Everything a step needs at run time. Cloneable so parallel batches can
/// hand each task its own context.
#[derive(Clone)]
pub struct RunContext {
    /// The full configuration (shared repos, deadlines, identity, …).
    pub config: Arc<Config>,
    /// Dry-run: describe, don't act.
    pub dry_run: bool,
    /// This run's generation (causal coordinate of the report).
    pub generation: u64,
    /// The leash-enforcing spawn handle — the only way to run a subprocess.
    pub exec: ExecGate,
    /// Results of steps that completed earlier in this run, in config order.
    /// Derived steps (e.g. an SLA check over a review-queue step) read these.
    pub prior: Vec<StepResult>,
    /// The agent state store, when available. Store-backed steps soft-skip
    /// without it.
    pub store: Option<std::sync::Arc<crate::store::Store>>,
}

/// A step implementation, registered in a [`crate::registry::StepRegistry`]
/// under [`Self::type_name`].
///
/// ## The data contract (FOUNDATION pillar A)
///
/// Reports serve humans AND agents: the markdown `output` is for the human;
/// [`Self::data_schema`] describes the typed `StepResult::data` payload an
/// executed step emits for agents. **Agents never parse prose.** Schemas are
/// versioned contracts — they are pinned by the golden-schema regression
/// harness, and breaking a shape is a breaking release. Dry-run and skipped
/// results may omit `data`; executed results MUST match the schema.
#[async_trait]
pub trait StepHandler: Send + Sync {
    /// The registry key, e.g. `"git-status"`.
    fn type_name(&self) -> &'static str;

    /// One-line human description, surfaced by `steps_list`.
    fn description(&self) -> &'static str;

    /// JSON Schema for this step's `StepResult::data` payload (executed,
    /// non-skipped results). Passthrough steps (external tools/plugins that
    /// own their payload) return a permissive schema and say so in the
    /// description.
    fn data_schema(&self) -> serde_json::Value;

    /// The external programs this step will spawn for `spec` (e.g. `["git"]`).
    /// Drives the declared-default exec grant and the engine's soft-skip
    /// probe. Pure steps return an empty list.
    fn required_programs(&self, spec: &StepSpec) -> Vec<String>;

    /// Execute the step. Step-level failure is encoded in the returned
    /// [`StepResult`] (`success: false` / `skipped: true`) — handlers do not
    /// return `Err`; routine execution never aborts on a step.
    async fn run(&self, spec: &StepSpec, cx: &RunContext) -> StepResult;
}

/// Shared helper: the repo list for a fan-out step — the step's own `repos`
/// override, else the shared list.
#[must_use]
pub fn repos_for(spec: &StepSpec, config: &Config) -> Vec<String> {
    if spec.repos.is_empty() {
        config.shared.repos.clone()
    } else {
        spec.repos.clone()
    }
}

/// Shared helper: resolve a spec's credential references into env pairs for
/// an [`crate::exec::ExecRequest`]. Failures name the variable but never the
/// value.
///
/// # Errors
/// Returns the first credential that fails to resolve, as
/// `(env_name, error_text)`.
pub async fn resolve_step_env(
    spec: &StepSpec,
    exec: &ExecGate,
) -> Result<Vec<(String, crate::credentials::Secret)>, (String, String)> {
    let mut pairs = Vec::with_capacity(spec.env.len());
    for (name, reference) in &spec.env {
        match reference.resolve(exec).await {
            Ok(secret) => pairs.push((name.clone(), secret)),
            Err(e) => return Err((name.clone(), e.to_string())),
        }
    }
    Ok(pairs)
}
