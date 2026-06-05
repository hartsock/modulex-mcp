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
//!
//! Forge steps (`gh`/`glab`), board scans, and the plugin protocol arrive in
//! follow-up changes; the registry shape is final.

use std::sync::Arc;

use crate::registry::StepRegistry;

pub mod dates;
pub mod git;
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
        ] {
            assert!(names.iter().any(|n| n == expected), "missing {expected}");
        }
    }
}
