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

/// Dispatch one tool call against the engine.
pub async fn call(engine: &Engine, name: &str, args: &Value) -> ToolOutcome {
    match name {
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
