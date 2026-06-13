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

/// Everything a tool handler can reach: the engine plus the facet policy
/// (the discovery trio dispatches through the policy).
pub struct CallCtx<'a> {
    /// The routine engine.
    pub engine: &'a Engine,
    /// The connection's facet exposure policy.
    pub policy: &'a crate::facets::FacetPolicy,
}

type ToolFuture<'a> = Pin<Box<dyn Future<Output = ToolOutcome> + Send + 'a>>;
type ToolHandler = for<'a> fn(&'a CallCtx<'a>, &'a Value) -> ToolFuture<'a>;

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

    /// The `tools/list` payload: ONLY tools whose facet the policy exposes
    /// (progressive disclosure — listing is context cost, not capability).
    #[must_use]
    pub fn specs_json(&self, policy: &crate::facets::FacetPolicy) -> Value {
        Value::Array(
            self.entries
                .iter()
                .filter(|e| policy.exposes(e.spec.facet))
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

    /// Dispatch one call. Callability is broader than listing: any
    /// registered tool whose facet is not DENIED is callable (clients learn
    /// names via tool_search). Unknown/denied names are tool errors.
    pub async fn call(
        &self,
        engine: &Engine,
        policy: &crate::facets::FacetPolicy,
        name: &str,
        args: &Value,
    ) -> ToolOutcome {
        let Some(entry) = self.entries.iter().find(|e| e.spec.name == name) else {
            return ToolOutcome::err(format!("unknown tool: {name}"));
        };
        if policy.denies(entry.spec.facet) {
            return ToolOutcome::err(format!(
                "tool {name} is in denied facet {:?}",
                entry.spec.facet
            ));
        }
        let cx = CallCtx { engine, policy };
        (entry.handler)(&cx, args).await
    }
}

/// The builtin registry (lazily built once per process).
pub fn registry() -> &'static ToolRegistry {
    static REGISTRY: OnceLock<ToolRegistry> = OnceLock::new();
    REGISTRY.get_or_init(build_registry)
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

fn h_routine_run<'a>(cx: &'a CallCtx<'a>, args: &'a Value) -> ToolFuture<'a> {
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
        match cx.engine.run_routine(routine, opts).await {
            Ok(report) => ToolOutcome::ok(render(&report, args)),
            Err(e) => engine_fault(&e),
        }
    })
}

fn h_routine_list<'a>(cx: &'a CallCtx<'a>, _args: &'a Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let routines: Vec<Value> = cx.engine
            .list_routines()
            .into_iter()
            .map(|(name, description, steps)| {
                json!({ "name": name, "description": description, "steps": steps })
            })
            .collect();
        ToolOutcome::ok(json!({ "routines": routines }).to_string())
    })
}

fn h_step_run<'a>(cx: &'a CallCtx<'a>, args: &'a Value) -> ToolFuture<'a> {
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
        match cx.engine.run_step(routine, step, dry_run).await {
            Ok(report) => ToolOutcome::ok(render(&report, args)),
            Err(e) => engine_fault(&e),
        }
    })
}

fn h_report_get<'a>(cx: &'a CallCtx<'a>, args: &'a Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let generation = args.get("generation").and_then(Value::as_u64);
        match cx.engine.report(generation) {
            Some(report) => ToolOutcome::ok(render(&report, args)),
            None => ToolOutcome::err(match generation {
                Some(generation) => format!("no stored report for generation {generation}"),
                None => "no reports yet — run a routine first".to_string(),
            }),
        }
    })
}

fn h_steps_list<'a>(cx: &'a CallCtx<'a>, _args: &'a Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let steps: Vec<Value> = cx.engine
            .step_specs()
            .into_iter()
            .map(|(name, description, schema)| {
                json!({ "type": name, "description": description, "data_schema": schema })
            })
            .collect();
        ToolOutcome::ok(json!({ "steps": steps }).to_string())
    })
}

async fn reminder_add_impl(engine: &Engine, args: &Value) -> ToolOutcome {
    {
        let cx_engine = engine;
        let store = match store_of(cx_engine) {
            Ok(store) => store,
            Err(fault) => return fault,
        };
        let Some(text) = args.get("text").and_then(Value::as_str) else {
            return ToolOutcome::err("reminder_add requires `text`");
        };
        // Mutation stamps: the generation current at call time —
        // "registered after run N". A counter, never a clock.
        let generation = cx_engine.current_generation();
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
    }
}

