//! The `reminders` step — surfaces open reminders from the agent state
//! store: overdue first, then due today, then dated upcoming, then undated.
//! Recurring reminders carry a `[daily]`-style tag.

use async_trait::async_trait;
use chrono::NaiveDate;

use crate::config::StepSpec;
use crate::report::StepResult;
use crate::step::{RunContext, StepHandler};
use crate::store::Reminder;

/// `reminders`: open reminders from the store.
pub struct Reminders;

#[async_trait]
impl StepHandler for Reminders {
    fn type_name(&self) -> &'static str {
        "reminders"
    }

    fn required_programs(&self, _spec: &StepSpec) -> Vec<String> {
        vec![]
    }

    async fn run(&self, spec: &StepSpec, cx: &RunContext) -> StepResult {
        let Some(store) = &cx.store else {
            return StepResult::skip(&spec.name, &spec.step_type, "agent state store unavailable");
        };
        if cx.dry_run {
            return StepResult::ok(
                &spec.name,
                &spec.step_type,
                "[dry-run] would list open reminders from the store",
            );
        }
        match store.reminders_open() {
            Ok(reminders) => {
                let today = chrono::Local::now().date_naive();
                StepResult::ok(&spec.name, &spec.step_type, render(&reminders, today))
            }
            Err(e) => StepResult::fail(&spec.name, &spec.step_type, e.to_string()),
        }
    }
}

fn parse_iso(text: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(text, "%Y-%m-%d").ok()
}

/// Pure renderer, factored so tests pin `today`.
fn render(reminders: &[Reminder], today: NaiveDate) -> String {
    if reminders.is_empty() {
        return "(no open reminders)".to_string();
    }

    let mut overdue = Vec::new();
    let mut due_today = Vec::new();
    let mut upcoming = Vec::new();
    let mut undated = Vec::new();

    for r in reminders {
        let tag = r
            .recurrence
            .as_deref()
            .map(|recurrence| format!(" [{recurrence}]"))
            .unwrap_or_default();
        match r.due.as_deref().and_then(parse_iso) {
            Some(due) if due < today => overdue.push(format!(
                "  OVERDUE ({} days): #{} {}{tag}",
                (today - due).num_days(),
                r.id,
                r.text
            )),
            Some(due) if due == today => {
                due_today.push(format!("  due today: #{} {}{tag}", r.id, r.text));
            }
            Some(due) => upcoming.push(format!("  {due}: #{} {}{tag}", r.id, r.text)),
            None => undated.push(format!("  - #{} {}{tag}", r.id, r.text)),
        }
    }

    let mut lines = Vec::new();
    lines.extend(overdue);
    lines.extend(due_today);
    lines.extend(upcoming);
    lines.extend(undated);
    lines.push(format!("({} open)", reminders.len()));
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use agent_bridle_core::Caveats;

    use super::*;
    use crate::config::Config;
    use crate::exec::test_support::{gate_with, MockSpawner};
    use crate::store::Store;

    fn cx_with(store: Option<Arc<Store>>) -> RunContext {
        RunContext {
            config: Arc::new(Config::default()),
            dry_run: false,
            generation: 5,
            exec: gate_with(&Caveats::top(), Arc::new(MockSpawner::default())),
            prior: Vec::new(),
            store,
        }
    }

    fn spec() -> StepSpec {
        toml::from_str("name=\"reminders\"\ntype=\"reminders\"").unwrap()
    }

    #[tokio::test]
    async fn missing_store_soft_skips() {
        let result = Reminders.run(&spec(), &cx_with(None)).await;
        assert!(result.skipped);
    }

    #[tokio::test]
    async fn lists_open_reminders_through_the_store() {
        let store = Arc::new(Store::in_memory().unwrap());
        store.reminder_add("ship it", None, None, 1).unwrap();
        let done = store.reminder_add("old", None, None, 1).unwrap();
        store.reminder_done(done, 2).unwrap();

        let result = Reminders.run(&spec(), &cx_with(Some(store))).await;
        assert!(result.success);
        assert!(result.output.contains("ship it"));
        assert!(!result.output.contains("old"));
        assert!(result.output.contains("(1 open)"));
    }

    #[test]
    fn render_buckets_and_tags() {
        let today = NaiveDate::parse_from_str("2026-06-05", "%Y-%m-%d").unwrap();
        let reminders = vec![
            Reminder {
                id: 1,
                text: "rotate token".into(),
                due: Some("2026-06-01".into()),
                recurrence: None,
                created_gen: 1,
                done_gen: None,
            },
            Reminder {
                id: 2,
                text: "standup".into(),
                due: Some("2026-06-05".into()),
                recurrence: Some("daily".into()),
                created_gen: 1,
                done_gen: None,
            },
            Reminder {
                id: 3,
                text: "CFP".into(),
                due: Some("2026-07-01".into()),
                recurrence: None,
                created_gen: 1,
                done_gen: None,
            },
            Reminder {
                id: 4,
                text: "someday".into(),
                due: None,
                recurrence: None,
                created_gen: 1,
                done_gen: None,
            },
        ];
        let body = render(&reminders, today);
        let lines: Vec<&str> = body.lines().collect();
        assert!(lines[0].contains("OVERDUE (4 days): #1 rotate token"));
        assert!(lines[1].contains("due today: #2 standup [daily]"));
        assert!(lines[2].contains("2026-07-01: #3 CFP"));
        assert!(lines[3].contains("- #4 someday"));
        assert!(lines[4].contains("(4 open)"));
    }

    #[test]
    fn render_empty() {
        let today = chrono::Local::now().date_naive();
        assert_eq!(render(&[], today), "(no open reminders)");
    }
}
