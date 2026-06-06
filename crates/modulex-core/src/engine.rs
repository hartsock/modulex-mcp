//! The engine: routine orchestration over the step registry.
//!
//! Determinism contract:
//!
//! - steps execute in config order; *adjacent* `parallel = true` steps run as
//!   one concurrent batch whose results are re-ordered back to config order;
//! - each run gets the next **generation** (monotonic counter — the report's
//!   identity, never a timestamp);
//! - one leash authorization per run: `Gate::new(generation)` →
//!   `authorize(...)` → an [`ExecGate`] shared by every step in the run;
//! - per-step failure is soft: it marks the report, never aborts the routine.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use agent_bridle_core::{Caveats, Gate, Tool, ToolContext, ToolResult};
use async_trait::async_trait;

use crate::config::{Config, StepSpec};
use crate::exec::{program_available, ExecGate, Spawner, TokioSpawner};
use crate::registry::StepRegistry;
use crate::report::{Report, StepResult};
use crate::step::RunContext;

/// How many reports the engine retains, newest last.
const REPORT_RETENTION: usize = 16;

/// Engine-level errors. Step-level failures are NOT errors — they live
/// inside the report.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// No routine with that name in the config.
    #[error("unknown routine {0:?}; configured: [{1}]")]
    UnknownRoutine(String, String),
    /// No step with that name in the routine.
    #[error("routine {routine:?} has no step named {step:?}")]
    UnknownStep {
        /// Routine searched.
        routine: String,
        /// Step name requested.
        step: String,
    },
    /// The leash denied the run outright (budget, generation, …).
    #[error("run denied by leash: {0}")]
    Denied(String),
}

/// Filters and flags for one run.
#[derive(Clone, Debug, Default)]
pub struct RunOptions {
    /// When non-empty, run only steps with these names.
    pub only: Vec<String>,
    /// Skip steps with these names.
    pub skip: Vec<String>,
    /// Describe, don't act.
    pub dry_run: bool,
}

/// The engine's exec capability, declared as a bridle [`Tool`] so the gate
/// can mint our [`ToolContext`]. `required()` stays `top()` — "confine me
/// entirely by the grant" — because the grant itself already encodes
/// deny-all-except-declared. Never dispatched as an MCP tool.
struct EngineExecTool;