fn h_reminder_add<'a>(cx: &'a CallCtx<'a>, args: &'a Value) -> ToolFuture<'a> {
    Box::pin(reminder_add_impl(cx.engine, args))
}

fn h_reminder_list<'a>(cx: &'a CallCtx<'a>, _args: &'a Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let store = match store_of(cx.engine) {
            Ok(store) => store,
            Err(fault) => return fault,
        };
        store_outcome(store.reminders_open())
    })
}

fn h_reminder_done<'a>(cx: &'a CallCtx<'a>, args: &'a Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let store = match store_of(cx.engine) {
            Ok(store) => store,
            Err(fault) => return fault,
        };
        let Some(id) = args.get("id").and_then(Value::as_i64) else {
            return ToolOutcome::err("reminder_done requires integer `id`");
        };
        match store.reminder_done(id, cx.engine.current_generation()) {
            Ok(true) => ToolOutcome::ok(format!("reminder #{id} done")),
            Ok(false) => ToolOutcome::err(format!("no open reminder #{id}")),
            Err(e) => ToolOutcome::err(e.to_string()),
        }
    })
}

async fn countdown_add_impl(engine: &Engine, args: &Value) -> ToolOutcome {
    {
        let cx_engine = engine;
        let store = match store_of(cx_engine) {
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
        let generation = cx_engine.current_generation();
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
    }
}

fn h_countdown_add<'a>(cx: &'a CallCtx<'a>, args: &'a Value) -> ToolFuture<'a> {
    Box::pin(countdown_add_impl(cx.engine, args))
}

fn h_countdown_retire<'a>(cx: &'a CallCtx<'a>, args: &'a Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let store = match store_of(cx.engine) {
            Ok(store) => store,
            Err(fault) => return fault,
        };
        let Some(id) = args.get("id").and_then(Value::as_i64) else {
            return ToolOutcome::err("countdown_retire requires integer `id`");
        };
        match store.countdown_retire(id, cx.engine.current_generation()) {
            Ok(true) => ToolOutcome::ok(format!("countdown #{id} retired")),
            Ok(false) => ToolOutcome::err(format!("no active countdown #{id}")),
            Err(e) => ToolOutcome::err(e.to_string()),
        }
    })
}

async fn watch_add_impl(engine: &Engine, args: &Value) -> ToolOutcome {
    {
        let cx_engine = engine;
        let store = match store_of(cx_engine) {
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
        let generation = cx_engine.current_generation();
        store_outcome(
            store
                .watch_add(url, note, generation)
                .map(|id| json!({ "id": id, "created_gen": generation })),
        )
    }
}

fn h_watch_add<'a>(cx: &'a CallCtx<'a>, args: &'a Value) -> ToolFuture<'a> {
    Box::pin(watch_add_impl(cx.engine, args))
}

fn h_watch_list<'a>(cx: &'a CallCtx<'a>, _args: &'a Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let store = match store_of(cx.engine) {
            Ok(store) => store,
            Err(fault) => return fault,
        };
        store_outcome(store.watches())
    })
}

fn h_watch_remove<'a>(cx: &'a CallCtx<'a>, args: &'a Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let store = match store_of(cx.engine) {
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

fn h_store_export<'a>(cx: &'a CallCtx<'a>, _args: &'a Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let store = match store_of(cx.engine) {
            Ok(store) => store,
            Err(fault) => return fault,
        };
        match store.export_json() {
            Ok(json) => ToolOutcome::ok(json),
            Err(e) => ToolOutcome::err(e.to_string()),
        }
    })
}

// ── store dispatch trio (the kind-keyed default surface) ──────────────

fn h_store_put<'a>(cx: &'a CallCtx<'a>, args: &'a Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let kind = args.get("kind").and_then(Value::as_str).unwrap_or("");
        let record = args.get("record").cloned().unwrap_or(json!({}));
        match kind {
            "reminder" => reminder_add_impl(cx.engine, &record).await,
            "countdown" => countdown_add_impl(cx.engine, &record).await,
            "watch" => watch_add_impl(cx.engine, &record).await,
            other => ToolOutcome::err(format!(
                "store_put: unknown kind {other:?} (reminder | countdown | watch)"
            )),
        }
    })
}

