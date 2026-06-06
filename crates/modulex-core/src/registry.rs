//! Step registry: handler key → implementation.
//!
//! Builtins come from [`crate::steps::builtin_registry`]; embedders (the
//! Python bindings, downstream crates) register additional handlers at
//! startup. Registration is name-keyed and last-write-wins, so a plugin can
//! deliberately shadow a builtin.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::step::StepHandler;

/// The handler registry consulted by the engine for each step's `type`.
#[derive(Clone, Default)]
pub struct StepRegistry {
    handlers: BTreeMap<String, Arc<dyn StepHandler>>,
}

impl StepRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a handler under its [`StepHandler::type_name`].
    pub fn register(&mut self, handler: Arc<dyn StepHandler>) {
        self.handlers
            .insert(handler.type_name().to_string(), handler);
    }

    /// Look up a handler by step type.
    #[must_use]
    pub fn get(&self, step_type: &str) -> Option<Arc<dyn StepHandler>> {
        self.handlers.get(step_type).cloned()
    }

    /// All registered type names, sorted.
    #[must_use]
    pub fn type_names(&self) -> Vec<String> {
        self.handlers.keys().cloned().collect()
    }

    /// Full specs for every registered step type, sorted by name:
    /// `(type_name, description, data_schema)` — the machine-readable step
    /// surface (FOUNDATION pillar A).
    #[must_use]
    pub fn specs(&self) -> Vec<(String, String, serde_json::Value)> {
        self.handlers
            .values()
            .map(|h| {
                (
                    h.type_name().to_string(),
                    h.description().to_string(),
                    h.data_schema(),
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;

    use super::*;
    use crate::config::StepSpec;
    use crate::report::StepResult;
    use crate::step::RunContext;

    struct Fake(&'static str);
    #[async_trait]
    impl StepHandler for Fake {
        fn type_name(&self) -> &'static str {
            self.0
        }
        fn description(&self) -> &'static str {
            "fake"
        }
        fn data_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn required_programs(&self, _spec: &StepSpec) -> Vec<String> {
            vec![]
        }
        async fn run(&self, spec: &StepSpec, _cx: &RunContext) -> StepResult {
            StepResult::ok(&spec.name, self.0, "fake")
        }
    }

    #[test]
    fn register_lookup_and_shadowing() {
        let mut reg = StepRegistry::new();
        reg.register(Arc::new(Fake("a")));
        reg.register(Arc::new(Fake("b")));
        assert!(reg.get("a").is_some());
        assert!(reg.get("missing").is_none());
        assert_eq!(reg.type_names(), vec!["a".to_string(), "b".to_string()]);

        // Last write wins — plugins may shadow builtins.
        reg.register(Arc::new(Fake("a")));
        assert_eq!(reg.type_names().len(), 2);
    }
}
