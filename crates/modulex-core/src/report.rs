//! Report model — the structured outcome of one routine run.
//!
//! A report's identity is its **generation**: a monotonic counter owned by
//! the engine (a causal coordinate, never wall-clock). Shapes mirror the
//! proven gila-plugin-morning models so existing dashboards translate 1:1.

use serde::Serialize;

/// Outcome for one repo inside a fan-out step.
#[derive(Clone, Debug, Serialize)]
pub struct RepoResult {
    /// Repo path or slug.
    pub repo: String,
    /// Per-repo body text.
    pub output: String,
    /// Whether this repo's sub-task succeeded.
    pub success: bool,
    /// Error detail when `success` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl RepoResult {
    /// A successful per-repo result.
    #[must_use]
    pub fn ok(repo: impl Into<String>, output: impl Into<String>) -> Self {
        Self {
            repo: repo.into(),
            output: output.into(),
            success: true,
            error: None,
        }
    }

    /// A failed per-repo result.
    #[must_use]
    pub fn err(repo: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            repo: repo.into(),
            output: String::new(),
            success: false,
            error: Some(error.into()),
        }
    }
}

/// Outcome of one step.
#[derive(Clone, Debug, Serialize)]
pub struct StepResult {
    /// Step display name (report section heading).
    pub step_name: String,
    /// Handler key.
    pub step_type: String,
    /// False when the step ran and failed. Skipped steps stay `true`-ish in
    /// routine accounting — see [`Report::add`].
    pub success: bool,
    /// True when the step was skipped (missing tool, auth unavailable).
    pub skipped: bool,
    /// Markdown body for the report section.
    pub output: String,
    /// Error detail when failed (or skip reason).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Per-repo breakdown for fan-out steps.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub repo_results: Vec<RepoResult>,
    /// Structured payload from `harness`/plugin steps.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl StepResult {
    /// A successful result with a text body.
    #[must_use]
    pub fn ok(name: &str, step_type: &str, output: impl Into<String>) -> Self {
        Self {
            step_name: name.to_string(),
            step_type: step_type.to_string(),
            success: true,
            skipped: false,
            output: output.into(),
            error: None,
            repo_results: Vec::new(),
            data: None,
        }
    }

    /// A failed result.
    #[must_use]
    pub fn fail(name: &str, step_type: &str, error: impl Into<String>) -> Self {
        Self {
            success: false,
            error: Some(error.into()),
            ..Self::ok(name, step_type, "")
        }
    }

    /// A skipped result (missing tool, soft auth failure).
    #[must_use]
    pub fn skip(name: &str, step_type: &str, reason: impl Into<String>) -> Self {
        Self {
            skipped: true,
            output: reason.into(),
            ..Self::ok(name, step_type, "")
        }
    }

    /// Attach per-repo results, deriving step `success` from them.
    #[must_use]
    pub fn with_repos(mut self, repos: Vec<RepoResult>) -> Self {
        self.success = repos.iter().all(|r| r.success);
        self.repo_results = repos;
        self
    }
}

/// The structured outcome of one routine run.
#[derive(Clone, Debug, Serialize)]
pub struct Report {
    /// Monotonic run identity (counter, never a clock).
    pub generation: u64,
    /// Routine name.
    pub routine: String,
    /// Whether this was a dry run.
    pub dry_run: bool,
    /// True iff every non-skipped step succeeded.
    pub success: bool,
    /// One-line accounting, e.g. `morning: 9 ran, 2 skipped, 1 failed`.
    pub summary: String,
    /// Per-step outcomes, in config order.
    pub step_results: Vec<StepResult>,
}

impl Report {
    /// An empty report for a run about to start.
    #[must_use]
    pub fn new(generation: u64, routine: &str, dry_run: bool) -> Self {
        Self {
            generation,
            routine: routine.to_string(),
            dry_run,
            success: true,
            summary: String::new(),
            step_results: Vec::new(),
        }
    }