fn h_store_query<'a>(cx: &'a CallCtx<'a>, args: &'a Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let kind = args.get("kind").and_then(Value::as_str).unwrap_or("");
        let store = match store_of(cx.engine) {
            Ok(store) => store,
            Err(fault) => return fault,
        };
        match kind {
            "reminder" => store_outcome(store.reminders_open()),
            "countdown" => store_outcome(store.countdowns_active()),
            "watch" => store_outcome(store.watches()),
            // The sovereignty view: everything, as plain JSON.
            "all" => match store.export_json() {
                Ok(json) => ToolOutcome::ok(json),
                Err(e) => ToolOutcome::err(e.to_string()),
            },
            other => ToolOutcome::err(format!(
                "store_query: unknown kind {other:?} (reminder | countdown | watch | all)"
            )),
        }
    })
}

fn h_store_close<'a>(cx: &'a CallCtx<'a>, args: &'a Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let kind = args.get("kind").and_then(Value::as_str).unwrap_or("");
        match kind {
            "reminder" => h_reminder_done(cx, args).await,
            "countdown" => h_countdown_retire(cx, args).await,
            "watch" => h_watch_remove(cx, args).await,
            other => ToolOutcome::err(format!(
                "store_close: unknown kind {other:?} (reminder | countdown | watch)"
            )),
        }
    })
}

// ── board dispatch (knowledge-board cards; opt-in `board` facet) ───────

fn h_board_put<'a>(cx: &'a CallCtx<'a>, args: &'a Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let store = match store_of(cx.engine) {
            Ok(store) => store,
            Err(fault) => return fault,
        };
        let Some(card_id) = args.get("card_id").and_then(Value::as_str) else {
            return ToolOutcome::err("board_put requires `card_id`");
        };
        let str_of = |k: &str| args.get(k).and_then(Value::as_str).map(ToString::to_string);

        let mut refs = Vec::new();
        if let Some(items) = args.get("blocked_on").and_then(Value::as_array) {
            for (i, v) in items.iter().enumerate() {
                if let Some(value) = v.as_str() {
                    refs.push(modulex_core::store::CardRef {
                        kind: "blocked_on".into(),
                        label: String::new(),
                        value: value.to_string(),
                        ordinal: i as i64,
                    });
                }
            }
        }
        if let Some(map) = args.get("refs").and_then(Value::as_object) {
            for (label, v) in map {
                if let Some(value) = v.as_str() {
                    refs.push(modulex_core::store::CardRef {
                        kind: "ref".into(),
                        label: label.clone(),
                        value: value.to_string(),
                        ordinal: 0,
                    });
                }
            }
        }

        let input = modulex_core::store::CardInput {
            card_id: card_id.to_string(),
            project: str_of("project").unwrap_or_default(),
            lane: str_of("lane").unwrap_or_else(|| "p2".to_string()),
            context: str_of("context").unwrap_or_default(),
            summary: str_of("summary").unwrap_or_default(),
            size: str_of("size"),
            status: str_of("status"),
            recurs: str_of("recurs"),
            expires: str_of("expires"),
            created: str_of("created"),
            updated: str_of("updated"),
            body: str_of("body").unwrap_or_default(),
            author: str_of("author"),
            source: str_of("source"),
            source_id: str_of("source_id"),
            refs,
        };
        // Mutation stamp: the generation current at call time — a counter.
        let generation = cx.engine.current_generation();
        store_outcome(
            store
                .card_add(&input, generation)
                .map(|id| json!({ "id": id, "card_id": card_id, "updated_gen": generation })),
        )
    })
}

fn h_board_query<'a>(cx: &'a CallCtx<'a>, args: &'a Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let store = match store_of(cx.engine) {
            Ok(store) => store,
            Err(fault) => return fault,
        };
        store_outcome(store.cards_query(
            args.get("project").and_then(Value::as_str),
            args.get("status").and_then(Value::as_str),
            args.get("lane").and_then(Value::as_str),
        ))
    })
}