#[async_trait]
impl Tool for EngineExecTool {
    fn name(&self) -> &str {
        "modulex-exec"
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

/// The routine engine. One per process; cheap to share behind an `Arc`.
pub struct Engine {
    config: Arc<Config>,
    registry: StepRegistry,
    granted: Caveats,
    spawner: Arc<dyn Spawner>,
    generation: AtomicU64,
    reports: Mutex<VecDeque<Report>>,
    store: Option<Arc<crate::store::Store>>,
}

impl Engine {
    /// An engine over the given config, registry, and grant, spawning real
    /// processes. Opens (creating if needed) the agent state store at the
    /// configured path; on failure the engine still runs, store-backed
    /// steps soft-skip, and a warning goes to stderr.
    #[must_use]
    pub fn new(config: Config, registry: StepRegistry, granted: Caveats) -> Self {
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let path = crate::store::Store::resolve_path(
            (!config.store.path.is_empty()).then_some(config.store.path.as_str()),
            home.as_deref(),
        );
        let store = match crate::store::Store::open(&path) {
            Ok(store) => Some(Arc::new(store)),
            Err(e) => {
                eprintln!(
                    "modulex: agent state store unavailable ({e}) — store-backed steps will skip"
                );
                None
            }
        };
        Self::with_spawner(config, registry, granted, Arc::new(TokioSpawner)).with_store_opt(store)
    }

    /// As [`Engine::new`] with an injected [`Spawner`] and NO store (tests).
    #[must_use]
    pub fn with_spawner(
        config: Config,
        registry: StepRegistry,
        granted: Caveats,
        spawner: Arc<dyn Spawner>,
    ) -> Self {
        Self {
            config: Arc::new(config),
            registry,
            granted,
            spawner,
            generation: AtomicU64::new(0),
            reports: Mutex::new(VecDeque::new()),
            store: None,
        }
    }

    /// Attach an agent state store (builder). Seeds the generation counter
    /// from the store's persisted value, so generations stay monotonic
    /// across process restarts.
    #[must_use]
    pub fn with_store(self, store: Arc<crate::store::Store>) -> Self {
        self.with_store_opt(Some(store))
    }

    fn with_store_opt(mut self, store: Option<Arc<crate::store::Store>>) -> Self {
        if let Some(store) = &store {
            self.generation
                .store(store.last_generation(), Ordering::Release);
        }
        self.store = store;
        self
    }

    /// The agent state store, when available.
    #[must_use]
    pub fn store(&self) -> Option<&Arc<crate::store::Store>> {
        self.store.as_ref()
    }

    /// The current generation: the identity of the LAST completed (or
    /// in-flight) run. Mutation stamps use this — "registered after run N".
    #[must_use]
    pub fn current_generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// The loaded configuration.
    #[must_use]
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Registered step types, sorted.
    #[must_use]
    pub fn step_types(&self) -> Vec<String> {
        self.registry.type_names()
    }

    /// Machine-readable step specs: `(type_name, description, data_schema)`.
    #[must_use]
    pub fn step_specs(&self) -> Vec<(String, String, serde_json::Value)> {
        self.registry.specs()
    }

    /// Configured routines as `(name, description, step_count)`, sorted.
    #[must_use]
    pub fn list_routines(&self) -> Vec<(String, String, usize)> {
        self.config
            .routines
            .iter()
            .map(|(name, r)| (name.clone(), r.description.clone(), r.steps.len()))
            .collect()
    }

    /// The most recent report, or the report with an exact `generation`.
    #[must_use]
    pub fn report(&self, generation: Option<u64>) -> Option<Report> {
        let reports = self.reports.lock().expect("report store poisoned");
        match generation {
            Some(generation) => reports.iter().find(|r| r.generation == generation).cloned(),
            None => reports.back().cloned(),
        }
    }

    /// Run one named step of a routine (debugging aid).
    ///
    /// # Errors
    /// [`EngineError::UnknownRoutine`] / [`EngineError::UnknownStep`] /
    /// [`EngineError::Denied`].
    pub async fn run_step(
        &self,
        routine: &str,
        step: &str,
        dry_run: bool,
    ) -> Result<Report, EngineError> {
        let spec = self.routine_spec(routine)?;
        if !spec.steps.iter().any(|s| s.name == step) {
            return Err(EngineError::UnknownStep {
                routine: routine.to_string(),
                step: step.to_string(),
            });
        }
        self.run_routine(
            routine,
            RunOptions {
                only: vec![step.to_string()],
                dry_run,
                ..RunOptions::default()
            },
        )
        .await
    }

    /// Run a routine and store + return its report.
    ///
    /// # Errors
    /// [`EngineError::UnknownRoutine`] when the name is not configured;
    /// [`EngineError::Denied`] when the leash refuses the run.
    pub async fn run_routine(
        &self,
        routine: &str,
        opts: RunOptions,
    ) -> Result<Report, EngineError> {
        let spec = self.routine_spec(routine)?.clone();

        // The run's causal coordinate, and the single leash authorization.
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        let gate = Gate::new(generation);
        let cx = gate
            .authorize(&EngineExecTool, &self.granted)
            .map_err(|e| EngineError::Denied(e.to_string()))?;
        let exec = ExecGate::new(cx, self.spawner.clone());

        let selected: Vec<StepSpec> = spec
            .steps
            .into_iter()
            .filter(|s| opts.only.is_empty() || opts.only.iter().any(|n| n == &s.name))
            .filter(|s| !opts.skip.iter().any(|n| n == &s.name))
            .collect();

        let mut report = Report::new(generation, routine, opts.dry_run);

        // Walk config order, batching adjacent parallel steps.
        let mut queue: VecDeque<StepSpec> = selected.into();
        while let Some(first) = queue.pop_front() {
            let mut batch = vec![first];
            if batch[0].parallel {
                while queue.front().is_some_and(|s| s.parallel) {
                    batch.push(queue.pop_front().expect("peeked"));
                }
            }

            let prior = report.step_results.clone();
            if batch.len() == 1 {
                let result = self
                    .run_one(&batch[0], opts.dry_run, generation, &exec, prior)
                    .await;
                report.add(result);
            } else {
                // Concurrent batch; results re-ordered to config order so the
                // report stays deterministic.
                let names: Vec<(String, String)> = batch
                    .iter()
                    .map(|s| (s.name.clone(), s.step_type.clone()))
                    .collect();
                let mut join = tokio::task::JoinSet::new();
                for (index, step) in batch.into_iter().enumerate() {
                    let engine_cx = self.batch_context(opts.dry_run, generation, &exec, &prior);
                    let handler = self.registry.get(&step.step_type);
                    join.spawn(async move {
                        (index, run_with(handler.as_deref(), &step, &engine_cx).await)
                    });
                }
                let mut slots: Vec<Option<StepResult>> = vec![None; names.len()];
                while let Some(joined) = join.join_next().await {
                    if let Ok((index, result)) = joined {
                        slots[index] = Some(result);
                    }
                }
                for (index, slot) in slots.into_iter().enumerate() {
                    // A panicked handler still appears in the report as a
                    // failed step — soft-failure is a guarantee, not a habit.
                    let (name, step_type) = &names[index];
                    report.add(slot.unwrap_or_else(|| {
                        StepResult::fail(name, step_type, "step task panicked")
                    }));
                }
            }
        }

        report.finalize();
        // Persist the generation so it stays monotonic across restarts
        // (best effort — a read-only disk shouldn't kill the report).
        if let Some(store) = &self.store {
            if let Err(e) = store.set_last_generation(generation) {
                eprintln!("modulex: could not persist generation {generation}: {e}");
            }
        }
        let mut reports = self.reports.lock().expect("report store poisoned");
        while reports.len() >= REPORT_RETENTION {
            reports.pop_front();
        }
        reports.push_back(report.clone());
        Ok(report)
    }

    fn routine_spec(&self, routine: &str) -> Result<&crate::config::RoutineSpec, EngineError> {
        self.config.routines.get(routine).ok_or_else(|| {
            EngineError::UnknownRoutine(
                routine.to_string(),
                self.config
                    .routines
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", "),
            )
        })
    }

    fn batch_context(
        &self,
        dry_run: bool,
        generation: u64,
        exec: &ExecGate,
        prior: &[StepResult],
    ) -> RunContext {
        RunContext {
            config: self.config.clone(),
            dry_run,
            generation,
            exec: exec.clone(),
            prior: prior.to_vec(),
            store: self.store.clone(),
        }
    }

    async fn run_one(
        &self,
        step: &StepSpec,
        dry_run: bool,
        generation: u64,
        exec: &ExecGate,
        prior: Vec<StepResult>,
    ) -> StepResult {
        let cx = RunContext {
            config: self.config.clone(),
            dry_run,
            generation,
            exec: exec.clone(),
            prior,
            store: self.store.clone(),
        };
        run_with(self.registry.get(&step.step_type).as_deref(), step, &cx).await
    }
}

/// Dispatch one step: unknown type → failed result; missing required tool →
/// skipped result; otherwise the handler runs.
async fn run_with(
    handler: Option<&dyn crate::step::StepHandler>,
    step: &StepSpec,
    cx: &RunContext,
) -> StepResult {
    let Some(handler) = handler else {
        return StepResult::fail(
            &step.name,
            &step.step_type,
            format!("unknown step type {:?}", step.step_type),
        );
    };

    // Soft-skip probe: a missing external tool skips the step, it does not
    // fail the routine. (Dry runs skip the probe — nothing will spawn.)
    if !cx.dry_run {
        for program in handler.required_programs(step) {
            if !program_available(&program) {
                return StepResult::skip(
                    &step.name,
                    &step.step_type,
                    format!("tool {program:?} not found — skipped"),
                );
            }
        }
    }

    handler.run(step, cx).await
}

#[cfg(test)]
mod tests {
    use agent_bridle_core::Scope;

