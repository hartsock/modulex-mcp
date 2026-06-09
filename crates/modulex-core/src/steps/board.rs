//! Board steps.
//!
//! - `board-scan` and `chores-check` are pure **filesystem** directory scans —
//!   no subprocesses, no store. `board-scan` lists `*.md` task stems per
//!   configured lane; `chores-check` reports `due:` lines that are due/overdue.
//! - `board` is the **store-backed** view: open cards grouped by lane from the
//!   agent state store (the operational knowledge board). It is the counterpart
//!   of `board-scan` for boards synced into the store via `import_dir`.

use async_trait::async_trait;
use chrono::NaiveDate;

use crate::config::{expand_tilde, StepSpec};
use crate::report::StepResult;
use crate::step::{RunContext, StepHandler};
use crate::store::Card;

/// `board-scan`: `### lane (N tasks)` plus the task stems, per lane.
pub struct BoardScan;

#[async_trait]
impl StepHandler for BoardScan {
    fn type_name(&self) -> &'static str {
        "board-scan"
    }

    fn description(&self) -> &'static str {
        "Task stems per configured board lane"
    }

    fn data_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["lanes"],
            "properties": {
                "lanes": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["lane", "found", "tasks"],
                        "properties": {
                            "lane": { "type": "string" },
                            "found": { "type": "boolean", "description": "lane directory exists" },
                            "tasks": { "type": "array", "items": { "type": "string" } }
                        }
                    }
                }
            }
        })
    }

    fn required_programs(&self, _spec: &StepSpec) -> Vec<String> {
        vec![]
    }

    async fn run(&self, spec: &StepSpec, cx: &RunContext) -> StepResult {
        let board = &cx.config.board;
        if board.path.is_empty() {
            let mut result =
                StepResult::ok(&spec.name, &spec.step_type, "No board path configured.");
            result.data = Some(serde_json::json!({ "lanes": [] }));
            return result;
        }
        let board_path = expand_tilde(&board.path);

        if cx.dry_run {
            return StepResult::ok(
                &spec.name,
                &spec.step_type,
                format!(
                    "[dry-run] would scan board at {} lanes: {:?}",
                    board_path.display(),
                    board.lanes
                ),
            );
        }

        let mut lines = Vec::new();
        let mut data_lanes = Vec::new();
        for lane in &board.lanes {
            let lane_dir = board_path.join(lane);
            if !lane_dir.is_dir() {
                lines.push(format!("### {lane}: (directory not found)"));
                data_lanes.push(serde_json::json!({
                    "lane": lane, "found": false, "tasks": [],
                }));
                continue;
            }
            let mut tasks: Vec<String> = std::fs::read_dir(&lane_dir)
                .map(|entries| {
                    entries
                        .filter_map(Result::ok)
                        .filter(|e| e.path().extension().is_some_and(|ext| ext == "md"))
                        .filter_map(|e| {
                            e.path()
                                .file_stem()
                                .map(|s| s.to_string_lossy().into_owned())
                        })
                        .collect()
                })
                .unwrap_or_default();
            tasks.sort();
            lines.push(format!("### {lane} ({} tasks)", tasks.len()));
            for task in &tasks {
                lines.push(format!("  - {task}"));
            }
            data_lanes.push(serde_json::json!({
                "lane": lane, "found": true, "tasks": tasks,
            }));
        }

        let mut result = StepResult::ok(
            &spec.name,
            &spec.step_type,
            if lines.is_empty() {
                "(no board data)".to_string()
            } else {
                lines.join("\n")
            },
        );
        result.data = Some(serde_json::json!({ "lanes": data_lanes }));
        result
    }
}

/// One `due:` finding in a chores file.
struct DueItem {
    chore: String,
    due: NaiveDate,
}

