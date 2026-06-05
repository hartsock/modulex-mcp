//! Builtin step handlers.
//!
//! | type | crate module | spawns |
//! |---|---|---|
//! | `git-tend` | [`git`] | git |
//! | `git-status` | [`git`] | git |
//! | `git-unpushed` | [`git`] | git |
//! | `deadline-calc` | [`dates`] | — |
//! | `countdown-calc` | [`dates`] | — |
//! | `script` | [`script`] | the configured command |
//! | `harness` | [`script`] | the configured command (JSON-on-stdout) |
//! | `github-pr-scan` | [`github`] | gh |
//! | `gitlab-mr-authored` | [`gitlab`] | glab |
//! | `gitlab-mr-review` | [`gitlab`] | glab |
//! | `gitlab-group-mrs` | [`gitlab`] | glab |
//! | `mr-sla-check` | [`gitlab`] | — (derived from prior results) |
//! | `board-scan` | [`board`] | — |
//! | `chores-check` | [`board`] | — |
//!
//! The plugin protocol (`type = "python"`) arrives with the Python surface;
//! the registry shape is final.

use std::sync::Arc;

use crate::registry::StepRegistry;

pub mod board;
pub mod dates;
pub mod git;
pub mod github;
pub mod gitlab;
pub mod script;

/// A registry holding every builtin handler.
#[must_use]
pub fn builtin_registry() -> StepRegistry {
    let mut registry = StepRegistry::new();
    registry.register(Arc::new(git::GitTend));
    registry.register(Arc::new(git::GitStatus));
    registry.register(Arc::new(git::GitUnpushed));
    registry.register(Arc::new(dates::DeadlineCalc));
    registry.register(Arc::new(dates::CountdownCalc));
    registry.register(Arc::new(script::Script));
    registry.register(Arc::new(script::Harness));
    registry.register(Arc::new(github::GithubPrScan));
    registry.register(Arc::new(gitlab::GitlabMrAuthored));
    registry.register(Arc::new(gitlab::GitlabMrReview));
    registry.register(Arc::new(gitlab::GitlabGroupMrs));
    registry.register(Arc::new(gitlab::MrSlaCheck));
    registry.register(Arc::new(board::BoardScan));
    registry.register(Arc::new(board::ChoresCheck));
    registry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_register_the_documented_types() {
        let names = builtin_registry().type_names();
        for expected in [
            "git-tend",
            "git-status",
            "git-unpushed",
            "deadline-calc",
            "countdown-calc",
            "script",
            "harness",
            "github-pr-scan",
            "gitlab-mr-authored",
            "gitlab-mr-review",
            "gitlab-group-mrs",
            "mr-sla-check",
            "board-scan",
            "chores-check",
        ] {
            assert!(names.iter().any(|n| n == expected), "missing {expected}");
        }
    }
}
