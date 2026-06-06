//! Pure date-math steps: `deadline-calc` and `countdown-calc`.
//!
//! These spawn nothing. Today's date is read once per run for *display*
//! arithmetic — it is never used to identify or order anything (report
//! identity is the generation counter).

use async_trait::async_trait;
use chrono::{Datelike, NaiveDate};

use crate::config::StepSpec;
use crate::report::StepResult;
use crate::step::{RunContext, StepHandler};

/// Count weekdays in `[start, end)` — Monday..Friday.
#[must_use]
pub fn work_days_between(start: NaiveDate, end: NaiveDate) -> u32 {
    if end <= start {
        return 0;
    }
    let mut count = 0;
    let mut current = start;
    while current < end {
        if current.weekday().num_days_from_monday() < 5 {
            count += 1;
        }
        current = current.succ_opt().expect("date overflow");
    }
    count
}

fn parse_iso(text: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(text, "%Y-%m-%d").ok()
}

fn today() -> NaiveDate {
    chrono::Local::now().date_naive()
}

/// `deadline-calc`: days remaining per configured deadline; past deadlines
/// are dropped.
pub struct DeadlineCalc;

#[async_trait]
impl StepHandler for DeadlineCalc {
    fn type_name(&self) -> &'static str {
        "deadline-calc"
    }

    fn description(&self) -> &'static str {
        "Days remaining per configured deadline; past deadlines dropped"
    }

    fn data_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["deadlines", "invalid"],
            "properties": {
                "deadlines": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["label", "date", "days_left"],
                        "properties": {
                            "label": { "type": "string" },
                            "date": { "type": "string", "description": "ISO YYYY-MM-DD" },
                            "end_date": { "type": ["string", "null"] },
                            "days_left": { "type": "integer", "minimum": 0 },
                            "notes": { "type": "string" }
                        }
                    }
                },
                "invalid": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "labels with unparsable dates"
                }
            }
        })
    }

    fn required_programs(&self, _spec: &StepSpec) -> Vec<String> {
        vec![]
    }

    async fn run(&self, spec: &StepSpec, cx: &RunContext) -> StepResult {
        let deadlines = &cx.config.deadlines;
        if deadlines.is_empty() {
            let mut result =
                StepResult::ok(&spec.name, &spec.step_type, "No deadlines configured.");
            result.data = Some(serde_json::json!({ "deadlines": [], "invalid": [] }));
            return result;
        }
        if cx.dry_run {
            let labels: Vec<&str> = deadlines.iter().map(|d| d.label.as_str()).collect();
            return StepResult::ok(
                &spec.name,
                &spec.step_type,
                format!(
                    "[dry-run] would calculate deadlines for: {}",
                    labels.join(", ")
                ),
            );
        }

        let computed = compute_deadlines(deadlines, today());
        let mut result = StepResult::ok(&spec.name, &spec.step_type, render_deadlines(&computed));
        result.data = Some(deadlines_data(&computed));
        result
    }
}

/// One computed upcoming deadline (or an invalid entry).
enum DeadlineRow {
    Upcoming {
        label: String,
        date: String,
        end_date: Option<String>,
        days_left: i64,
        notes: String,
    },
    Invalid(String),
}

/// Pure computation, factored so tests pin `today`. Past deadlines dropped.
fn compute_deadlines(
    deadlines: &[crate::config::DeadlineEntry],
    today: NaiveDate,
) -> Vec<DeadlineRow> {
    let mut rows = Vec::new();
    for dl in deadlines {
        let Some(target) = parse_iso(&dl.date) else {
            rows.push(DeadlineRow::Invalid(dl.label.clone()));
            continue;
        };
        if target < today {
            continue; // past deadline
        }
        rows.push(DeadlineRow::Upcoming {
            label: dl.label.clone(),
            date: dl.date.clone(),
            end_date: dl.end_date.clone(),
            days_left: (target - today).num_days(),
            notes: dl.notes.clone(),
        });
    }
    rows
}

fn deadlines_data(rows: &[DeadlineRow]) -> serde_json::Value {
    let mut deadlines = Vec::new();
    let mut invalid = Vec::new();
    for row in rows {
        match row {
            DeadlineRow::Upcoming {
                label,
                date,
                end_date,
                days_left,
                notes,
            } => deadlines.push(serde_json::json!({
                "label": label, "date": date, "end_date": end_date,
                "days_left": days_left, "notes": notes,
            })),
            DeadlineRow::Invalid(label) => invalid.push(label.clone()),
        }
    }
    serde_json::json!({ "deadlines": deadlines, "invalid": invalid })
}