fn h_board_move<'a>(cx: &'a CallCtx<'a>, args: &'a Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let store = match store_of(cx.engine) {
            Ok(store) => store,
            Err(fault) => return fault,
        };
        let Some(id) = args.get("id").and_then(Value::as_i64) else {
            return ToolOutcome::err("board_move requires integer `id`");
        };
        let Some(lane) = args.get("lane").and_then(Value::as_str) else {
            return ToolOutcome::err("board_move requires `lane`");
        };
        let context = args.get("context").and_then(Value::as_str);
        match store.card_move(id, lane, context, cx.engine.current_generation()) {
            Ok(true) => ToolOutcome::ok(format!("card #{id} moved to {lane}")),
            Ok(false) => ToolOutcome::err(format!("no card #{id}")),
            Err(e) => ToolOutcome::err(e.to_string()),
        }
    })
}

fn h_board_close<'a>(cx: &'a CallCtx<'a>, args: &'a Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let store = match store_of(cx.engine) {
            Ok(store) => store,
            Err(fault) => return fault,
        };
        let Some(id) = args.get("id").and_then(Value::as_i64) else {
            return ToolOutcome::err("board_close requires integer `id`");
        };
        let lane = args.get("lane").and_then(Value::as_str).unwrap_or("done");
        match store.card_close(id, lane, cx.engine.current_generation()) {
            Ok(true) => ToolOutcome::ok(format!("card #{id} closed to {lane}")),
            Ok(false) => ToolOutcome::err(format!("no card #{id}")),
            Err(e) => ToolOutcome::err(e.to_string()),
        }
    })
}

// ── mcp registry (downstream MCPs behind modulex; opt-in `mcp` facet) ──

fn h_mcp_register<'a>(cx: &'a CallCtx<'a>, args: &'a Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let store = match store_of(cx.engine) {
            Ok(store) => store,
            Err(fault) => return fault,
        };
        let action = args.get("action").and_then(Value::as_str).unwrap_or("add");
        match action {
            "list" => store_outcome(store.mcp_servers()),
            "remove" => {
                let Some(name) = args.get("name").and_then(Value::as_str) else {
                    return ToolOutcome::err("mcp_register remove requires `name`");
                };
                match store.mcp_unregister(name) {
                    Ok(true) => ToolOutcome::ok(format!("mcp server {name:?} unregistered")),
                    Ok(false) => ToolOutcome::err(format!("no registered mcp server {name:?}")),
                    Err(e) => ToolOutcome::err(e.to_string()),
                }
            }
            "add" => {
                let (Some(name), Some(command)) = (
                    args.get("name").and_then(Value::as_str),
                    args.get("command").and_then(Value::as_str),
                ) else {
                    return ToolOutcome::err("mcp_register add requires `name` and `command`");
                };
                let server_args = str_list(args, "args");
                let note = args.get("note").and_then(Value::as_str).unwrap_or("");
                // Mutation stamp: the generation current at call time — a counter.
                let generation = cx.engine.current_generation();
                store_outcome(
                    store
                        .mcp_register(name, command, &server_args, note, generation)
                        .map(|()| json!({ "name": name, "created_gen": generation })),
                )
            }
            other => ToolOutcome::err(format!(
                "mcp_register: unknown action {other:?} (add | list | remove)"
            )),
        }
    })
}

// ── discovery trio (the constant-size long tail) ───────────────────────

fn h_tool_search<'a>(cx: &'a CallCtx<'a>, args: &'a Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_lowercase();
        let terms: Vec<&str> = query.split_whitespace().collect();
        let matches: Vec<Value> = registry()
            .specs()
            .filter(|spec| !cx.policy.denies(spec.facet))
            .filter(|spec| {
                terms.is_empty()
                    || terms.iter().all(|t| {
                        spec.name.to_lowercase().contains(t)
                            || spec.description.to_lowercase().contains(t)
                    })
            })
            .map(|spec| {
                let summary = spec.description.split('.').next().unwrap_or("").trim();
                json!({
                    "name": spec.name,
                    "summary": summary,
                    "facet": spec.facet,
                    "mutates": spec.mutates,
                })
            })
            .collect();
        ToolOutcome::ok(json!({ "tools": matches }).to_string())
    })
}

