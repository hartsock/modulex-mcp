//! The MCP tool surface: a registry of [`ToolSpec`]s with declared
//! side-effect (`mutates`) and facet membership, plus dispatch into the
//! engine (FOUNDATION pass F2).
//!
//! Design rules:
//!
//! - **Declared, not guessed**: every tool states `mutates` up front — it
//!   feeds the surface policy (F3 facets / `tool_invoke` gating) and the
//!   self-documentation surface (#30).
//! - **Ordered registry**: `tools/list` order is stable and pinned by tests;
//!   growing the default surface is a deliberate, reviewed change (#32).
//! - **Facets** name the disclosure group a tool belongs to (`core`,
//!   `store`); F3 wires them to config-gated exposure.

use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;

use modulex_core::{Engine, EngineError, Report, RunOptions};
use serde_json::{json, Value};

/// The result of one `tools/call`: text content plus the error flag.
pub struct ToolOutcome {
    /// Text payload (markdown or compact JSON, per the `format` argument).
    pub text: String,
    /// True ONLY for engine faults — never for per-step failures.
    pub is_error: bool,
}

impl ToolOutcome {
    fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
        }
    }

    fn err(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
        }
    }
}

/// A registered tool: its MCP spec plus the declared policy fields.
pub struct ToolSpec {
    /// Tool name (`tools/call` dispatch key).
    pub name: &'static str,
    /// Human description, shown in `tools/list`.
    pub description: &'static str,
    /// JSON Schema for the arguments.
    pub input_schema: Value,
    /// DECLARED side-effect flag: true when invoking changes state on the
    /// user's behalf (store writes, routine execution). Feeds facet policy
    /// and self-documentation — never inferred from names.
    pub mutates: bool,
    /// Disclosure group (F3 wires these to config-gated exposure).
    pub facet: &'static str,
}

type ToolFuture<'a> = Pin<Box<dyn Future<Output = ToolOutcome> + Send + 'a>>;
type ToolHandler = for<'a> fn(&'a Engine, &'a Value) -> ToolFuture<'a>;

struct ToolEntry {
    spec: ToolSpec,
    handler: ToolHandler,
}

/// The ordered tool registry — the single source of truth for the MCP
/// surface. Plugins will register into it (F5); facets filter it (F3).
pub struct ToolRegistry {
    entries: Vec<ToolEntry>,
}

impl ToolRegistry {
    /// All registered specs, in stable `tools/list` order.
    pub fn specs(&self) -> impl Iterator<Item = &ToolSpec> {
        self.entries.iter().map(|e| &e.spec)
    }

    /// The `tools/list` payload.
    #[must_use]
    pub fn specs_json(&self) -> Value {
        Value::Array(
            self.entries
                .iter()
                .map(|e| {
                    json!({
                        "name": e.spec.name,
                        "description": e.spec.description,
                        "inputSchema": e.spec.input_schema,
                    })
                })
                .collect(),
        )
    }

    /// Dispatch one call. Unknown names are a tool error (engine fault).
    pub async fn call(&self, engine: &Engine, name: &str, args: &Value) -> ToolOutcome {
        match self.entries.iter().find(|e| e.spec.name == name) {
            Some(entry) => (entry.handler)(engine, args).await,
            None => ToolOutcome::err(format!("unknown tool: {name}")),
        }
    }
}

/// The builtin registry (lazily built once per process).
pub fn registry() -> &'static ToolRegistry {
    static REGISTRY: OnceLock<ToolRegistry> = OnceLock::new();
    REGISTRY.get_or_init(build_registry)
}

/// Tool definitions for `tools/list` (compat shim over [`registry`]).
#[must_use]
pub fn tool_specs() -> Value {
    registry().specs_json()
}

/// Dispatch one tool call against the engine (compat shim over [`registry`]).
pub async fn call(engine: &Engine, name: &str, args: &Value) -> ToolOutcome {
    registry().call(engine, name, args).await
}

// ── shared helpers ─────────────────────────────────────────────────────

