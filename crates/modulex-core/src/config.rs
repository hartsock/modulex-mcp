//! Configuration model and loader.
//!
//! TOML, searched in order: `$MODULEX_CONFIG` → `./modulex.toml` →
//! `~/.modulex/config.toml`. Routines are *data*: an ordered list of
//! [`StepSpec`]s. Volatile policy (repos, deadlines, recipients, hosts)
//! lives here — the engine ships mechanism only.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use agent_bridle_core::Caveats;
use serde::Deserialize;

use crate::credentials::CredentialRef;
use crate::registry::StepRegistry;

/// Environment variable naming an explicit config path (first in the search
/// order).
pub const ENV_CONFIG: &str = "MODULEX_CONFIG";

fn default_timeout() -> u64 {
    30
}

/// One step inside a routine. `type` selects the handler from the
/// [`StepRegistry`]; unrecognized keys flatten into [`StepSpec::params`] for
/// the handler to interpret.
#[derive(Clone, Debug, Deserialize)]
pub struct StepSpec {
    /// Display name, used as the report section heading.
    pub name: String,
    /// Handler key, e.g. `"git-status"` or `"script"`.
    #[serde(rename = "type")]
    pub step_type: String,
    /// Steps marked parallel that are *adjacent in config order* run as one
    /// concurrent batch. Results are re-ordered to config order, so reports
    /// stay deterministic.
    #[serde(default)]
    pub parallel: bool,
    /// Per-step repo override; empty means "use the shared repo list".
    #[serde(default)]
    pub repos: Vec<String>,
    /// Subprocess timeout in seconds.
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    /// Credential REFERENCES injected into the step's subprocess environment.
    /// Never literal values — see [`CredentialRef`].
    #[serde(default)]
    pub env: BTreeMap<String, CredentialRef>,
    /// Handler-specific parameters (e.g. `command`, `args`, `script`).
    #[serde(flatten)]
    pub params: toml::Table,
}

impl StepSpec {
    /// A string param from [`Self::params`], if present and a string.
    #[must_use]
    pub fn param_str(&self, key: &str) -> Option<&str> {
        self.params.get(key).and_then(|v| v.as_str())
    }

    /// A string-array param from [`Self::params`]; non-string entries are
    /// ignored.
    #[must_use]
    pub fn param_str_list(&self, key: &str) -> Vec<String> {
        self.params
            .get(key)
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// An integer param from [`Self::params`].
    #[must_use]
    pub fn param_int(&self, key: &str) -> Option<i64> {
        self.params.get(key).and_then(toml::Value::as_integer)
    }
}

/// A named, ordered sequence of steps.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct RoutineSpec {
    /// One-line description shown by `routine_list` / `modulex list`.
    #[serde(default)]
    pub description: String,
    /// The steps, in execution order.
    #[serde(default)]
    pub steps: Vec<StepSpec>,
}

/// Who the user is on the forges; consulted by review-queue steps.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct IdentityConfig {
    /// Forge username (reviewer filters).
    #[serde(default)]
    pub username: String,
    /// GitLab host, e.g. `gitlab.example.com`.
    #[serde(default)]
    pub gitlab_host: String,
}

/// A GitLab group to scan for MR activity.
#[derive(Clone, Debug, Deserialize)]
pub struct GitLabGroupConfig {
    /// Group path.
    pub name: String,
    /// `"recent"` (last 7 days) or `"all"` (all open).
    #[serde(default = "default_scan")]
    pub scan: String,
    /// Page size cap.
    #[serde(default = "default_per_page")]
    pub per_page: u32,
}

fn default_scan() -> String {
    "recent".into()
}
fn default_per_page() -> u32 {
    20
}

/// Shared data sources referenced by multiple steps.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct SharedConfig {
    /// Local repo paths for the git-* steps.
    #[serde(default)]
    pub repos: Vec<String>,
    /// `owner/repo` slugs for github-pr-scan.
    #[serde(default)]
    pub github_repos: Vec<String>,
    /// GitLab project paths for MR steps.
    #[serde(default)]
    pub gitlab_projects: Vec<String>,
    /// GitLab groups for group-wide MR scans.
    #[serde(default)]
    pub gitlab_groups: Vec<GitLabGroupConfig>,
}

/// Board/lane scan configuration.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct BoardConfig {
    /// Board root directory.
    #[serde(default)]
    pub path: String,
    /// Lane subdirectories to scan.
    #[serde(default)]
    pub lanes: Vec<String>,
}

/// Chores manifest location.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ChoresConfig {
    /// Chores board directory.
    #[serde(default)]
    pub path: String,
}