fn h_tool_describe<'a>(cx: &'a CallCtx<'a>, args: &'a Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let Some(name) = args.get("name").and_then(Value::as_str) else {
            return ToolOutcome::err("tool_describe requires `name`");
        };
        match registry().specs().find(|s| s.name == name) {
            Some(spec) if !cx.policy.denies(spec.facet) => ToolOutcome::ok(
                json!({
                    "name": spec.name,
                    "description": spec.description,
                    "inputSchema": spec.input_schema,
                    "mutates": spec.mutates,
                    "facet": spec.facet,
                })
                .to_string(),
            ),
            _ => ToolOutcome::err(format!("unknown tool: {name}")),
        }
    })
}

fn h_tool_invoke<'a>(cx: &'a CallCtx<'a>, args: &'a Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let Some(name) = args.get("name").and_then(Value::as_str) else {
            return ToolOutcome::err("tool_invoke requires `name`");
        };
        // One-level guard: the discovery trio is not re-entrant.
        if matches!(name, "tool_invoke" | "tool_search" | "tool_describe") {
            return ToolOutcome::err("tool_invoke cannot invoke the discovery tools");
        }
        let inner = args.get("arguments").cloned().unwrap_or(json!({}));
        registry().call(cx.engine, cx.policy, name, &inner).await
    })
}

/// Inline routines are bounded — a runaway composer hits this, not the OS.
const EVAL_MAX_STEPS: usize = 32;

fn h_routine_eval<'a>(cx: &'a CallCtx<'a>, args: &'a Value) -> ToolFuture<'a> {
    Box::pin(async move {
        let Some(raw_steps) = args.get("steps").and_then(Value::as_array) else {
            return ToolOutcome::err("routine_eval requires a `steps` array");
        };
        if raw_steps.is_empty() {
            return ToolOutcome::err("routine_eval: `steps` must not be empty");
        }
        if raw_steps.len() > EVAL_MAX_STEPS {
            return ToolOutcome::err(format!(
                "routine_eval: at most {EVAL_MAX_STEPS} steps per eval ({} given)",
                raw_steps.len()
            ));
        }
        let mut steps = Vec::with_capacity(raw_steps.len());
        for (index, raw) in raw_steps.iter().enumerate() {
            match serde_json::from_value::<modulex_core::StepSpec>(raw.clone()) {
                Ok(step) => steps.push(step),
                Err(e) => {
                    return ToolOutcome::err(format!(
                        "routine_eval: steps[{index}] is not a valid step spec: {e}"
                    ))
                }
            }
        }
        let opts = RunOptions {
            only: Vec::new(),
            skip: Vec::new(),
            dry_run: args
                .get("dry_run")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        };
        match cx.engine.run_inline(steps, opts).await {
            Ok(report) => ToolOutcome::ok(render(&report, args)),
            Err(e) => engine_fault(&e),
        }
    })
}

// ── the registry ───────────────────────────────────────────────────────