fn render(report: &Report, args: &Value) -> String {
    match args.get("format").and_then(Value::as_str) {
        Some("json") => report.to_json(),
        // The agent-native view: structured payloads only (data contract).
        Some("data") => report.to_data_json(),
        _ => report.to_text(),
    }
}

fn str_list(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(ToString::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn engine_fault(e: &EngineError) -> ToolOutcome {
    ToolOutcome::err(e.to_string())
}

/// The store handle, or the standard fault when it's unavailable.
fn store_of(engine: &Engine) -> Result<&std::sync::Arc<modulex_core::Store>, ToolOutcome> {
    engine.store().ok_or_else(|| {
        ToolOutcome::err("agent state store unavailable (could not be opened at startup)")
    })
}

/// Unify `Result<T: Serialize, StoreError>` into a tool outcome.
fn store_outcome<T: serde::Serialize>(
    result: Result<T, modulex_core::store::StoreError>,
) -> ToolOutcome {
    match result {
        Ok(value) => {
            ToolOutcome::ok(serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string()))
        }
        Err(e) => ToolOutcome::err(e.to_string()),
    }
}

/// Shared schema fragment for the report `format` argument.
fn format_property() -> Value {
    json!({ "type": "string", "enum": ["text", "json", "data"], "default": "text" })
}

// ── handlers ───────────────────────────────────────────────────────────

fn h_routine_run<'a>(engine: &'a Engine, args: &'a Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let Some(routine) = args.get("routine").and_then(Value::as_str) else {
            return ToolOutcome::err("routine_run requires `routine`");
        };
        let opts = RunOptions {
            only: str_list(args, "only"),
            skip: str_list(args, "skip"),
            dry_run: args
                .get("dry_run")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        };
        match engine.run_routine(routine, opts).await {
            Ok(report) => ToolOutcome::ok(render(&report, args)),
            Err(e) => engine_fault(&e),
        }
    })
}

fn h_routine_list<'a>(engine: &'a Engine, _args: &'a Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let routines: Vec<Value> = engine
            .list_routines()
            .into_iter()
            .map(|(name, description, steps)| {
                json!({ "name": name, "description": description, "steps": steps })
            })
            .collect();
        ToolOutcome::ok(json!({ "routines": routines }).to_string())
    })
}

fn h_step_run<'a>(engine: &'a Engine, args: &'a Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let (Some(routine), Some(step)) = (
            args.get("routine").and_then(Value::as_str),
            args.get("step").and_then(Value::as_str),
        ) else {
            return ToolOutcome::err("step_run requires `routine` and `step`");
        };
        let dry_run = args
            .get("dry_run")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        match engine.run_step(routine, step, dry_run).await {
            Ok(report) => ToolOutcome::ok(render(&report, args)),
            Err(e) => engine_fault(&e),
        }
    })
}

fn h_report_get<'a>(engine: &'a Engine, args: &'a Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let generation = args.get("generation").and_then(Value::as_u64);
        match engine.report(generation) {
            Some(report) => ToolOutcome::ok(render(&report, args)),
            None => ToolOutcome::err(match generation {
                Some(generation) => format!("no stored report for generation {generation}"),
                None => "no reports yet — run a routine first".to_string(),
            }),
        }
    })
}

fn h_steps_list<'a>(engine: &'a Engine, _args: &'a Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let steps: Vec<Value> = engine
            .step_specs()
            .into_iter()
            .map(|(name, description, schema)| {
                json!({ "type": name, "description": description, "data_schema": schema })
            })
            .collect();
        ToolOutcome::ok(json!({ "steps": steps }).to_string())
    })
}

fn h_reminder_add<'a>(engine: &'a Engine, args: &'a Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let store = match store_of(engine) {
            Ok(store) => store,
            Err(fault) => return fault,
        };
        let Some(text) = args.get("text").and_then(Value::as_str) else {
            return ToolOutcome::err("reminder_add requires `text`");
        };
        // Mutation stamps: the generation current at call time —
        // "registered after run N". A counter, never a clock.
        let generation = engine.current_generation();
        store_outcome(
            store
                .reminder_add(
                    text,
                    args.get("due").and_then(Value::as_str),
                    args.get("recurrence").and_then(Value::as_str),
                    generation,
                )
                .map(|id| json!({ "id": id, "created_gen": generation })),
        )
    })
}