    use super::*;
    use crate::exec::test_support::MockSpawner;

    fn engine_with(config_toml: &str, outputs: Vec<crate::exec::ExecOutput>) -> Engine {
        let config = Config::from_toml(config_toml).expect("config parses");
        let registry = crate::steps::builtin_registry();
        let declared = config.declared_programs(&registry);
        let granted = Caveats {
            exec: Scope::only(declared),
            ..Caveats::top()
        };
        Engine::with_spawner(
            config,
            registry,
            granted,
            Arc::new(MockSpawner::with_outputs(outputs)),
        )
    }

    const TWO_STEP: &str = r#"
[routines.demo]
description = "test routine"

[[routines.demo.steps]]
name = "first"
type = "script"
command = "echo"

[[routines.demo.steps]]
name = "second"
type = "script"
command = "echo"
"#;

    #[tokio::test]
    async fn generations_are_monotonic_and_reports_are_stored() {
        let engine = engine_with(TWO_STEP, vec![]);
        let a = engine
            .run_routine(
                "demo",
                RunOptions {
                    dry_run: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let b = engine
            .run_routine(
                "demo",
                RunOptions {
                    dry_run: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(a.generation, 1);
        assert_eq!(b.generation, 2);
        assert_eq!(engine.report(None).unwrap().generation, 2);
        assert_eq!(engine.report(Some(1)).unwrap().generation, 1);
        assert!(engine.report(Some(99)).is_none());
    }

    #[tokio::test]
    async fn unknown_routine_is_an_engine_error() {
        let engine = engine_with(TWO_STEP, vec![]);
        let err = engine
            .run_routine("nope", RunOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(err, EngineError::UnknownRoutine(..)));
        assert!(err.to_string().contains("demo"));
    }

    #[tokio::test]
    async fn unknown_step_type_fails_softly_inside_the_report() {
        let engine = engine_with(
            r#"
[[routines.r.steps]]
name = "mystery"
type = "no-such-type"
"#,
            vec![],
        );
        let report = engine
            .run_routine("r", RunOptions::default())
            .await
            .unwrap();
        assert!(!report.success);
        assert!(report.step_results[0]
            .error
            .as_deref()
            .unwrap()
            .contains("unknown step type"));
    }

    #[tokio::test]
    async fn only_and_skip_filter_by_step_name() {
        let engine = engine_with(TWO_STEP, vec![]);
        let report = engine
            .run_routine(
                "demo",
                RunOptions {
                    only: vec!["second".into()],
                    dry_run: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(report.step_results.len(), 1);
        assert_eq!(report.step_results[0].step_name, "second");

        let report = engine
            .run_routine(
                "demo",
                RunOptions {
                    skip: vec!["second".into()],
                    dry_run: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(report.step_results.len(), 1);
        assert_eq!(report.step_results[0].step_name, "first");
    }

    #[tokio::test]
    async fn missing_tool_soft_skips_the_step() {
        let engine = engine_with(
            r#"
[[routines.r.steps]]
name = "ghost"
type = "script"
command = "definitely-not-a-real-binary-xyzzy"
"#,
            vec![],
        );
        let report = engine
            .run_routine("r", RunOptions::default())
            .await
            .unwrap();
        assert!(report.success, "skip must not fail the routine");
        assert!(report.step_results[0].skipped);
        assert!(report.step_results[0].output.contains("not found"));
    }

    #[tokio::test]
    async fn parallel_batch_results_keep_config_order() {
        let engine = engine_with(
            r#"
[[routines.r.steps]]
name = "alpha"
type = "script"
command = "sh"
parallel = true

[[routines.r.steps]]
name = "beta"
type = "script"
command = "sh"
parallel = true

[[routines.r.steps]]
name = "gamma"
type = "script"
command = "sh"
"#,
            vec![
                MockSpawner::ok("one"),
                MockSpawner::ok("two"),
                MockSpawner::ok("three"),
            ],
        );
        let report = engine
            .run_routine("r", RunOptions::default())
            .await
            .unwrap();
        let names: Vec<&str> = report
            .step_results
            .iter()
            .map(|r| r.step_name.as_str())
            .collect();
        assert_eq!(names, vec!["alpha", "beta", "gamma"]);
        assert!(report.success);
    }

    #[tokio::test]
    async fn run_step_runs_exactly_one_and_validates_the_name() {
        let engine = engine_with(TWO_STEP, vec![]);
        let report = engine.run_step("demo", "first", true).await.unwrap();
        assert_eq!(report.step_results.len(), 1);

        let err = engine.run_step("demo", "nope", true).await.unwrap_err();
        assert!(matches!(err, EngineError::UnknownStep { .. }));
    }

    #[tokio::test]
    async fn report_retention_is_bounded() {
        let engine = engine_with(TWO_STEP, vec![]);
        for _ in 0..20 {
            engine
                .run_routine(
                    "demo",
                    RunOptions {
                        dry_run: true,
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
        }
        assert!(engine.report(Some(1)).is_none(), "oldest evicted");
        assert_eq!(engine.report(None).unwrap().generation, 20);
    }
}