/// The default-surface budget (#32): the number of tools the DEFAULT facet
/// policy may list, pinned by CI. Growing it is a deliberate change to this
/// constant with its own justification — never a side effect of a feature.
/// (Room is reserved within the budget for F4's `routine_eval`.)
pub const DEFAULT_TOOL_BUDGET: usize = 12;

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
                facet: "store-classic",
            },
            handler: h_reminder_add,
        },
        ToolEntry {
            spec: ToolSpec {
                name: "reminder_list",
                description: "List open reminders from the agent state store.",
                input_schema: json!({ "type": "object", "properties": {} }),
                mutates: false,
                facet: "store-classic",
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
                facet: "store-classic",
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
                facet: "store-classic",
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
                facet: "store-classic",
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
                facet: "store-classic",
            },
            handler: h_watch_add,
        },
        ToolEntry {
            spec: ToolSpec {
                name: "watch_list",
                description: "List registered URL watches.",
                input_schema: json!({ "type": "object", "properties": {} }),
                mutates: false,
                facet: "store-classic",
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
                facet: "store-classic",
            },
            handler: h_watch_remove,
        },
        ToolEntry {
            spec: ToolSpec {
                name: "store_put",
                description: "Register a record in the agent state store. Kind-keyed: \
                    kind=reminder {text, due?, recurrence?} | countdown {label, \
                    start_date, end_date, total_work_days?, display?} | watch {url, \
                    note?}. Returns {id, created_gen}.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "kind": { "type": "string",
                                  "enum": ["reminder", "countdown", "watch"] },
                        "record": { "type": "object",
                                    "description": "kind-specific fields; see tool_describe of the classic tool for full schemas" }
                    },
                    "required": ["kind", "record"]
                }),
                mutates: true,
                facet: "store",
            },
            handler: h_store_put,
        },
        ToolEntry {
            spec: ToolSpec {
                name: "store_query",
                description: "Read from the agent state store. kind=reminder (open) | \
                    countdown (active) | watch (all) | all (full plain-JSON export — \
                    the sovereignty view).",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "kind": { "type": "string",
                                  "enum": ["reminder", "countdown", "watch", "all"] }
                    },
                    "required": ["kind"]
                }),
                mutates: false,
                facet: "store",
            },
            handler: h_store_query,
        },
        ToolEntry {
            spec: ToolSpec {
                name: "store_close",
                description: "Close a record by id: kind=reminder (mark done) | \
                    countdown (retire) | watch (remove).",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "kind": { "type": "string",
                                  "enum": ["reminder", "countdown", "watch"] },
                        "id": { "type": "integer" }
                    },
                    "required": ["kind", "id"]
                }),
                mutates: true,
                facet: "store",
            },
            handler: h_store_close,
        },
        ToolEntry {
            spec: ToolSpec {
                name: "tool_search",
                description: "Search the FULL tool registry (all facets, including \
                    tools not in tools/list) by keywords. Returns [{name, summary, \
                    facet, mutates}]. Use tool_describe for a schema, tool_invoke to \
                    call.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string",
                                   "description": "keywords; empty lists everything" }
                    }
                }),
                mutates: false,
                facet: "core",
            },
            handler: h_tool_search,
        },
        ToolEntry {
            spec: ToolSpec {
                name: "tool_describe",
                description: "Full spec of one registered tool (any facet): \
                    description, inputSchema, mutates, facet. Schemas are disclosed \
                    at the moment of need, never preloaded.",
                input_schema: json!({
                    "type": "object",
                    "properties": { "name": { "type": "string" } },
                    "required": ["name"]
                }),
                mutates: false,
                facet: "core",
            },
            handler: h_tool_describe,
        },
        ToolEntry {
            spec: ToolSpec {
                name: "tool_invoke",
                description: "Invoke any registered tool by name (any facet not \
                    denied), with validated dispatch — the long tail of the surface \
                    without the schema cost in tools/list.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "arguments": { "type": "object" }
                    },
                    "required": ["name"]
                }),
                mutates: true, // can reach mutating tools
                facet: "core",
            },
            handler: h_tool_invoke,
        },
        ToolEntry {
            spec: ToolSpec {
                name: "routine_eval",
                description: "Run an INLINE routine: an ad-hoc array of step specs \
                    with identical semantics to a config routine — same leash (an \
                    inline step can never use a program outside the engine's grant), \
                    same soft failures, generation-stamped report stored as 'eval'. \
                    Compose multi-step queries instead of wishing for more tools. \
                    Step shape: {name, type, ...type-specific params} — see \
                    steps_list for types and their data schemas.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "steps": {
                            "type": "array",
                            "items": { "type": "object",
                                       "description": "a step spec: {name, type, ...params}" },
                            "minItems": 1,
                            "maxItems": 32
                        },
                        "dry_run": { "type": "boolean", "default": false },
                        "format": format_property()
                    },
                    "required": ["steps"]
                }),
                mutates: true, // executes arbitrary (leashed) configured work
                facet: "core",
            },
            handler: h_routine_eval,
        },
        ToolEntry {
            spec: ToolSpec {
                name: "store_export",
                description: "Export the whole agent state store as plain JSON \
                    (sovereignty: the content is never locked into SQLite).",
                input_schema: json!({ "type": "object", "properties": {} }),
                mutates: false,
                facet: "store-classic",
            },
            handler: h_store_export,
        },
        // ── board facet: opt-in, NOT in the default index (budget stays 12).
        // Reachable by default via tool_search/tool_invoke, the `board` step,
        // and routine_eval — at zero tools/list cost.
        ToolEntry {
            spec: ToolSpec {
                name: "board_put",
                description: "Create or update a knowledge-board card (upsert by \
                    card_id). Fields: card_id (req), project, lane (default p2), \
                    context, summary, body, size, status, recurs, expires, created, \
                    updated, refs {label: url}, blocked_on [url]. Returns {id, \
                    card_id, updated_gen}.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "card_id": { "type": "string" },
                        "project": { "type": "string" },
                        "lane": { "type": "string",
                                  "enum": ["p0", "p1", "p2", "done", "dropped"] },
                        "context": { "type": "string" },
                        "summary": { "type": "string" },
                        "body": { "type": "string" },
                        "size": { "type": "string" },
                        "status": { "type": "string" },
                        "recurs": { "type": "string" },
                        "expires": { "type": "string" },
                        "created": { "type": "string" },
                        "updated": { "type": "string" },
                        "refs": { "type": "object",
                                  "additionalProperties": { "type": "string" } },
                        "blocked_on": { "type": "array", "items": { "type": "string" } }
                    },
                    "required": ["card_id"]
                }),
                mutates: true,
                facet: "board",
            },
            handler: h_board_put,
        },
        ToolEntry {
            spec: ToolSpec {
                name: "board_query",
                description: "Query knowledge-board cards by any combination of \
                    lane / project / status (omit all for every card). Returns the \
                    cards with their refs and blocked_on entries.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "lane": { "type": "string" },
                        "project": { "type": "string" },
                        "status": { "type": "string" }
                    }
                }),
                mutates: false,
                facet: "board",
            },
            handler: h_board_query,
        },
        ToolEntry {
            spec: ToolSpec {
                name: "board_move",
                description: "Move a card to a lane (and optionally context). \
                    Moving into done/dropped stamps closed_gen; moving out clears it.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "integer" },
                        "lane": { "type": "string",
                                  "enum": ["p0", "p1", "p2", "done", "dropped"] },
                        "context": { "type": "string" }
                    },
                    "required": ["id", "lane"]
                }),
                mutates: true,
                facet: "board",
            },
            handler: h_board_move,
        },
        ToolEntry {
            spec: ToolSpec {
                name: "board_close",
                description: "Close a card by id — move it to done (default) or \
                    dropped, stamping closed_gen.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "integer" },
                        "lane": { "type": "string", "enum": ["done", "dropped"] }
                    },
                    "required": ["id"]
                }),
                mutates: true,
                facet: "board",
            },
            handler: h_board_close,
        },
        // ── mcp facet: opt-in, NOT in the default index (budget stays 12).
        // Reachable by default via tool_search/tool_invoke and the `mcp-query`
        // step — at zero tools/list cost. This is the credential-proxy
        // registry: a hotseat agent points at modulex, never at the
        // downstream's credentials.
        ToolEntry {
            spec: ToolSpec {
                name: "mcp_register",
                description: "Manage the registry of downstream MCP servers hidden \
                    behind modulex (issue #7 proxy). action=add {name, command, \
                    args?, note?} registers a server (stores the INVOCATION SHAPE \
                    only — never credentials; the mcp-query step injects credential \
                    references at spawn time and the command is exec-leashed) | \
                    list returns all servers | remove {name} unregisters. Returns \
                    {name, created_gen} on add.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "action": { "type": "string",
                                    "enum": ["add", "list", "remove"], "default": "add" },
                        "name": { "type": "string",
                                  "description": "registry name (add/remove)" },
                        "command": { "type": "string",
                                     "description": "program to spawn as a stdio MCP \
                                                     server; must be in the exec grant" },
                        "args": { "type": "array", "items": { "type": "string" },
                                  "description": "static argv for the command" },
                        "note": { "type": "string" }
                    }
                }),
                mutates: true,
                facet: "mcp",
            },
            handler: h_mcp_register,
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
            ("reminder_add", true, "store-classic"),
            ("reminder_list", false, "store-classic"),
            ("reminder_done", true, "store-classic"),
            ("countdown_add", true, "store-classic"),
            ("countdown_retire", true, "store-classic"),
            ("watch_add", true, "store-classic"),
            ("watch_list", false, "store-classic"),
            ("watch_remove", true, "store-classic"),
            ("store_put", true, "store"),
            ("store_query", false, "store"),
            ("store_close", true, "store"),
            ("tool_search", false, "core"),
            ("tool_describe", false, "core"),
            ("tool_invoke", true, "core"),
            ("routine_eval", true, "core"),
            ("store_export", false, "store-classic"),
            ("board_put", true, "board"),
            ("board_query", false, "board"),
            ("board_move", true, "board"),
            ("board_close", true, "board"),
            ("mcp_register", true, "mcp"),
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

    /// THE BUDGET PIN (#32): the default facet policy lists at most
    /// DEFAULT_TOOL_BUDGET tools, with the exact set pinned. Progressive
    /// disclosure is enforced here, not hoped for.
    #[test]
    fn default_surface_fits_the_budget() {
        let policy =
            crate::facets::FacetPolicy::resolve(None, &modulex_core::config::McpConfig::default());
        let listed: Vec<&str> = registry()
            .specs()
            .filter(|s| policy.exposes(s.facet))
            .map(|s| s.name)
            .collect();
        assert!(
            listed.len() <= DEFAULT_TOOL_BUDGET,
            "default surface ({}) exceeds the budget ({DEFAULT_TOOL_BUDGET}): {listed:?}",
            listed.len()
        );
        assert_eq!(
            listed,
            vec![
                "routine_run",
                "routine_list",
                "step_run",
                "report_get",
                "steps_list",
                "store_put",
                "store_query",
                "store_close",
                "tool_search",
                "tool_describe",
                "tool_invoke",
                "routine_eval",
            ],
            "the default index changed — a deliberate, reviewed event"
        );
    }

    /// The board facet is OPT-IN: absent from the default index (the budget
    /// stays 12), but listed when exposed and always discoverable.
    #[test]
    fn board_facet_is_opt_in_but_discoverable() {
        use modulex_core::config::McpConfig;

        // Default: board tools are NOT listed.
        let default = crate::facets::FacetPolicy::resolve(None, &McpConfig::default());
        let listed_by_default: Vec<&str> = registry()
            .specs()
            .filter(|s| default.exposes(s.facet))
            .map(|s| s.name)
            .collect();
        assert!(
            !listed_by_default.iter().any(|n| n.starts_with("board_")),
            "board tools must not appear in the default index"
        );

        // Opt in via expose: the four board tools list.
        let opted =
            crate::facets::FacetPolicy::resolve(Some("core,store,board"), &McpConfig::default());
        let board_listed = registry()
            .specs()
            .filter(|s| opted.exposes(s.facet) && s.facet == "board")
            .count();
        assert_eq!(board_listed, 4, "opting into `board` lists all four tools");

        // Discoverable even when not listed (callability is broader than listing).
        assert!(
            registry()
                .specs()
                .any(|s| s.name == "board_put" && !default.denies(s.facet)),
            "board_put is reachable via tool_search/tool_invoke by default"
        );
    }

    /// The mcp (credential-proxy) facet is OPT-IN: absent from the default
    /// index (the budget stays 12), but listed when exposed and always
    /// discoverable via tool_search/tool_invoke.
    #[test]
    fn mcp_facet_is_opt_in_but_discoverable() {
        use modulex_core::config::McpConfig;

        let default = crate::facets::FacetPolicy::resolve(None, &McpConfig::default());
        assert!(
            !registry()
                .specs()
                .filter(|s| default.exposes(s.facet))
                .any(|s| s.name == "mcp_register"),
            "mcp_register must not appear in the default index"
        );

        let opted =
            crate::facets::FacetPolicy::resolve(Some("core,store,mcp"), &McpConfig::default());
        assert!(
            registry()
                .specs()
                .any(|s| s.name == "mcp_register" && opted.exposes(s.facet)),
            "opting into `mcp` lists mcp_register"
        );

        // Discoverable (callable) by default even though it is not listed.
        assert!(
            registry()
                .specs()
                .any(|s| s.name == "mcp_register" && !default.denies(s.facet)),
            "mcp_register is reachable via tool_search/tool_invoke by default"
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
