//! The data-contract enforcement harness (FOUNDATION pillar A, #26).
//!
//! Two guarantees:
//!
//! 1. **Golden schemas** — every registered step type's `data_schema()` is
//!    pinned to a checked-in golden file. Changing a shape makes this test
//!    fail until the golden is updated — so every schema change is a visible,
//!    reviewed diff (and a breaking release per the standing law).
//! 2. **Outputs conform** — each builtin, driven through a mock spawner /
//!    in-memory store, produces `data` that VALIDATES against its schema.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use modulex_core::exec::test_support::MockSpawner;
use modulex_core::{steps::builtin_registry, Caveats, Config, Engine, GrantedCaveats, Store};

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

/// Guarantee 1: schemas match their checked-in goldens.
///
/// To update after an INTENTIONAL schema change:
///   UPDATE_GOLDEN_SCHEMAS=1 cargo test -p modulex-core --all-features golden
/// then review the diff like the breaking change it is.
#[test]
fn golden_schemas_are_pinned() {
    let registry = builtin_registry();
    let update = std::env::var_os("UPDATE_GOLDEN_SCHEMAS").is_some();
    let mut failures = Vec::new();

    for (name, _description, schema) in registry.specs() {
        let path = golden_dir().join(format!("{name}.json"));
        let rendered = serde_json::to_string_pretty(&schema).expect("schema serializes") + "\n";
        if update {
            std::fs::write(&path, &rendered).expect("write golden");
            continue;
        }
        match std::fs::read_to_string(&path) {
            Ok(golden) if golden == rendered => {}
            Ok(_) => failures.push(format!(
                "{name}: schema CHANGED vs tests/golden/{name}.json — a data \
                 contract is a versioned contract; if intentional, rerun with \
                 UPDATE_GOLDEN_SCHEMAS=1 and treat it as a breaking change"
            )),
            Err(_) => failures.push(format!(
                "{name}: missing golden tests/golden/{name}.json — generate it \
                 with UPDATE_GOLDEN_SCHEMAS=1"
            )),
        }
    }

    // The other direction: no orphaned goldens for unregistered steps.
    for entry in std::fs::read_dir(golden_dir()).expect("golden dir") {
        let path = entry.expect("entry").path();
        if path.extension().is_some_and(|e| e == "json") {
            let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
            if !registry.type_names().contains(&stem) {
                failures.push(format!("orphaned golden for unregistered step {stem:?}"));
            }
        }
    }

    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

const CONTRACT_CONFIG: &str = r#"
[identity]
username = "someone"
gitlab_host = "gitlab.example.com"

[shared]
repos = ["/r/a"]
github_repos = ["owner/repo"]
gitlab_projects = ["group/a"]

[[shared.gitlab_groups]]
name = "group"
scan = "all"

[board]
path = "/nonexistent-board"
lanes = ["p0"]

[chores]
path = ""

[[deadlines]]
label = "CFP"
date = "2999-12-31"

[[countdowns]]
label = "ramp"
start_date = "2026-06-01"
end_date = "2999-12-31"

[routines.contract]

[[routines.contract.steps]]
name = "tend"
type = "git-tend"

[[routines.contract.steps]]
name = "status"
type = "git-status"

[[routines.contract.steps]]
name = "unpushed"
type = "git-unpushed"

[[routines.contract.steps]]
name = "deadlines"
type = "deadline-calc"

[[routines.contract.steps]]
name = "countdowns"
type = "countdown-calc"

[[routines.contract.steps]]
name = "sh"
type = "script"
command = "sh"

[[routines.contract.steps]]
name = "tool"
type = "harness"
command = "sh"

[[routines.contract.steps]]
name = "prs"
type = "github-pr-scan"

[[routines.contract.steps]]
name = "authored"
type = "gitlab-mr-authored"

[[routines.contract.steps]]
name = "review"
type = "gitlab-mr-review"

[[routines.contract.steps]]
name = "groups"
type = "gitlab-group-mrs"

[[routines.contract.steps]]
name = "sla"
type = "mr-sla-check"

[[routines.contract.steps]]
name = "board"
type = "board-scan"

[[routines.contract.steps]]
name = "chores"
type = "chores-check"

[[routines.contract.steps]]
name = "agenda"
type = "reminders"
"#;

/// Guarantee 2: executed builtins emit `data` that validates against their
/// published schema. (Steps whose tools are scripted via the mock spawner;
/// url-watch and the python plugin are covered by their module tests — the
/// former needs the web feature's fetcher seam, the latter a script file.)
#[tokio::test(flavor = "multi_thread")]
async fn executed_step_data_validates_against_schema() {
    let config = Config::from_toml(CONTRACT_CONFIG).unwrap();
    let registry = builtin_registry();
    let declared = config.declared_programs(&registry);
    let granted: Caveats = GrantedCaveats::resolve(None, None, declared)
        .unwrap()
        .caveats;

    // Scripted outputs, in step order (git fan-outs consume one per repo).
    // FIXTURE-SYNC (#36): these strings mimic real tools; their shapes are
    // verified by tests/live_contract.rs (git states, gh --json fields,
    // harness stdout contract). Change a fixture → re-check its live test.
    let outputs = vec![
        MockSpawner::ok("Already up to date.\n"),     // tend: fetch
        MockSpawner::ok("Already up to date.\n"),     // tend: pull
        MockSpawner::ok(" M src/lib.rs\n"),           // status
        MockSpawner::ok("abc123 wip\n"),              // unpushed
        MockSpawner::ok("script out\n"),              // script
        MockSpawner::ok(r#"{"summary":"ok","n":1}"#), // harness
        MockSpawner::ok(r#"[{"number":7,"title":"t","author":{"login":"a"}}]"#), // gh
        MockSpawner::ok("!1 mr\n"),                   // glab authored
        MockSpawner::ok(""),                          // glab review (none)
        MockSpawner::ok("!2 mr\n"),                   // glab groups
    ];
    let spawner = Arc::new(MockSpawner::with_outputs(outputs));
    let store = Arc::new(Store::in_memory().unwrap());
    store
        .reminder_add("validate me", Some("2999-01-01"), None, 0)
        .unwrap();
    let engine = Engine::with_spawner(config, registry, granted, spawner).with_store(store);

    let report = engine
        .run_routine("contract", modulex_core::RunOptions::default())
        .await
        .unwrap();

    let schemas: BTreeMap<String, serde_json::Value> = builtin_registry()
        .specs()
        .into_iter()
        .map(|(name, _, schema)| (name, schema))
        .collect();

    let mut failures = Vec::new();
    for step in &report.step_results {
        if step.skipped {
            failures.push(format!(
                "{} ({}): unexpectedly skipped: {}",
                step.step_name, step.step_type, step.output
            ));
            continue;
        }
        let Some(data) = &step.data else {
            failures.push(format!(
                "{} ({}): executed step emitted NO data — the contract requires it",
                step.step_name, step.step_type
            ));
            continue;
        };
        let schema = &schemas[&step.step_type];
        let validator = jsonschema::validator_for(schema).expect("valid schema");
        for error in validator.iter_errors(data) {
            failures.push(format!(
                "{} ({}): data violates schema at {}: {}",
                step.step_name, step.step_type, error.instance_path, error
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "data contract violations:\n{}",
        failures.join("\n")
    );
}
