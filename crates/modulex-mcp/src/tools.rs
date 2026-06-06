//! The MCP tool surface: specs (JSON Schema) and dispatch into the engine.

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

/// Tool definitions for `tools/list`.
#[must_use]
pub fn tool_specs() -> Value {
    json!([
        {
            "name": "routine_run",
            "description": "Run a named routine and return its report. Per-step \
                failures are soft: they appear inside the report, not as tool \
                errors. Reports are identified by a monotonic generation counter.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "routine": { "type": "string", "description": "Routine name from the config" },
                    "only": { "type": "array", "items": { "type": "string" },
                              "description": "Run only these step names" },
                    "skip": { "type": "array", "items": { "type": "string" },
                              "description": "Skip these step names" },
                    "dry_run": { "type": "boolean", "default": false },
                    "format": { "type": "string", "enum": ["text", "json"], "default": "text" }
                },
                "required": ["routine"]
            }
        },
        {
            "name": "routine_list",
            "description": "List configured routines (name, description, step count).",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "step_run",
            "description": "Run a single step of a routine (debugging aid). Returns \
                a one-step report.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "routine": { "type": "string" },
                    "step": { "type": "string", "description": "Step name within the routine" },
                    "dry_run": { "type": "boolean", "default": false },
                    "format": { "type": "string", "enum": ["text", "json"], "default": "text" }
                },
                "required": ["routine", "step"]
            }
        },
        {
            "name": "report_get",
            "description": "Fetch a stored report: the latest, or an exact generation. \
                Generations are monotonic counters, not timestamps.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "generation": { "type": "integer", "minimum": 1,
                                    "description": "Exact report generation; omit for latest" },
                    "format": { "type": "string", "enum": ["text", "json"], "default": "text" }
                }
            }
        },
        {
            "name": "steps_list",
            "description": "List registered step types (builtin plus plugins).",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "reminder_add",
            "description": "Register a reminder ('remind me of X') in the agent state \
                store. It surfaces in every `reminders` step until marked done.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": { "type": "string" },
                    "due": { "type": "string", "description": "Optional ISO date YYYY-MM-DD" },
                    "recurrence": { "type": "string", "enum": ["daily", "weekly", "monthly"] }
                },
                "required": ["text"]
            }
        },
        {
            "name": "reminder_list",
            "description": "List open reminders from the agent state store.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "reminder_done",
            "description": "Mark a reminder done (by id from reminder_list).",
            "inputSchema": {
                "type": "object",
                "properties": { "id": { "type": "integer" } },
                "required": ["id"]
            }
        },
        {
            "name": "countdown_add",
            "description": "Register a countdown (work-day progress) in the agent state \
                store; merged with config countdowns by the countdown-calc step.",
            "inputSchema": {
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
            }
        },
        {
            "name": "countdown_retire",
            "description": "Retire a stored countdown (by id).",
            "inputSchema": {
                "type": "object",
                "properties": { "id": { "type": "integer" } },
                "required": ["id"]
            }
        },
        {
            "name": "watch_add",
            "description": "Register an http(s) URL for change tracking. The url-watch \
                step fetches it (net-leashed, SSRF-screened) and reports changes.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": { "type": "string" },
                    "note": { "type": "string" }
                },
                "required": ["url"]
            }
        },
        {
            "name": "watch_list",
            "description": "List registered URL watches.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "watch_remove",
            "description": "Remove a URL watch (by id).",
            "inputSchema": {
                "type": "object",
                "properties": { "id": { "type": "integer" } },
                "required": ["id"]
            }
        },
        {
            "name": "store_export",
            "description": "Export the whole agent state store as plain JSON \
                (sovereignty: the content is never locked into SQLite).",
            "inputSchema": { "type": "object", "properties": {} }
        }
    ])
}