fn h_reminder_list<'a>(engine: &'a Engine, _args: &'a Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let store = match store_of(engine) {
            Ok(store) => store,
            Err(fault) => return fault,
        };
        store_outcome(store.reminders_open())
    })
}

fn h_reminder_done<'a>(engine: &'a Engine, args: &'a Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let store = match store_of(engine) {
            Ok(store) => store,
            Err(fault) => return fault,
        };
        let Some(id) = args.get("id").and_then(Value::as_i64) else {
            return ToolOutcome::err("reminder_done requires integer `id`");
        };
        match store.reminder_done(id, engine.current_generation()) {
            Ok(true) => ToolOutcome::ok(format!("reminder #{id} done")),
            Ok(false) => ToolOutcome::err(format!("no open reminder #{id}")),
            Err(e) => ToolOutcome::err(e.to_string()),
        }
    })
}

fn h_countdown_add<'a>(engine: &'a Engine, args: &'a Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let store = match store_of(engine) {
            Ok(store) => store,
            Err(fault) => return fault,
        };
        let (Some(label), Some(start), Some(end)) = (
            args.get("label").and_then(Value::as_str),
            args.get("start_date").and_then(Value::as_str),
            args.get("end_date").and_then(Value::as_str),
        ) else {
            return ToolOutcome::err("countdown_add requires `label`, `start_date`, `end_date`");
        };
        let total = args
            .get("total_work_days")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(30);
        let generation = engine.current_generation();
        store_outcome(
            store
                .countdown_add(
                    label,
                    start,
                    end,
                    total,
                    args.get("display").and_then(Value::as_str),
                    generation,
                )
                .map(|id| json!({ "id": id, "created_gen": generation })),
        )
    })
}

fn h_countdown_retire<'a>(engine: &'a Engine, args: &'a Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let store = match store_of(engine) {
            Ok(store) => store,
            Err(fault) => return fault,
        };
        let Some(id) = args.get("id").and_then(Value::as_i64) else {
            return ToolOutcome::err("countdown_retire requires integer `id`");
        };
        match store.countdown_retire(id, engine.current_generation()) {
            Ok(true) => ToolOutcome::ok(format!("countdown #{id} retired")),
            Ok(false) => ToolOutcome::err(format!("no active countdown #{id}")),
            Err(e) => ToolOutcome::err(e.to_string()),
        }
    })
}

fn h_watch_add<'a>(engine: &'a Engine, args: &'a Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let store = match store_of(engine) {
            Ok(store) => store,
            Err(fault) => return fault,
        };
        let Some(url) = args.get("url").and_then(Value::as_str) else {
            return ToolOutcome::err("watch_add requires `url`");
        };
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return ToolOutcome::err("watch_add: only http(s) URLs can be watched");
        }
        let note = args.get("note").and_then(Value::as_str).unwrap_or("");
        let generation = engine.current_generation();
        store_outcome(
            store
                .watch_add(url, note, generation)
                .map(|id| json!({ "id": id, "created_gen": generation })),
        )
    })
}

fn h_watch_list<'a>(engine: &'a Engine, _args: &'a Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let store = match store_of(engine) {
            Ok(store) => store,
            Err(fault) => return fault,
        };
        store_outcome(store.watches())
    })
}

fn h_watch_remove<'a>(engine: &'a Engine, args: &'a Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let store = match store_of(engine) {
            Ok(store) => store,
            Err(fault) => return fault,
        };
        let Some(id) = args.get("id").and_then(Value::as_i64) else {
            return ToolOutcome::err("watch_remove requires integer `id`");
        };
        match store.watch_remove(id) {
            Ok(true) => ToolOutcome::ok(format!("watch #{id} removed")),
            Ok(false) => ToolOutcome::err(format!("no watch #{id}")),
            Err(e) => ToolOutcome::err(e.to_string()),
        }
    })
}

