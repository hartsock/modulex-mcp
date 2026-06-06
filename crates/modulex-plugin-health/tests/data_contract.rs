//! The plugin-local data-contract harness — same pattern as the core's
//! (golden-pinned schemas + conformance), scoped to this plugin's steps.
//! See docs/PLUGIN_AUTHORING.md: every plugin crate carries its own.

use std::path::PathBuf;
use std::sync::Arc;

use agent_bridle_core::{Caveats, Gate, Scope, Tool, ToolContext, ToolResult};
use modulex_core::exec::test_support::MockSpawner;
use modulex_core::{Config, ExecGate, RunContext, StepHandler, StepRegistry, StepSpec};

fn plugin_registry() -> StepRegistry {
    let mut registry = StepRegistry::new();
    modulex_plugin_health::register(&mut registry);
    registry
}

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

#[test]
fn golden_schemas_are_pinned() {
    let registry = plugin_registry();
    let update = std::env::var_os("UPDATE_GOLDEN_SCHEMAS").is_some();
    let mut failures = Vec::new();
    for (name, _description, schema) in registry.specs() {
        let path = golden_dir().join(format!("{name}.json"));
        let rendered = serde_json::to_string_pretty(&schema).expect("serializes") + "\n";
        if update {
            std::fs::write(&path, &rendered).expect("write golden");
            continue;
        }
        match std::fs::read_to_string(&path) {
            Ok(golden) if golden == rendered => {}
            Ok(_) => failures.push(format!(
                "{name}: schema CHANGED vs tests/golden/{name}.json — breaking \
                 change; if intentional rerun with UPDATE_GOLDEN_SCHEMAS=1"
            )),
            Err(_) => failures.push(format!(
                "{name}: missing golden — generate with UPDATE_GOLDEN_SCHEMAS=1"
            )),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

struct TestTool;
#[async_trait::async_trait]
impl Tool for TestTool {
    fn name(&self) -> &str {
        "t"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    async fn invoke(
        &self,
        _a: serde_json::Value,
        _c: &ToolContext,
    ) -> ToolResult<serde_json::Value> {
        Ok(serde_json::Value::Null)
    }
}

fn cx(outputs: Vec<modulex_core::ExecOutput>) -> RunContext {
    let spawner = Arc::new(MockSpawner::with_outputs(outputs));
    let granted = Caveats {
        exec: Scope::only(
            ["df", "systemctl", "nvidia-smi", "lspci"]
                .iter()
                .map(ToString::to_string),
        ),
        ..Caveats::top()
    };
    let tc = Gate::new(1)
        .authorize(&TestTool, &granted)
        .expect("authorize");
    RunContext {
        config: Arc::new(Config::default()),
        dry_run: false,
        generation: 1,
        exec: ExecGate::new(tc, spawner),
        prior: Vec::new(),
        store: None,
    }
}

fn spec(toml_text: &str) -> StepSpec {
    toml::from_str(toml_text).unwrap()
}

/// Every executed step's `data` validates against its published schema.
#[tokio::test(flavor = "multi_thread")]
async fn executed_step_data_validates_against_schema() {
    let registry = plugin_registry();
    let schemas: std::collections::BTreeMap<String, serde_json::Value> = registry
        .specs()
        .into_iter()
        .map(|(n, _, s)| (n, s))
        .collect();

    let cases: Vec<(StepSpec, Vec<modulex_core::ExecOutput>)> = vec![
        (
            spec("name=\"d\"\ntype=\"disk-check\""),
            vec![MockSpawner::ok(
                "Filesystem 1024-blocks Used Available Capacity Mounted on\n\
                 /dev/sda1 100000000 50000000 45000000 53% /\n",
            )],
        ),
        (
            spec("name=\"s\"\ntype=\"service-check\"\nservices=[\"sshd\"]"),
            vec![MockSpawner::ok("active\n")],
        ),
        (
            spec("name=\"g\"\ntype=\"gpu-check\""),
            vec![MockSpawner::ok("RTX, 1, 2, 3, 4\n")],
        ),
    ];

    let mut failures = Vec::new();
    for (step, outputs) in cases {
        let handler = registry.get(&step.step_type).expect("registered");
        let result = handler.run(&step, &cx(outputs)).await;
        let Some(data) = &result.data else {
            failures.push(format!("{}: no data emitted", step.step_type));
            continue;
        };
        let validator = jsonschema::validator_for(&schemas[&step.step_type]).expect("schema");
        for error in validator.iter_errors(data) {
            failures.push(format!(
                "{}: {} at {}",
                step.step_type, error, error.instance_path
            ));
        }
    }
    assert!(failures.is_empty(), "violations:\n{}", failures.join("\n"));
}

/// Tier-3 live checks (#36): real df/systemctl on this host, opt-in.
#[tokio::test(flavor = "multi_thread")]
async fn live_disk_and_service_checks() {
    if std::env::var_os("MODULEX_LIVE_TESTS").is_none() {
        eprintln!("live: skipped (set MODULEX_LIVE_TESTS=1)");
        return;
    }
    let spawner = Arc::new(modulex_core::TokioSpawner);
    let granted = Caveats {
        exec: Scope::only(["df".to_string(), "systemctl".to_string()]),
        ..Caveats::top()
    };
    let tc = Gate::new(1)
        .authorize(&TestTool, &granted)
        .expect("authorize");
    let cx = RunContext {
        config: Arc::new(Config::default()),
        dry_run: false,
        generation: 1,
        exec: ExecGate::new(tc, spawner),
        prior: Vec::new(),
        store: None,
    };

    if cx.exec.program_available("df") {
        let result = modulex_plugin_health::DiskCheck
            .run(&spec("name=\"d\"\ntype=\"disk-check\""), &cx)
            .await;
        assert!(result.success, "real df failed: {result:?}");
        let data = result.data.unwrap();
        assert!(data["mounts"][0]["used_percent"].is_u64());
        eprintln!("live df: {}", result.output);
    }
    if cx.exec.program_available("systemctl") {
        let result = modulex_plugin_health::ServiceCheck
            .run(
                &spec("name=\"s\"\ntype=\"service-check\"\nservices=[\"ssh\"]"),
                &cx,
            )
            .await;
        let data = result.data.unwrap();
        eprintln!("live systemctl ssh: {}", data["services"][0]["state"]);
        assert!(data["services"][0]["state"].is_string());
    }
}