    /// Append a step result; a non-skipped failure marks the run failed.
    pub fn add(&mut self, result: StepResult) {
        if !result.success && !result.skipped {
            self.success = false;
        }
        self.step_results.push(result);
    }

    /// Recompute the `summary` line from the accumulated results.
    pub fn finalize(&mut self) {
        let total = self.step_results.len();
        let skipped = self.step_results.iter().filter(|r| r.skipped).count();
        let failed = self
            .step_results
            .iter()
            .filter(|r| !r.success && !r.skipped)
            .count();
        let ran = total - skipped;
        let suffix = if self.dry_run { " [dry-run]" } else { "" };
        self.summary = format!(
            "{}: {ran} ran, {skipped} skipped, {failed} failed{suffix}",
            self.routine
        );
    }

    /// Render as human-readable markdown.
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut lines = vec![
            format!("# {} — gen {}", self.routine, self.generation),
            String::new(),
        ];
        for result in &self.step_results {
            lines.push(format!("## {}", result.step_name));
            if result.skipped {
                lines.push(format!(
                    "(skipped: {})",
                    if result.output.is_empty() {
                        result.error.as_deref().unwrap_or("no reason given")
                    } else {
                        &result.output
                    }
                ));
            } else if let Some(error) = &result.error {
                lines.push(format!("ERROR: {error}"));
            } else if result.output.is_empty() {
                lines.push("(no output)".to_string());
            } else {
                lines.push(result.output.clone());
            }
            lines.push(String::new());
        }
        lines.push("---".to_string());
        lines.push(self.summary.clone());
        lines.join("\n")
    }

    /// Render as compact JSON (MCP payloads stay small).
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_accounting_ignores_skips() {
        let mut report = Report::new(7, "morning", false);
        report.add(StepResult::ok("a", "script", "fine"));
        report.add(StepResult::skip("b", "script", "tool missing"));
        report.add(StepResult::fail("c", "script", "boom"));
        report.finalize();
        assert!(!report.success);
        assert_eq!(report.summary, "morning: 2 ran, 1 skipped, 1 failed");
    }

    #[test]
    fn all_skips_is_still_success() {
        let mut report = Report::new(1, "r", true);
        report.add(StepResult::skip("a", "script", "no tool"));
        report.finalize();
        assert!(report.success);
        assert_eq!(report.summary, "r: 0 ran, 1 skipped, 0 failed [dry-run]");
    }

    #[test]
    fn to_text_renders_sections_and_summary() {
        let mut report = Report::new(3, "demo", false);
        report.add(StepResult::ok("weather", "script", "sunny"));
        report.add(StepResult::fail("mail", "script", "imap down"));
        report.finalize();
        let text = report.to_text();
        assert!(text.starts_with("# demo — gen 3"));
        assert!(text.contains("## weather\nsunny"));
        assert!(text.contains("## mail\nERROR: imap down"));
        assert!(text.ends_with("demo: 2 ran, 0 skipped, 1 failed"));
    }

    #[test]
    fn to_json_is_compact_and_omits_empty_fields() {
        let mut report = Report::new(2, "r", false);
        report.add(StepResult::ok("a", "script", "x"));
        report.finalize();
        let json = report.to_json();
        assert!(!json.contains('\n'));
        assert!(!json.contains("repo_results"));
        assert!(!json.contains("\"error\""));
        assert!(json.contains("\"generation\":2"));
    }

    #[test]
    fn with_repos_derives_success() {
        let ok = StepResult::ok("s", "git-status", "").with_repos(vec![
            RepoResult::ok("a", "(clean)"),
            RepoResult::ok("b", "M x.rs"),
        ]);
        assert!(ok.success);
        let bad = StepResult::ok("s", "git-status", "")
            .with_repos(vec![RepoResult::ok("a", ""), RepoResult::err("b", "gone")]);
        assert!(!bad.success);
    }
}