fn h_store_export<'a>(engine: &'a Engine, _args: &'a Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let store = match store_of(engine) {
            Ok(store) => store,
            Err(fault) => return fault,
        };
        match store.export_json() {
            Ok(json) => ToolOutcome::ok(json),
            Err(e) => ToolOutcome::err(e.to_string()),
        }
    })
}

// ── the registry ───────────────────────────────────────────────────────

#[allow(clippy::too_many_lines)] // a flat, ordered spec table reads best whole
fn build_registry() -> ToolRegistry {
    let entries = vec![
        ToolEntry {
            spec: ToolSpec {
                name: "routine_run",
                description: "Run a named routine and return its report. Per-step \
                    failures are soft: they appear inside the report, not as tool \
                    errors. Reports are identified by a monotonic generation counter.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "routine": { "type": "string", "description": "Routine name from the config" },
                        "only": { "type": "array", "items": { "type": "string" },
                                  "description": "Run only these step names" },
                        "skip": { "type": "array", "items": { "type": "string" },
                                  "description": "Skip these step names" },
                        "dry_run": { "type": "boolean", "default": false },
                        "format": format_property()
                    },
                    "required": ["routine"]
                }),
                mutates: true, // executes config-defined work (pulls, fetches, watch updates)
                facet: "core",
            },
            handler: h_routine_run,
        },
        ToolEntry {
            spec: ToolSpec {
                name: "routine_list",
                description: "List configured routines (name, description, step count).",
                input_schema: json!({ "type": "object", "properties": {} }),
                mutates: false,
                facet: "core",
            },
            handler: h_routine_list,
        },
        ToolEntry {
            spec: ToolSpec {
                name: "step_run",
                description: "Run a single step of a routine (debugging aid). Returns \
                    a one-step report.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "routine": { "type": "string" },
                        "step": { "type": "string", "description": "Step name within the routine" },
                        "dry_run": { "type": "boolean", "default": false },
                        "format": format_property()
                    },
                    "required": ["routine", "step"]
                }),
                mutates: true,
                facet: "core",
            },
            handler: h_step_run,
        },
        ToolEntry {
            spec: ToolSpec {
                name: "report_get",
                description: "Fetch a stored report: the latest, or an exact generation. \
                    Generations are monotonic counters, not timestamps.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "generation": { "type": "integer", "minimum": 1,
                                        "description": "Exact report generation; omit for latest" },
                        "format": format_property()
                    }
                }),
                mutates: false,
                facet: "core",
            },
            handler: h_report_get,
        },
        ToolEntry {
            spec: ToolSpec {
                name: "steps_list",
                description: "List registered step types with their data-contract \
                    schemas: [{type, description, data_schema}]. Executed steps' \
                    `data` payloads conform to these schemas (versioned contracts).",
                input_schema: json!({ "type": "object", "properties": {} }),
                mutates: false,
                facet: "core",
            },
            handler: h_steps_list,
        },
        ToolEntry {
            spec: ToolSpec {
                name: "reminder_add",
                description: "Register a reminder ('remind me of X') in the agent state \
                    store. It surfaces in every `reminders` step until marked done.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "text": { "type": "string" },
                        "due": { "type": "string", "description": "Optional ISO date YYYY-MM-DD" },
                        "recurrence": { "type": "string", "enum": ["daily", "weekly", "monthly"] }
                    },
                    "required": ["text"]
                }),
                mutates: true,
                facet: "store",
            },
            handler: h_reminder_add,
        },
        ToolEntry {
            spec: ToolSpec {
                name: "reminder_list",
                description: "List open reminders from the agent state store.",
                input_schema: json!({ "type": "object", "properties": {} }),
                mutates: false,
                facet: "store",
            },
            handler: h_reminder_list,
        },
        ToolEntry {
            spec: ToolSpec {
                name: "reminder_done",
                description: "Mark a reminder done (by id from reminder_list).",
                input_schema: json!({
                    "type": "object",
                    "properties": { "id": { "type": "integer" } },
                    "required": ["id"]
                }),
                mutates: true,
                facet: "store",
            },
            handler: h_reminder_done,
        },
        ToolEntry {
            spec: ToolSpec {
                name: "countdown_add",
                description: "Register a countdown (work-day progress) in the agent state \
                    store; merged with config countdowns by the countdown-calc step.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "label": { "type": "string" },
                        "start_date": { "type": "string", "description": "ISO date" },
                        "end_date": { "type": "string", "description": "ISO date; expires after" },
                        "total_work_days": { "type": "integer", "default": 30 },
                        "display": { "type": "string",
                                     "description": "Template with {label} {n} {total}" }
                    },
                    "required": ["label", "start_date", "end_date"]
                }),
                mutates: true,
                facet: "store",
            },
            handler: h_countdown_add,
        },
        ToolEntry {
            spec: ToolSpec {
                name: "countdown_retire",
                description: "Retire a stored countdown (by id).",
                input_schema: json!({
                    "type": "object",
                    "properties": { "id": { "type": "integer" } },
                    "required": ["id"]
                }),
                mutates: true,
                facet: "store",
            },
            handler: h_countdown_retire,
        },
        ToolEntry {
            spec: ToolSpec {
                name: "watch_add",
                description: "Register an http(s) URL for change tracking. The url-watch \
                    step fetches it (net-leashed, SSRF-screened) and reports changes.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "url": { "type": "string" },
                        "note": { "type": "string" }
                    },
                    "required": ["url"]
                }),
                mutates: true,
                facet: "store",
            },
            handler: h_watch_add,
        },
        ToolEntry {
            spec: ToolSpec {
                name: "watch_list",
                description: "List registered URL watches.",
                input_schema: json!({ "type": "object", "properties": {} }),
                mutates: false,
                facet: "store",
            },
            handler: h_watch_list,
        },
        ToolEntry {
            spec: ToolSpec {
                name: "watch_remove",
                description: "Remove a URL watch (by id).",
                input_schema: json!({
                    "type": "object",
                    "properties": { "id": { "type": "integer" } },
                    "required": ["id"]
                }),
                mutates: true,
                facet: "store",
            },
            handler: h_watch_remove,
        },
        ToolEntry {
            spec: ToolSpec {
                name: "store_export",
                description: "Export the whole agent state store as plain JSON \
                    (sovereignty: the content is never locked into SQLite).",
                input_schema: json!({ "type": "object", "properties": {} }),
                mutates: false,
                facet: "store",
            },
            handler: h_store_export,
        },
    ];
    ToolRegistry { entries }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The surface-policy pin (#32 precursor): names, order, facets, and the
    /// DECLARED mutates set. Growing or reordering the surface, or flipping
    /// a side-effect declaration, is a deliberate reviewed change HERE.
    #[test]
    fn surface_policy_is_pinned() {
        let expected: Vec<(&str, bool, &str)> = vec![
            ("routine_run", true, "core"),
            ("routine_list", false, "core"),
            ("step_run", true, "core"),
            ("report_get", false, "core"),
            ("steps_list", false, "core"),
            ("reminder_add", true, "store"),
            ("reminder_list", false, "store"),
            ("reminder_done", true, "store"),
            ("countdown_add", true, "store"),
            ("countdown_retire", true, "store"),
            ("watch_add", true, "store"),
            ("watch_list", false, "store"),
            ("watch_remove", true, "store"),
            ("store_export", false, "store"),
        ];
        let actual: Vec<(&str, bool, &str)> = registry()
            .specs()
            .map(|s| (s.name, s.mutates, s.facet))
            .collect();
        assert_eq!(
            actual, expected,
            "the tool surface policy changed — review!"
        );
    }

    #[test]
    fn tool_names_are_unique() {
        let mut names: Vec<&str> = registry().specs().map(|s| s.name).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "duplicate tool name registered");
    }

    #[test]
    fn every_spec_has_schema_and_description() {
        for spec in registry().specs() {
            assert!(
                !spec.description.is_empty(),
                "{}: empty description",
                spec.name
            );
            assert!(
                spec.input_schema.is_object(),
                "{}: inputSchema must be an object",
                spec.name
            );
        }
    }
}
