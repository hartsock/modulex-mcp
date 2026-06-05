//! Filesystem board steps: `board-scan` and `chores-check`.
//!
//! Pure directory scans — no subprocesses. `board-scan` lists `*.md` tasks
//! per configured lane. `chores-check` looks for `due: YYYY-MM-DD` lines in
//! the chores directory's markdown files and reports what's due or overdue
//! (today's date is display math, never coordination).

use async_trait::async_trait;
use chrono::NaiveDate;

use crate::config::{expand_tilde, StepSpec};
use crate::report::StepResult;
use crate::step::{RunContext, StepHandler};

/// `board-scan`: `### lane (N tasks)` plus the task stems, per lane.
pub struct BoardScan;

#[async_trait]
impl StepHandler for BoardScan {
    fn type_name(&self) -> &'static str {
        "board-scan"
    }

    fn required_programs(&self, _spec: &StepSpec) -> Vec<String> {
        vec![]
    }

    async fn run(&self, spec: &StepSpec, cx: &RunContext) -> StepResult {
        let board = &cx.config.board;
        if board.path.is_empty() {
            return StepResult::ok(&spec.name, &spec.step_type, "No board path configured.");
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
        for lane in &board.lanes {
            let lane_dir = board_path.join(lane);
            if !lane_dir.is_dir() {
                lines.push(format!("### {lane}: (directory not found)"));
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
            for task in tasks {
                lines.push(format!("  - {task}"));
            }
        }

        StepResult::ok(
            &spec.name,
            &spec.step_type,
            if lines.is_empty() {
                "(no board data)".to_string()
            } else {
                lines.join("\n")
            },
        )
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

    fn required_programs(&self, _spec: &StepSpec) -> Vec<String> {
        vec![]
    }

    async fn run(&self, spec: &StepSpec, cx: &RunContext) -> StepResult {
        let chores = &cx.config.chores;
        if chores.path.is_empty() {
            return StepResult::ok(&spec.name, &spec.step_type, "No chores path configured.");
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
        let body = render_chores(&scan_due_items(&dir), today);
        StepResult::ok(&spec.name, &spec.step_type, body)
    }
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use agent_bridle_core::Caveats;

    use super::*;
    use crate::config::Config;
    use crate::exec::test_support::{gate_with, MockSpawner};

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
}