fn render(report: &Report, args: &Value) -> String {
    match args.get("format").and_then(Value::as_str) {
        Some("json") => report.to_json(),
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

/// Dispatch one tool call against the engine.
#[allow(clippy::too_many_lines)] // a flat tool dispatch reads better unsplit
pub async fn call(engine: &Engine, name: &str, args: &Value) -> ToolOutcome {
    // Mutation stamps: the generation current at call time — "registered
    // after run N". A counter, never a clock.
    let generation = engine.current_generation();
    match name {
        "reminder_add" => {
            let store = match store_of(engine) {
                Ok(store) => store,
                Err(fault) => return fault,
            };
            let Some(text) = args.get("text").and_then(Value::as_str) else {
                return ToolOutcome::err("reminder_add requires `text`");
            };
            store_outcome(
                store
                    .reminder_add(
                        text,
                        args.get("due").and_then(Value::as_str),
                        args.get("recurrence").and_then(Value::as_str),
                        generation,
                    )
                    .map(|id| serde_json::json!({ "id": id, "created_gen": generation })),
            )
        }
        "reminder_list" => {
            let store = match store_of(engine) {
                Ok(store) => store,
                Err(fault) => return fault,
            };
            store_outcome(store.reminders_open())
        }
        "reminder_done" => {
            let store = match store_of(engine) {
                Ok(store) => store,
                Err(fault) => return fault,
            };
            let Some(id) = args.get("id").and_then(Value::as_i64) else {
                return ToolOutcome::err("reminder_done requires integer `id`");
            };
            match store.reminder_done(id, generation) {
                Ok(true) => ToolOutcome::ok(format!("reminder #{id} done")),
                Ok(false) => ToolOutcome::err(format!("no open reminder #{id}")),
                Err(e) => ToolOutcome::err(e.to_string()),
            }
        }
        "countdown_add" => {
            let store = match store_of(engine) {
                Ok(store) => store,
                Err(fault) => return fault,
            };
            let (Some(label), Some(start), Some(end)) = (
                args.get("label").and_then(Value::as_str),
                args.get("start_date").and_then(Value::as_str),
                args.get("end_date").and_then(Value::as_str),
            ) else {
                return ToolOutcome::err(
                    "countdown_add requires `label`, `start_date`, `end_date`",
                );
            };
            let total = args
                .get("total_work_days")
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(30);
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
                    .map(|id| serde_json::json!({ "id": id, "created_gen": generation })),
            )
        }
        "countdown_retire" => {
            let store = match store_of(engine) {
                Ok(store) => store,
                Err(fault) => return fault,
            };
            let Some(id) = args.get("id").and_then(Value::as_i64) else {
                return ToolOutcome::err("countdown_retire requires integer `id`");
            };
            match store.countdown_retire(id, generation) {
                Ok(true) => ToolOutcome::ok(format!("countdown #{id} retired")),
                Ok(false) => ToolOutcome::err(format!("no active countdown #{id}")),
                Err(e) => ToolOutcome::err(e.to_string()),
            }
        }
        "watch_add" => {
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
            store_outcome(
                store
                    .watch_add(url, note, generation)
                    .map(|id| serde_json::json!({ "id": id, "created_gen": generation })),
            )
        }
        "watch_list" => {
            let store = match store_of(engine) {
                Ok(store) => store,
                Err(fault) => return fault,
            };
            store_outcome(store.watches())
        }
        "watch_remove" => {
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
        }
        "store_export" => {
            let store = match store_of(engine) {
                Ok(store) => store,
                Err(fault) => return fault,
            };
            match store.export_json() {
                Ok(json) => ToolOutcome::ok(json),
                Err(e) => ToolOutcome::err(e.to_string()),
            }
        }
        "routine_run" => {
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
        }
        "routine_list" => {
            let routines: Vec<Value> = engine
                .list_routines()
                .into_iter()
                .map(|(name, description, steps)| {
                    json!({ "name": name, "description": description, "steps": steps })
                })
                .collect();
            ToolOutcome::ok(json!({ "routines": routines }).to_string())
        }
        "step_run" => {
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
        }
        "report_get" => {
            let generation = args.get("generation").and_then(Value::as_u64);
            match engine.report(generation) {
                Some(report) => ToolOutcome::ok(render(&report, args)),
                None => ToolOutcome::err(match generation {
                    Some(generation) => format!("no stored report for generation {generation}"),
                    None => "no reports yet — run a routine first".to_string(),
                }),
            }
        }
        "steps_list" => ToolOutcome::ok(json!({ "step_types": engine.step_types() }).to_string()),
        other => ToolOutcome::err(format!("unknown tool: {other}")),
    }
}