/// Scan a directory's `*.md` files for `due: YYYY-MM-DD` lines.
fn scan_due_items(dir: &std::path::Path) -> Vec<DueItem> {
    let mut items = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return items;
    };
    let mut paths: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "md"))
        .collect();
    paths.sort();
    for path in paths {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let chore = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        for line in text.lines() {
            let trimmed = line.trim();
            if let Some(value) = trimmed
                .strip_prefix("due:")
                .or_else(|| trimmed.strip_prefix("Due:"))
            {
                if let Ok(due) = NaiveDate::parse_from_str(value.trim(), "%Y-%m-%d") {
                    items.push(DueItem {
                        chore: chore.clone(),
                        due,
                    });
                }
            }
        }
    }
    items
}

/// `chores-check`: due and overdue chores from `due:` lines.
pub struct ChoresCheck;

#[async_trait]
impl StepHandler for ChoresCheck {
    fn type_name(&self) -> &'static str {
        "chores-check"
    }

    fn description(&self) -> &'static str {
        "Due and overdue chores from `due:` lines in the chores directory"
    }

    fn data_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["overdue", "due_today", "upcoming"],
            "properties": {
                "overdue": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["chore", "days_overdue"],
                        "properties": {
                            "chore": { "type": "string" },
                            "days_overdue": { "type": "integer", "minimum": 1 }
                        }
                    }
                },
                "due_today": { "type": "array", "items": { "type": "string" } },
                "upcoming": { "type": "integer", "minimum": 0 }
            }
        })
    }

    fn required_programs(&self, _spec: &StepSpec) -> Vec<String> {
        vec![]
    }

    async fn run(&self, spec: &StepSpec, cx: &RunContext) -> StepResult {
        let chores = &cx.config.chores;
        if chores.path.is_empty() {
            let mut result =
                StepResult::ok(&spec.name, &spec.step_type, "No chores path configured.");
            result.data = Some(serde_json::json!({
                "overdue": [], "due_today": [], "upcoming": 0,
            }));
            return result;
        }
        let dir = expand_tilde(&chores.path);

        if cx.dry_run {
            return StepResult::ok(
                &spec.name,
                &spec.step_type,
                format!("[dry-run] would check chores at {}", dir.display()),
            );
        }
        if !dir.is_dir() {
            return StepResult::skip(
                &spec.name,
                &spec.step_type,
                format!("chores directory not found: {}", dir.display()),
            );
        }

        let today = chrono::Local::now().date_naive();
        let items = scan_due_items(&dir);
        let mut result = StepResult::ok(&spec.name, &spec.step_type, render_chores(&items, today));
        result.data = Some(chores_data(&items, today));
        result
    }
}

/// Typed buckets for the data contract (same bucketing as the renderer).
fn chores_data(items: &[DueItem], today: NaiveDate) -> serde_json::Value {
    let mut overdue = Vec::new();
    let mut due_today = Vec::new();
    let mut upcoming = 0usize;
    for item in items {
        if item.due < today {
            overdue.push(serde_json::json!({
                "chore": item.chore,
                "days_overdue": (today - item.due).num_days(),
            }));
        } else if item.due == today {
            due_today.push(item.chore.clone());
        } else {
            upcoming += 1;
        }
    }
    serde_json::json!({ "overdue": overdue, "due_today": due_today, "upcoming": upcoming })
}

/// Pure renderer, factored so tests pin `today`.
fn render_chores(items: &[DueItem], today: NaiveDate) -> String {
    let mut overdue = Vec::new();
    let mut due_today = Vec::new();
    let mut upcoming = 0usize;
    for item in items {
        if item.due < today {
            overdue.push(format!(
                "  OVERDUE ({} days): {}",
                (today - item.due).num_days(),
                item.chore
            ));
        } else if item.due == today {
            due_today.push(format!("  due today: {}", item.chore));
        } else {
            upcoming += 1;
        }
    }

    if overdue.is_empty() && due_today.is_empty() {
        return format!("No chores due. ({upcoming} upcoming)");
    }
    let mut lines = Vec::new();
    lines.extend(overdue);
    lines.extend(due_today);
    lines.push(format!("({upcoming} upcoming)"));
    lines.join("\n")
}