fn render_deadlines(rows: &[DeadlineRow]) -> String {
    let mut lines = Vec::new();
    for row in rows {
        match row {
            DeadlineRow::Invalid(label) => {
                lines.push(format!("  {label:<30}  invalid date"));
            }
            DeadlineRow::Upcoming {
                label,
                date,
                end_date,
                days_left,
                notes,
            } => {
                let weeks = days_left / 7;
                let date_str = match end_date {
                    Some(end) => format!("{date} to {end}"),
                    None => date.clone(),
                };
                let suffix = if weeks > 0 {
                    format!(" ({weeks} weeks)")
                } else {
                    String::new()
                };
                let notes = if notes.is_empty() {
                    String::new()
                } else {
                    format!("  — {notes}")
                };
                lines.push(format!(
                    "  {label:<30}  {date_str:<25}  {days_left} days{suffix}{notes}"
                ));
            }
        }
    }
    if lines.is_empty() {
        "(no upcoming deadlines)".to_string()
    } else {
        lines.join("\n")
    }
}

/// `countdown-calc`: elapsed work days against a total, via a display
/// template; expired countdowns are dropped.
pub struct CountdownCalc;

#[async_trait]
impl StepHandler for CountdownCalc {
    fn type_name(&self) -> &'static str {
        "countdown-calc"
    }

    fn description(&self) -> &'static str {
        "Elapsed work days per countdown (config + store entries merged)"
    }

    fn data_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["countdowns", "invalid"],
            "properties": {
                "countdowns": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["label", "n", "total"],
                        "properties": {
                            "label": { "type": "string" },
                            "n": { "type": "integer", "description": "work days elapsed" },
                            "total": { "type": "integer" },
                            "display_line": { "type": "string" },
                            "role": { "type": "string" }
                        }
                    }
                },
                "invalid": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "labels with unparsable dates"
                }
            }
        })
    }

    fn required_programs(&self, _spec: &StepSpec) -> Vec<String> {
        vec![]
    }

    async fn run(&self, spec: &StepSpec, cx: &RunContext) -> StepResult {
        // Config entries + agent-registered store entries, merged. Store
        // failures degrade to config-only (soft).
        let mut countdowns = cx.config.countdowns.clone();
        if let Some(store) = &cx.store {
            if let Ok(stored) = store.countdowns_active() {
                countdowns.extend(stored.into_iter().map(|c| crate::config::CountdownEntry {
                    label: c.label,
                    start_date: c.start_date,
                    end_date: c.end_date,
                    total_work_days: c.total_work_days,
                    role: String::new(),
                    display: c.display,
                }));
            }
        }
        if countdowns.is_empty() {
            let mut result =
                StepResult::ok(&spec.name, &spec.step_type, "No countdowns configured.");
            result.data = Some(serde_json::json!({ "countdowns": [], "invalid": [] }));
            return result;
        }
        if cx.dry_run {
            let labels: Vec<&str> = countdowns.iter().map(|c| c.label.as_str()).collect();
            return StepResult::ok(
                &spec.name,
                &spec.step_type,
                format!(
                    "[dry-run] would calculate countdowns for: {}",
                    labels.join(", ")
                ),
            );
        }

        let computed = compute_countdowns(&countdowns, today());
        let mut result = StepResult::ok(&spec.name, &spec.step_type, render_countdowns(&computed));
        result.data = Some(countdowns_data(&computed));
        result
    }
}

/// One computed active countdown (or an invalid entry).
enum CountdownRow {
    Active {
        label: String,
        n: u32,
        total: u32,
        display_line: String,
        role: String,
    },
    Invalid(String),
}

/// Pure computation, factored so tests pin `today`. Expired entries dropped.
fn compute_countdowns(
    countdowns: &[crate::config::CountdownEntry],
    today: NaiveDate,
) -> Vec<CountdownRow> {
    let mut rows = Vec::new();
    for cd in countdowns {
        let (Some(start), Some(end)) = (parse_iso(&cd.start_date), parse_iso(&cd.end_date)) else {
            rows.push(CountdownRow::Invalid(cd.label.clone()));
            continue;
        };
        if today > end {
            continue; // expired
        }
        let n = work_days_between(start, today);
        let display_line = cd
            .display
            .replace("{label}", &cd.label)
            .replace("{n}", &n.to_string())
            .replace("{total}", &cd.total_work_days.to_string());
        rows.push(CountdownRow::Active {
            label: cd.label.clone(),
            n,
            total: cd.total_work_days,
            display_line,
            role: cd.role.clone(),
        });
    }
    rows
}

