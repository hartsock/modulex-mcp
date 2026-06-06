//! Facet policy — which tool groups a connection sees (FOUNDATION pillar B).
//!
//! Resolution order (first hit wins), mirroring the leash's three-tier
//! sourcing discipline:
//!
//! 1. **`$MODULEX_TOOLS`** — comma-separated facet list, e.g.
//!    `core,store,store-classic`.
//! 2. **`[mcp] expose`** in the config file.
//! 3. **Default**: the budgeted index — `core` + `store`.
//!
//! `[mcp] deny` (config only) switches facets fully off: not listed, not
//! discoverable, not invokable. Exposure is about *context cost*, never
//! authorization — the leash governs effects.

use modulex_core::config::McpConfig;

/// Environment variable carrying an inline facet list.
pub const ENV_TOOLS: &str = "MODULEX_TOOLS";

/// Facets exposed when nothing is configured: the budgeted default index.
pub const DEFAULT_FACETS: &[&str] = &["core", "store"];

/// Where the exposure came from — surfaced in the startup banner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FacetSource {
    /// `$MODULEX_TOOLS`.
    Env,
    /// `[mcp] expose` in the config file.
    Config,
    /// The built-in default surface.
    Default,
}

/// The resolved exposure policy.
#[derive(Clone, Debug)]
pub struct FacetPolicy {
    exposed: Vec<String>,
    denied: Vec<String>,
    /// Provenance of `exposed`, for the banner.
    pub source: FacetSource,
}

impl FacetPolicy {
    /// Resolve from `$MODULEX_TOOLS` / config / default.
    #[must_use]
    pub fn load(mcp: &McpConfig) -> Self {
        let env = std::env::var(ENV_TOOLS).ok();
        Self::resolve(env.as_deref(), mcp)
    }

    /// Pure resolution, factored for tests.
    #[must_use]
    pub fn resolve(env: Option<&str>, mcp: &McpConfig) -> Self {
        let denied = mcp.deny.clone();
        let (exposed, source) = if let Some(env) = env {
            (
                env.split(',')
                    .map(str::trim)
                    .filter(|f| !f.is_empty())
                    .map(ToString::to_string)
                    .collect(),
                FacetSource::Env,
            )
        } else if !mcp.expose.is_empty() {
            (mcp.expose.clone(), FacetSource::Config)
        } else {
            (
                DEFAULT_FACETS.iter().map(ToString::to_string).collect(),
                FacetSource::Default,
            )
        };
        Self {
            exposed,
            denied,
            source,
        }
    }

    /// Is this facet's tool group listed in `tools/list`?
    #[must_use]
    pub fn exposes(&self, facet: &str) -> bool {
        !self.denies(facet) && self.exposed.iter().any(|f| f == facet)
    }

    /// Is this facet switched fully off (not listed, not discoverable, not
    /// invokable)?
    #[must_use]
    pub fn denies(&self, facet: &str) -> bool {
        self.denied.iter().any(|f| f == facet)
    }

    /// One-line provenance banner for stderr.
    #[must_use]
    pub fn banner(&self) -> String {
        let exposed = self.exposed.join(", ");
        let deny = if self.denied.is_empty() {
            String::new()
        } else {
            format!("; denied: [{}]", self.denied.join(", "))
        };
        match self.source {
            FacetSource::Env => {
                format!("modulex-mcp: tool facets from ${ENV_TOOLS}: [{exposed}]{deny}")
            }
            FacetSource::Config => {
                format!("modulex-mcp: tool facets from config [mcp]: [{exposed}]{deny}")
            }
            FacetSource::Default => {
                format!("modulex-mcp: tool facets = default index [{exposed}]{deny}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mcp(expose: &[&str], deny: &[&str]) -> McpConfig {
        McpConfig {
            expose: expose.iter().map(ToString::to_string).collect(),
            deny: deny.iter().map(ToString::to_string).collect(),
        }
    }

    #[test]
    fn env_wins_then_config_then_default() {
        let policy = FacetPolicy::resolve(Some("core, store-classic"), &mcp(&["store"], &[]));
        assert_eq!(policy.source, FacetSource::Env);
        assert!(policy.exposes("core"));
        assert!(policy.exposes("store-classic"));
        assert!(!policy.exposes("store"));

        let policy = FacetPolicy::resolve(None, &mcp(&["core"], &[]));
        assert_eq!(policy.source, FacetSource::Config);
        assert!(policy.exposes("core"));
        assert!(!policy.exposes("store"));

        let policy = FacetPolicy::resolve(None, &mcp(&[], &[]));
        assert_eq!(policy.source, FacetSource::Default);
        assert!(policy.exposes("core"));
        assert!(policy.exposes("store"));
        assert!(!policy.exposes("store-classic"), "classic is opt-in");
    }

    #[test]
    fn deny_beats_expose_everywhere() {
        let policy = FacetPolicy::resolve(Some("core,store"), &mcp(&[], &["store"]));
        assert!(policy.exposes("core"));
        assert!(!policy.exposes("store"), "denied facets never list");
        assert!(policy.denies("store"));
    }

    #[test]
    fn banner_names_provenance() {
        assert!(FacetPolicy::resolve(None, &mcp(&[], &[]))
            .banner()
            .contains("default index"));
        assert!(FacetPolicy::resolve(Some("core"), &mcp(&[], &[]))
            .banner()
            .contains(ENV_TOOLS));
        assert!(FacetPolicy::resolve(None, &mcp(&["core"], &["x"]))
            .banner()
            .contains("denied: [x]"));
    }
}