/// The default lanes shown when a `board` step has no `lane` param: the open
/// work lanes, in priority order (closed lanes are surfaced only on request).
const OPEN_LANES: &[&str] = &["p0", "p1", "p2"];

/// `board`: open cards grouped by lane, from the agent state store.
///
/// Params (all optional): `lane` (a single lane to show), `project` (filter).
/// With no `lane`, shows the open lanes (`p0`/`p1`/`p2`).
pub struct Board;

#[async_trait]
impl StepHandler for Board {
    fn type_name(&self) -> &'static str {
        "board"
    }

    fn description(&self) -> &'static str {
        "Open knowledge-board cards grouped by lane, from the agent state store"
    }

    fn data_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["lanes", "open"],
            "properties": {
                "open": { "type": "integer", "minimum": 0 },
                "lanes": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["lane", "cards"],
                        "properties": {
                            "lane": { "type": "string" },
                            "cards": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "required": ["card_id", "summary"],
                                    "properties": {
                                        "card_id": { "type": "string" },
                                        "project": { "type": "string" },
                                        "summary": { "type": "string" },
                                        "status":  { "type": ["string", "null"] },
                                        "size":    { "type": ["string", "null"] },
                                        "blocked": { "type": "boolean" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        })
    }

    fn required_programs(&self, _spec: &StepSpec) -> Vec<String> {
        vec![]
    }

    async fn run(&self, spec: &StepSpec, cx: &RunContext) -> StepResult {
        let Some(store) = &cx.store else {
            return StepResult::skip(&spec.name, &spec.step_type, "agent state store unavailable");
        };
        let lane = spec.param_str("lane");
        let project = spec.param_str("project");

        if cx.dry_run {
            return StepResult::ok(
                &spec.name,
                &spec.step_type,
                "[dry-run] would list open board cards from the store",
            );
        }

        let cards = match store.cards_query(project, None, lane) {
            Ok(cards) => cards,
            Err(e) => return StepResult::fail(&spec.name, &spec.step_type, e.to_string()),
        };

        // Which lanes to show, in order: the requested one, else the open lanes.
        let lanes: Vec<String> = match lane {
            Some(l) => vec![l.to_string()],
            None => OPEN_LANES.iter().map(ToString::to_string).collect(),
        };

        let mut result = StepResult::ok(&spec.name, &spec.step_type, render(&cards, &lanes));
        result.data = Some(board_data(&cards, &lanes));
        result
    }
}

/// Cards in a given lane, preserving store order.
fn cards_in<'a>(cards: &'a [Card], lane: &str) -> Vec<&'a Card> {
    cards.iter().filter(|c| c.lane == lane).collect()
}

/// Typed payload for the data contract.
fn board_data(cards: &[Card], lanes: &[String]) -> serde_json::Value {
    let lane_views: Vec<serde_json::Value> = lanes
        .iter()
        .map(|lane| {
            let entries: Vec<serde_json::Value> = cards_in(cards, lane)
                .into_iter()
                .map(|c| {
                    serde_json::json!({
                        "card_id": c.card_id,
                        "project": c.project,
                        "summary": c.summary,
                        "status": c.status,
                        "size": c.size,
                        "blocked": c.closed_gen.is_none()
                            && c.refs.iter().any(|r| r.kind == "blocked_on"),
                    })
                })
                .collect();
            serde_json::json!({ "lane": lane, "cards": entries })
        })
        .collect();
    let open = cards
        .iter()
        .filter(|c| lanes.iter().any(|l| l == &c.lane))
        .count();
    serde_json::json!({ "lanes": lane_views, "open": open })
}