fn countdowns_data(rows: &[CountdownRow]) -> serde_json::Value {
    let mut countdowns = Vec::new();
    let mut invalid = Vec::new();
    for row in rows {
        match row {
            CountdownRow::Active {
                label,
                n,
                total,
                display_line,
                role,
            } => countdowns.push(serde_json::json!({
                "label": label, "n": n, "total": total,
                "display_line": display_line, "role": role,
            })),
            CountdownRow::Invalid(label) => invalid.push(label.clone()),
        }
    }
    serde_json::json!({ "countdowns": countdowns, "invalid": invalid })
}

fn render_countdowns(rows: &[CountdownRow]) -> String {
    let mut lines = Vec::new();
    for row in rows {
        match row {
            CountdownRow::Invalid(label) => lines.push(format!("- {label}: invalid dates")),
            CountdownRow::Active {
                display_line, role, ..
            } => {
                lines.push(display_line.clone());
                if !role.is_empty() {
                    lines.push(format!("  Role: {role}"));
                }
            }
        }
    }
    if lines.is_empty() {
        "(no active countdowns)".to_string()
    } else {
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CountdownEntry, DeadlineEntry};

    fn date(text: &str) -> NaiveDate {
        parse_iso(text).unwrap()
    }

    #[test]
    fn work_days_excludes_weekends_and_is_zero_for_inverted_ranges() {
        // 2026-06-01 is a Monday; one full week = 5 work days.
        assert_eq!(work_days_between(date("2026-06-01"), date("2026-06-08")), 5);
        // Saturday..Monday spans no weekdays except none (Sat, Sun).
        assert_eq!(work_days_between(date("2026-06-06"), date("2026-06-08")), 0);
        // end <= start → 0.
        assert_eq!(work_days_between(date("2026-06-08"), date("2026-06-01")), 0);
        assert_eq!(work_days_between(date("2026-06-01"), date("2026-06-01")), 0);
    }

    #[test]
    fn deadlines_render_days_weeks_notes_and_drop_past() {
        let deadlines = vec![
            DeadlineEntry {
                label: "soon".into(),
                date: "2026-06-10".into(),
                end_date: None,
                notes: "submit".into(),
            },
            DeadlineEntry {
                label: "far".into(),
                date: "2026-07-01".into(),
                end_date: Some("2026-07-03".into()),
                notes: String::new(),
            },
            DeadlineEntry {
                label: "past".into(),
                date: "2026-01-01".into(),
                end_date: None,
                notes: String::new(),
            },
            DeadlineEntry {
                label: "broken".into(),
                date: "not-a-date".into(),
                end_date: None,
                notes: String::new(),
            },
        ];
        let out = render_deadlines(&compute_deadlines(&deadlines, date("2026-06-05")));
        assert!(out.contains("soon"));
        assert!(out.contains("5 days"));
        assert!(out.contains("— submit"));
        assert!(out.contains("2026-07-01 to 2026-07-03"));
        assert!(out.contains("26 days (3 weeks)"));
        assert!(!out.contains("past"));
        assert!(out.contains("broken"));
        assert!(out.contains("invalid date"));
    }

    #[test]
    fn deadlines_all_past_renders_placeholder() {
        let deadlines = vec![DeadlineEntry {
            label: "gone".into(),
            date: "2020-01-01".into(),
            end_date: None,
            notes: String::new(),
        }];
        assert_eq!(
            render_deadlines(&compute_deadlines(&deadlines, date("2026-06-05"))),
            "(no upcoming deadlines)"
        );
    }

    #[test]
    fn countdowns_render_template_and_drop_expired() {
        let countdowns = vec![
            CountdownEntry {
                label: "Ramp".into(),
                start_date: "2026-06-01".into(), // Monday
                end_date: "2026-07-15".into(),
                total_work_days: 30,
                role: "pilot".into(),
                display: "{label}: work day {n} of {total}".into(),
            },
            CountdownEntry {
                label: "Done".into(),
                start_date: "2026-01-01".into(),
                end_date: "2026-02-01".into(),
                total_work_days: 30,
                role: String::new(),
                display: "{label}".into(),
            },
        ];
        // Friday 2026-06-05: Mon..Thu elapsed = 4 work days before today.
        let out = render_countdowns(&compute_countdowns(&countdowns, date("2026-06-05")));
        assert!(out.contains("Ramp: work day 4 of 30"));
        assert!(out.contains("Role: pilot"));
        assert!(!out.contains("Done"));
    }
}