/// A fixed date to count down to.
#[derive(Clone, Debug, Deserialize)]
pub struct DeadlineEntry {
    /// Display label.
    pub label: String,
    /// ISO date `YYYY-MM-DD`.
    pub date: String,
    /// Optional range end, display-only.
    #[serde(default)]
    pub end_date: Option<String>,
    /// Free-form note appended to the line.
    #[serde(default)]
    pub notes: String,
}

/// A progress countdown measured in elapsed work days.
#[derive(Clone, Debug, Deserialize)]
pub struct CountdownEntry {
    /// Display label.
    pub label: String,
    /// ISO start date (inclusive).
    #[serde(default)]
    pub start_date: String,
    /// ISO end date; the countdown expires after this.
    #[serde(default)]
    pub end_date: String,
    /// Denominator for the display template.
    #[serde(default = "default_total_work_days")]
    pub total_work_days: u32,
    /// Optional role line.
    #[serde(default)]
    pub role: String,
    /// Display template with `{label}`, `{n}`, `{total}` placeholders.
    #[serde(default = "default_countdown_display")]
    pub display: String,
}

fn default_total_work_days() -> u32 {
    30
}
fn default_countdown_display() -> String {
    "{label}: work day {n} of {total}".into()
}

/// Top-level configuration.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Config {
    /// Forge identity.
    #[serde(default)]
    pub identity: IdentityConfig,
    /// Shared data sources.
    #[serde(default)]
    pub shared: SharedConfig,
    /// Board scan config.
    #[serde(default)]
    pub board: BoardConfig,
    /// Chores config.
    #[serde(default)]
    pub chores: ChoresConfig,
    /// Deadlines for `deadline-calc`.
    #[serde(default)]
    pub deadlines: Vec<DeadlineEntry>,
    /// Countdowns for `countdown-calc`.
    #[serde(default)]
    pub countdowns: Vec<CountdownEntry>,
    /// Optional explicit leash grant (tier 2 of the caveats search order).
    #[serde(default)]
    pub caveats: Option<Caveats>,
    /// The routines, by name.
    #[serde(default)]
    pub routines: BTreeMap<String, RoutineSpec>,
}

impl Config {
    /// Parse a TOML string.
    ///
    /// # Errors
    /// Returns an error when the TOML is malformed or does not match the
    /// config shape.
    pub fn from_toml(text: &str) -> anyhow::Result<Self> {
        Ok(toml::from_str(text)?)
    }

    /// Read and parse a config file.
    ///
    /// # Errors
    /// Returns an error when the file is unreadable or unparsable.
    pub fn from_path(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?;
        Self::from_toml(&text).map_err(|e| anyhow::anyhow!("cannot parse {}: {e}", path.display()))
    }

    /// Locate and load the config: `$MODULEX_CONFIG` → `./modulex.toml` →
    /// `~/.modulex/config.toml`. Returns the config and the path it came from.
    ///
    /// # Errors
    /// Returns an error when no config file is found in the search order, or
    /// when the located file fails to parse (a present-but-broken config is an
    /// error, never a silent fall-through).
    pub fn load() -> anyhow::Result<(Self, PathBuf)> {
        let env = std::env::var_os(ENV_CONFIG).map(PathBuf::from);
        let home = std::env::var_os("HOME").map(PathBuf::from);
        Self::resolve(env.as_deref(), home.as_deref())
    }

    /// Pure search-order resolution, factored for tests.
    ///
    /// # Errors
    /// See [`Config::load`].
    pub fn resolve(env: Option<&Path>, home: Option<&Path>) -> anyhow::Result<(Self, PathBuf)> {
        if let Some(path) = env {
            return Ok((Self::from_path(path)?, path.to_path_buf()));
        }
        let cwd = PathBuf::from("modulex.toml");
        if cwd.is_file() {
            return Ok((Self::from_path(&cwd)?, cwd));
        }
        if let Some(home) = home {
            let path = home.join(".modulex").join("config.toml");
            if path.is_file() {
                return Ok((Self::from_path(&path)?, path));
            }
        }
        anyhow::bail!(
            "no modulex config found: set ${ENV_CONFIG}, or create ./modulex.toml \
             or ~/.modulex/config.toml"
        )
    }

    /// The union of programs every configured step declares it will spawn —
    /// the *declared default* exec grant when no explicit caveats are given.
    /// Includes the argv0 of every `{cmd = ..}` credential reference: those
    /// commands run through the same leash, so they need the same grant.
    #[must_use]
    pub fn declared_programs(&self, registry: &StepRegistry) -> BTreeSet<String> {
        let mut programs = BTreeSet::new();
        for routine in self.routines.values() {
            for spec in &routine.steps {
                if let Some(handler) = registry.get(&spec.step_type) {
                    programs.extend(handler.required_programs(spec));
                }
                programs.extend(
                    spec.env
                        .values()
                        .filter_map(CredentialRef::declared_program),
                );
            }
        }
        programs
    }
}