/// Pure renderer, factored so tests pin the output shape.
fn render(cards: &[Card], lanes: &[String]) -> String {
    let mut lines = Vec::new();
    for lane in lanes {
        let entries = cards_in(cards, lane);
        lines.push(format!("### {lane} ({} cards)", entries.len()));
        for c in entries {
            let blocked = if c.refs.iter().any(|r| r.kind == "blocked_on") {
                " [blocked]"
            } else {
                ""
            };
            lines.push(format!("  - {}: {}{blocked}", c.card_id, c.summary));
        }
    }
    if lines.is_empty() {
        "(no board cards)".to_string()
    } else {
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use agent_bridle_core::Caveats;

    use super::*;
    use crate::config::Config;
    use crate::exec::test_support::{gate_with, MockSpawner};
    use crate::store::{CardInput, Store};

    fn cx_with(config: Config) -> RunContext {
        RunContext {
            config: Arc::new(config),
            dry_run: false,
            generation: 1,
            exec: gate_with(&Caveats::top(), Arc::new(MockSpawner::default())),
            prior: Vec::new(),
            store: None,
        }
    }

    fn unique_dir(tag: &str) -> std::path::PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "modulex-board-test-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn date(text: &str) -> NaiveDate {
        NaiveDate::parse_from_str(text, "%Y-%m-%d").unwrap()
    }

    #[tokio::test]
    async fn board_scan_lists_lane_tasks_and_flags_missing_lanes() {
        let root = unique_dir("lanes");
        let p0 = root.join("p0");
        std::fs::create_dir_all(&p0).unwrap();
        std::fs::write(p0.join("fix-the-leak.md"), "x").unwrap();
        std::fs::write(p0.join("notes.txt"), "not a task").unwrap();

        let mut config = Config::default();
        config.board.path = root.to_string_lossy().into_owned();
        config.board.lanes = vec!["p0".into(), "ghost".into()];

        let spec: StepSpec = toml::from_str("name=\"b\"\ntype=\"board-scan\"").unwrap();
        let result = BoardScan.run(&spec, &cx_with(config)).await;
        assert!(result.success);
        assert!(result.output.contains("### p0 (1 tasks)"));
        assert!(result.output.contains("  - fix-the-leak"));
        assert!(result.output.contains("### ghost: (directory not found)"));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn chores_renderer_buckets_overdue_today_upcoming() {
        let items = vec![
            DueItem {
                chore: "water plants".into(),
                due: date("2026-06-01"),
            },
            DueItem {
                chore: "backups".into(),
                due: date("2026-06-05"),
            },
            DueItem {
                chore: "rotate keys".into(),
                due: date("2026-07-01"),
            },
        ];
        let body = render_chores(&items, date("2026-06-05"));
        assert!(body.contains("OVERDUE (4 days): water plants"));
        assert!(body.contains("due today: backups"));
        assert!(body.contains("(1 upcoming)"));
    }

    #[test]
    fn chores_renderer_all_clear() {
        let items = vec![DueItem {
            chore: "later".into(),
            due: date("2026-07-01"),
        }];
        assert_eq!(
            render_chores(&items, date("2026-06-05")),
            "No chores due. (1 upcoming)"
        );
    }

    #[tokio::test]
    async fn chores_check_parses_due_lines_from_markdown() {
        let dir = unique_dir("chores");
        std::fs::write(dir.join("old-task.md"), "# Old\ndue: 2020-01-01\n").unwrap();
        std::fs::write(dir.join("future.md"), "Due: 2999-12-31\n").unwrap();
        std::fs::write(dir.join("no-date.md"), "nothing here\n").unwrap();

        let mut config = Config::default();
        config.chores.path = dir.to_string_lossy().into_owned();

        let spec: StepSpec = toml::from_str("name=\"c\"\ntype=\"chores-check\"").unwrap();
        let result = ChoresCheck.run(&spec, &cx_with(config)).await;
        assert!(result.success);
        assert!(result.output.contains("OVERDUE"));
        assert!(result.output.contains("old-task"));
        assert!(result.output.contains("(1 upcoming)"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn missing_chores_dir_soft_skips() {
        let mut config = Config::default();
        config.chores.path = "/nonexistent/chores/dir".into();
        let spec: StepSpec = toml::from_str("name=\"c\"\ntype=\"chores-check\"").unwrap();
        let result = ChoresCheck.run(&spec, &cx_with(config)).await;
        assert!(result.skipped);
    }

    // ── board (store-backed) ───────────────────────────────────────────

    fn cx_with_store(store: Option<Arc<Store>>) -> RunContext {
        RunContext {
            config: Arc::new(Config::default()),
            dry_run: false,
            generation: 1,
            exec: gate_with(&Caveats::top(), Arc::new(MockSpawner::default())),
            prior: Vec::new(),
            store,
        }
    }

    fn seed(store: &Store, card_id: &str, lane: &str, summary: &str) {
        store
            .card_add(
                &CardInput {
                    card_id: card_id.into(),
                    project: "homelab".into(),
                    lane: lane.into(),
                    summary: summary.into(),
                    ..Default::default()
                },
                1,
            )
            .unwrap();
    }

    #[tokio::test]
    async fn board_step_missing_store_soft_skips() {
        let spec: StepSpec = toml::from_str("name=\"b\"\ntype=\"board\"").unwrap();
        let result = Board.run(&spec, &cx_with_store(None)).await;
        assert!(result.skipped);
    }

    #[tokio::test]
    async fn board_step_lists_open_lanes_only() {
        let store = Arc::new(Store::in_memory().unwrap());
        seed(&store, "a", "p0", "active thing");
        seed(&store, "b", "p2", "backlog thing");
        seed(&store, "c", "done", "finished thing");

        let spec: StepSpec = toml::from_str("name=\"b\"\ntype=\"board\"").unwrap();
        let result = Board.run(&spec, &cx_with_store(Some(store))).await;
        assert!(result.success);
        assert!(result.output.contains("### p0 (1 cards)"));
        assert!(result.output.contains("active thing"));
        assert!(result.output.contains("### p2 (1 cards)"));
        assert!(
            !result.output.contains("finished thing"),
            "done lane excluded by default"
        );

        let data = result.data.unwrap();
        assert_eq!(data["open"], 2, "p0 + p2, not done");
    }

    #[tokio::test]
    async fn board_step_honors_lane_param() {
        let store = Arc::new(Store::in_memory().unwrap());
        seed(&store, "a", "p0", "active thing");
        seed(&store, "c", "done", "finished thing");

        let spec: StepSpec = toml::from_str("name=\"b\"\ntype=\"board\"\nlane=\"done\"").unwrap();
        let result = Board.run(&spec, &cx_with_store(Some(store))).await;
        assert!(result.output.contains("### done (1 cards)"));
        assert!(result.output.contains("finished thing"));
    }

    #[test]
    fn board_render_flags_blocked_cards() {
        let cards = vec![Card {
            id: 1,
            card_id: "x".into(),
            project: "p".into(),
            lane: "p0".into(),
            context: String::new(),
            summary: "do the thing".into(),
            size: None,
            status: Some("blocked".into()),
            recurs: None,
            expires: None,
            created: None,
            updated: None,
            body: String::new(),
            author: None,
            source: None,
            source_id: None,
            created_gen: 1,
            updated_gen: 1,
            closed_gen: None,
            refs: vec![crate::store::CardRef {
                kind: "blocked_on".into(),
                label: String::new(),
                value: "https://x/1".into(),
                ordinal: 0,
            }],
        }];
        let body = render(&cards, &["p0".to_string()]);
        assert!(body.contains("### p0 (1 cards)"));
        assert!(body.contains("- x: do the thing [blocked]"));
    }
}
