//! Sourcing the granted leash — what every spawn in a run is confined to.
//!
//! Resolution order (first hit wins):
//!
//! 1. **`$MODULEX_CAVEATS`** — inline JSON in the agent-mesh `Caveats` serde
//!    shape. An orchestrator can mint a per-session leash this way.
//! 2. **`[caveats]`** table in the modulex config file (same shape in TOML).
//! 3. **Declared default** — `exec` restricted to exactly the programs the
//!    configured steps declare via
//!    [`crate::step::StepHandler::required_programs`]. This is the deliberate
//!    divergence from agent-bridle-mcp's unconfined `top()` default: modulex
//!    knows its work up front, so the safe default is *deny everything else*.

use std::collections::BTreeSet;

use agent_bridle_core::{Caveats, Scope};

/// Environment variable carrying an inline JSON grant.
pub const ENV_CAVEATS: &str = "MODULEX_CAVEATS";

/// Where the granted leash came from — surfaced in the startup banner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaveatsSource {
    /// `$MODULEX_CAVEATS` (JSON).
    Env,
    /// The `[caveats]` table of the loaded config file.
    ConfigFile,
    /// No explicit grant: exec confined to the declared program set.
    DeclaredDefault(BTreeSet<String>),
}

/// The resolved leash plus its provenance.
#[derive(Clone, Debug)]
pub struct GrantedCaveats {
    /// The granted authority every spawn is confined to.
    pub caveats: Caveats,
    /// Provenance, for the banner.
    pub source: CaveatsSource,
}

impl GrantedCaveats {
    /// Resolve from `$MODULEX_CAVEATS`, the config's `[caveats]`, or the
    /// declared default.
    ///
    /// # Errors
    /// Errors only when `$MODULEX_CAVEATS` is present but malformed — a
    /// missing source falls through.
    pub fn load(
        config_caveats: Option<&Caveats>,
        declared: BTreeSet<String>,
    ) -> anyhow::Result<Self> {
        let env = std::env::var(ENV_CAVEATS).ok();
        Self::resolve(env.as_deref(), config_caveats, declared)
    }

    /// Pure resolution, factored for tests.
    ///
    /// # Errors
    /// See [`GrantedCaveats::load`].
    pub fn resolve(
        env: Option<&str>,
        config_caveats: Option<&Caveats>,
        declared: BTreeSet<String>,
    ) -> anyhow::Result<Self> {
        if let Some(json) = env {
            let caveats: Caveats = serde_json::from_str(json).map_err(|e| {
                anyhow::anyhow!("${ENV_CAVEATS} is set but is not valid Caveats JSON: {e}")
            })?;
            return Ok(Self {
                caveats,
                source: CaveatsSource::Env,
            });
        }

        if let Some(caveats) = config_caveats {
            return Ok(Self {
                caveats: caveats.clone(),
                source: CaveatsSource::ConfigFile,
            });
        }

        Ok(Self {
            caveats: Caveats {
                exec: Scope::only(declared.iter().cloned()),
                ..Caveats::top()
            },
            source: CaveatsSource::DeclaredDefault(declared),
        })
    }

    /// One-line provenance banner for stderr.
    #[must_use]
    pub fn banner(&self) -> String {
        match &self.source {
            CaveatsSource::Env => format!("modulex: leash loaded from ${ENV_CAVEATS} (JSON)"),
            CaveatsSource::ConfigFile => "modulex: leash loaded from config [caveats]".to_string(),
            CaveatsSource::DeclaredDefault(programs) => {
                let list = programs.iter().cloned().collect::<Vec<_>>().join(", ");
                format!(
                    "modulex: leash = declared default (exec only: [{list}]); \
                     set ${ENV_CAVEATS} or [caveats] to override"
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use agent_bridle_core::CountBound;

    use super::*;

    fn declared(programs: &[&str]) -> BTreeSet<String> {
        programs.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn env_json_wins_and_parses_the_mesh_shape() {
        let json = r#"{
            "fs_read": "all", "fs_write": "all",
            "exec": { "only": ["echo"] }, "net": "all",
            "max_calls": { "at_most": 3 }, "valid_for_generation": "all"
        }"#;
        let g = GrantedCaveats::resolve(Some(json), None, declared(&["git"])).unwrap();
        assert_eq!(g.source, CaveatsSource::Env);
        assert_eq!(g.caveats.exec, Scope::only(["echo".to_string()]));
        assert_eq!(g.caveats.max_calls, CountBound::AtMost(3));
    }

    #[test]
    fn malformed_env_json_is_an_error() {
        let err = GrantedCaveats::resolve(Some("{ nope"), None, declared(&[])).unwrap_err();
        assert!(err.to_string().contains(ENV_CAVEATS));
    }

    #[test]
    fn config_caveats_are_second() {
        let from_config = Caveats {
            exec: Scope::only(["git".to_string()]),
            ..Caveats::top()
        };
        let g = GrantedCaveats::resolve(None, Some(&from_config), declared(&["gh"])).unwrap();
        assert_eq!(g.source, CaveatsSource::ConfigFile);
        assert_eq!(g.caveats.exec, Scope::only(["git".to_string()]));
    }

    #[test]
    fn default_is_declared_programs_not_unconfined() {
        let g = GrantedCaveats::resolve(None, None, declared(&["git", "gh"])).unwrap();
        assert!(matches!(g.source, CaveatsSource::DeclaredDefault(_)));
        assert_eq!(
            g.caveats.exec,
            Scope::only(["git".to_string(), "gh".to_string()])
        );
        assert!(g.banner().contains("declared default"));
        assert!(g.banner().contains("git"));
    }

    #[test]
    fn empty_declared_default_grants_nothing() {
        let g = GrantedCaveats::resolve(None, None, BTreeSet::new()).unwrap();
        assert_eq!(g.caveats.exec, Scope::none());
    }
}