/// Expand a leading `~/` against `$HOME`. Anything else passes through.
#[must_use]
pub fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_routine_parses_with_flattened_params() {
        let cfg = Config::from_toml(
            r#"
[routines.morning]
description = "demo"

[[routines.morning.steps]]
name = "weather"
type = "script"
command = "~/bin/weather.sh"
args = ["--brief"]
timeout = 10
"#,
        )
        .unwrap();
        let routine = &cfg.routines["morning"];
        assert_eq!(routine.description, "demo");
        let step = &routine.steps[0];
        assert_eq!(step.step_type, "script");
        assert_eq!(step.timeout, 10);
        assert_eq!(step.param_str("command"), Some("~/bin/weather.sh"));
        assert_eq!(step.param_str_list("args"), vec!["--brief".to_string()]);
        assert!(!step.parallel);
    }

    #[test]
    fn credential_refs_parse_all_three_shapes() {
        let cfg = Config::from_toml(
            r#"
[[routines.r.steps]]
name = "s"
type = "script"
command = "x"
env = { A = { env = "TOKEN_VAR" }, B = { file = "~/.keys/b" }, C = { cmd = "pass show c" } }
"#,
        )
        .unwrap();
        let env = &cfg.routines["r"].steps[0].env;
        assert!(matches!(&env["A"], CredentialRef::Env { env } if env == "TOKEN_VAR"));
        assert!(matches!(&env["B"], CredentialRef::File { file } if file == "~/.keys/b"));
        assert!(matches!(&env["C"], CredentialRef::Cmd { cmd } if cmd == "pass show c"));
    }

    #[test]
    fn caveats_table_parses_the_mesh_shape() {
        let cfg = Config::from_toml(
            r#"
[caveats]
fs_read = "all"
fs_write = "all"
exec = { only = ["git", "gh"] }
net = "all"
max_calls = "unlimited"
valid_for_generation = "all"
"#,
        )
        .unwrap();
        let caveats = cfg.caveats.expect("caveats present");
        assert_eq!(
            caveats.exec,
            agent_bridle_core::Scope::only(["git".to_string(), "gh".to_string()])
        );
    }

    #[test]
    fn shared_and_dates_sections_parse() {
        let cfg = Config::from_toml(
            r#"
[identity]
username = "someone"

[shared]
repos = ["~/src/a", "~/src/b"]
github_repos = ["owner/repo"]

[[shared.gitlab_groups]]
name = "mygroup"

[[deadlines]]
label = "CFP"
date = "2026-07-01"

[[countdowns]]
label = "Ramp"
start_date = "2026-06-01"
end_date = "2026-07-15"
"#,
        )
        .unwrap();
        assert_eq!(cfg.shared.repos.len(), 2);
        assert_eq!(cfg.shared.gitlab_groups[0].scan, "recent");
        assert_eq!(cfg.shared.gitlab_groups[0].per_page, 20);
        assert_eq!(cfg.deadlines[0].label, "CFP");
        assert_eq!(cfg.countdowns[0].total_work_days, 30);
    }

    #[test]
    fn declared_programs_include_cmd_credentials() {
        // Regression (fresh-eyes 2026-06-05): the declared default grant
        // missed {cmd=..} credential programs, denying them at run time.
        let cfg = Config::from_toml(
            r#"
[[routines.r.steps]]
name = "s"
type = "script"
command = "tool"
env = { TOKEN = { cmd = "pass show t" }, OTHER = { env = "X" } }
"#,
        )
        .unwrap();
        let declared = cfg.declared_programs(&crate::steps::builtin_registry());
        assert!(declared.contains("tool"));
        assert!(declared.contains("pass"));
        assert_eq!(declared.len(), 2);
    }

    #[test]
    fn resolve_errors_when_nothing_found() {
        let err = Config::resolve(None, Some(Path::new("/nonexistent-home"))).unwrap_err();
        assert!(err.to_string().contains("no modulex config"));
    }

    #[test]
    fn expand_tilde_expands_home_prefix_only() {
        std::env::var_os("HOME").expect("HOME set in tests");
        assert!(expand_tilde("~/x").is_absolute());
        assert_eq!(expand_tilde("/abs/x"), PathBuf::from("/abs/x"));
        assert_eq!(expand_tilde("rel/x"), PathBuf::from("rel/x"));
    }
}
