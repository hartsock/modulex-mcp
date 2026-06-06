//! modulex-plugin-health — host health steps: disk pressure, service
//! states, and accelerator detection with graceful degradation.
//!
//! **The reference plugin** (FOUNDATION pass F5): the worked example of the
//! plugin crate model — see `docs/PLUGIN_AUTHORING.md`, which was written
//! from this crate. Notable demonstrations:
//!
//! - **Steps, not tools**: three step types, ZERO MCP tools — host health
//!   is read in routines, so this plugin costs agents nothing in context.
//! - **Declared authority**: `required_programs` (grant + skip-probe) and
//!   `optional_programs` (grant only — the GPU fallback chain) feed the
//!   deny-by-default exec grant; nothing here can quietly widen the leash.
//! - **All checks are read-only.** Every spawn goes through the engine's
//!   `ExecGate`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use modulex_core::config::StepSpec;
use modulex_core::report::StepResult;
use modulex_core::step::{RunContext, StepHandler};
use modulex_core::{ExecGate, ExecRequest, StepRegistry};

/// Register this plugin's step types. The single entry point a host binary
/// calls; everything else is reachable through the engine.
pub fn register(registry: &mut StepRegistry) {
    registry.register(Arc::new(DiskCheck));
    registry.register(Arc::new(ServiceCheck));
    registry.register(Arc::new(GpuCheck));
}

async fn run_tool(
    exec: &ExecGate,
    program: &str,
    args: &[&str],
    timeout: u64,
) -> Result<modulex_core::ExecOutput, String> {
    exec.spawn(
        ExecRequest::new(program)
            .args(args.iter().map(ToString::to_string).collect())
            .timeout(Duration::from_secs(timeout)),
    )
    .await
    .map_err(|e| e.to_string())
}

// ── disk-check ─────────────────────────────────────────────────────────

/// `disk-check`: usage per configured mount with warn/critical thresholds.
pub struct DiskCheck;

/// Parsed `df -P` line for one mount (pure, tested).
fn parse_df(stdout: &str) -> Option<(u8, String)> {
    // POSIX df: header line, then one data line:
    //   Filesystem 1024-blocks Used Available Capacity Mounted on
    let line = stdout.lines().nth(1)?;
    let fields: Vec<&str> = line.split_whitespace().collect();
    let percent: u8 = fields.get(4)?.trim_end_matches('%').parse().ok()?;
    let available_kb: u64 = fields.get(3)?.parse().ok()?;
    let available = if available_kb >= 1024 * 1024 {
        format!("{:.1}G free", available_kb as f64 / (1024.0 * 1024.0))
    } else {
        format!("{}M free", available_kb / 1024)
    };
    Some((percent, available))
}

fn disk_state(percent: u8, warn: u8, critical: u8) -> &'static str {
    if percent >= critical {
        "critical"
    } else if percent >= warn {
        "warn"
    } else {
        "ok"
    }
}

#[async_trait]
impl StepHandler for DiskCheck {
    fn type_name(&self) -> &'static str {
        "disk-check"
    }

    fn description(&self) -> &'static str {
        "Disk usage per configured mount, with warn/critical thresholds"
    }

    fn data_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["mounts"],
            "properties": {
                "mounts": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["mount", "state"],
                        "properties": {
                            "mount": { "type": "string" },
                            "state": { "type": "string",
                                       "enum": ["ok", "warn", "critical", "error"] },
                            "used_percent": { "type": "integer" },
                            "detail": { "type": "string" }
                        }
                    }
                }
            }
        })
    }

    fn required_programs(&self, _spec: &StepSpec) -> Vec<String> {
        vec!["df".into()]
    }

    async fn run(&self, spec: &StepSpec, cx: &RunContext) -> StepResult {
        let mounts = {
            let configured = spec.param_str_list("mounts");
            if configured.is_empty() {
                vec!["/".to_string()]
            } else {
                configured
            }
        };
        let warn = u8::try_from(spec.param_int("warn_percent").unwrap_or(80)).unwrap_or(80);
        let critical = u8::try_from(spec.param_int("crit_percent").unwrap_or(90)).unwrap_or(90);

        if cx.dry_run {
            return StepResult::ok(
                &spec.name,
                &spec.step_type,
                format!("[dry-run] would check disk usage on: {}", mounts.join(", ")),
            );
        }

        let mut lines = Vec::new();
        let mut data_mounts = Vec::new();
        let mut all_ok = true;
        for mount in &mounts {
            match run_tool(&cx.exec, "df", &["-P", mount], spec.timeout).await {
                Ok(out) if out.success() => match parse_df(&out.stdout) {
                    Some((percent, available)) => {
                        let state = disk_state(percent, warn, critical);
                        let marker = match state {
                            "critical" => "CRITICAL ",
                            "warn" => "WARN ",
                            _ => "",
                        };
                        lines.push(format!("{marker}{mount}: {percent}% used, {available}"));
                        data_mounts.push(serde_json::json!({
                            "mount": mount, "state": state,
                            "used_percent": percent, "detail": available,
                        }));
                    }
                    None => {
                        all_ok = false;
                        lines.push(format!("{mount}: unparsable df output"));
                        data_mounts.push(serde_json::json!({
                            "mount": mount, "state": "error",
                            "detail": "unparsable df output",
                        }));
                    }
                },
                Ok(out) => {
                    all_ok = false;
                    let err = out.stderr.trim().to_string();
                    lines.push(format!("{mount}: ERROR {err}"));
                    data_mounts.push(serde_json::json!({
                        "mount": mount, "state": "error", "detail": err,
                    }));
                }
                Err(e) => {
                    all_ok = false;
                    lines.push(format!("{mount}: ERROR {e}"));
                    data_mounts.push(serde_json::json!({
                        "mount": mount, "state": "error", "detail": e,
                    }));
                }
            }
        }

        let mut result = StepResult::ok(&spec.name, &spec.step_type, lines.join("\n"));
        result.success = all_ok;
        result.data = Some(serde_json::json!({ "mounts": data_mounts }));
        result
    }
}

// ── service-check ──────────────────────────────────────────────────────

/// `service-check`: systemd unit states for a configured list.
pub struct ServiceCheck;

#[async_trait]
impl StepHandler for ServiceCheck {
    fn type_name(&self) -> &'static str {
        "service-check"
    }

    fn description(&self) -> &'static str {
        "systemd unit states for configured services (read-only is-active)"
    }

    fn data_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["services"],
            "properties": {
                "services": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["service", "state"],
                        "properties": {
                            "service": { "type": "string" },
                            "state": { "type": "string",
                                       "description": "systemd state token (active, inactive, failed, ...) or 'error'" },
                            "detail": { "type": "string" }
                        }
                    }
                }
            }
        })
    }

    fn required_programs(&self, _spec: &StepSpec) -> Vec<String> {
        vec!["systemctl".into()]
    }

    async fn run(&self, spec: &StepSpec, cx: &RunContext) -> StepResult {
        let services = spec.param_str_list("services");
        if services.is_empty() {
            let mut result = StepResult::ok(&spec.name, &spec.step_type, "No services configured.");
            result.data = Some(serde_json::json!({ "services": [] }));
            return result;
        }
        if cx.dry_run {
            return StepResult::ok(
                &spec.name,
                &spec.step_type,
                format!("[dry-run] would check services: {}", services.join(", ")),
            );
        }

        let mut lines = Vec::new();
        let mut data_services = Vec::new();
        let mut all_active = true;
        for service in &services {
            match run_tool(&cx.exec, "systemctl", &["is-active", service], spec.timeout).await {
                // `is-active` prints the state token and exits non-zero for
                // anything but active — the stdout token is the data either
                // way.
                Ok(out) => {
                    let state = out.stdout.trim().to_string();
                    let state = if state.is_empty() {
                        "unknown".to_string()
                    } else {
                        state
                    };
                    if state != "active" {
                        all_active = false;
                    }
                    lines.push(format!(
                        "{}{service}: {state}",
                        if state == "active" { "" } else { "ATTN " }
                    ));
                    data_services.push(serde_json::json!({
                        "service": service, "state": state,
                    }));
                }
                Err(e) => {
                    all_active = false;
                    lines.push(format!("ATTN {service}: ERROR {e}"));
                    data_services.push(serde_json::json!({
                        "service": service, "state": "error", "detail": e,
                    }));
                }
            }
        }

        let mut result = StepResult::ok(&spec.name, &spec.step_type, lines.join("\n"));
        result.success = all_active;
        result.data = Some(serde_json::json!({ "services": data_services }));
        result
    }
}

// ── gpu-check ──────────────────────────────────────────────────────────

/// `gpu-check`: accelerator detection with a 3-tier fallback chain —
/// rich tool → kernel interface → PCI scan. Reports which tier answered.
pub struct GpuCheck;

/// Parse `nvidia-smi --query-gpu=... --format=csv,noheader,nounits` lines
/// (pure, tested).
fn parse_nvidia_smi(stdout: &str) -> Vec<serde_json::Value> {
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let fields: Vec<&str> = line.split(',').map(str::trim).collect();
            serde_json::json!({
                "name": fields.first().copied().unwrap_or("?"),
                "utilization_percent": fields.get(1).and_then(|f| f.parse::<u32>().ok()),
                "memory_used_mib": fields.get(2).and_then(|f| f.parse::<u64>().ok()),
                "memory_total_mib": fields.get(3).and_then(|f| f.parse::<u64>().ok()),
                "temperature_c": fields.get(4).and_then(|f| f.parse::<u32>().ok()),
            })
        })
        .collect()
}

#[async_trait]
impl StepHandler for GpuCheck {
    fn type_name(&self) -> &'static str {
        "gpu-check"
    }

    fn description(&self) -> &'static str {
        "Accelerator detection: nvidia-smi → /proc driver → lspci fallback chain"
    }

    fn data_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["tier"],
            "properties": {
                "tier": { "type": "string",
                          "enum": ["nvidia-smi", "proc", "lspci", "none"],
                          "description": "which probe tier answered" },
                "gpus": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["name"],
                        "properties": {
                            "name": { "type": "string" },
                            "utilization_percent": { "type": ["integer", "null"] },
                            "memory_used_mib": { "type": ["integer", "null"] },
                            "memory_total_mib": { "type": ["integer", "null"] },
                            "temperature_c": { "type": ["integer", "null"] }
                        }
                    }
                },
                "detail": { "type": "string" }
            }
        })
    }

    fn required_programs(&self, _spec: &StepSpec) -> Vec<String> {
        vec![] // every probe is optional — that's the point of the chain
    }

    fn optional_programs(&self, _spec: &StepSpec) -> Vec<String> {
        // Fallback chain members: granted, never skip-probed.
        vec!["nvidia-smi".into(), "lspci".into()]
    }

    async fn run(&self, spec: &StepSpec, cx: &RunContext) -> StepResult {
        if cx.dry_run {
            return StepResult::ok(
                &spec.name,
                &spec.step_type,
                "[dry-run] would probe: nvidia-smi → /proc/driver/nvidia → lspci",
            );
        }

        // Tier 1: the rich tool.
        if cx.exec.program_available("nvidia-smi") {
            if let Ok(out) = run_tool(
                &cx.exec,
                "nvidia-smi",
                &[
                    "--query-gpu=name,utilization.gpu,memory.used,memory.total,temperature.gpu",
                    "--format=csv,noheader,nounits",
                ],
                spec.timeout,
            )
            .await
            {
                if out.success() {
                    let gpus = parse_nvidia_smi(&out.stdout);
                    let lines: Vec<String> = gpus
                        .iter()
                        .map(|g| {
                            format!(
                                "{}: {}% util, {}/{} MiB, {}°C",
                                g["name"].as_str().unwrap_or("?"),
                                g["utilization_percent"],
                                g["memory_used_mib"],
                                g["memory_total_mib"],
                                g["temperature_c"]
                            )
                        })
                        .collect();
                    let mut result = StepResult::ok(&spec.name, &spec.step_type, lines.join("\n"));
                    result.data = Some(serde_json::json!({ "tier": "nvidia-smi", "gpus": gpus }));
                    return result;
                }
            }
        }

        // Tier 2: the kernel interface (a file read — no exec at all).
        if let Ok(version) = std::fs::read_to_string("/proc/driver/nvidia/version") {
            let line = version.lines().next().unwrap_or("").trim().to_string();
            let mut result = StepResult::ok(
                &spec.name,
                &spec.step_type,
                format!("driver present (proc tier): {line}"),
            );
            result.data = Some(serde_json::json!({
                "tier": "proc", "gpus": [], "detail": line,
            }));
            return result;
        }

        // Tier 3: PCI scan.
        if cx.exec.program_available("lspci") {
            if let Ok(out) = run_tool(&cx.exec, "lspci", &[], spec.timeout).await {
                if out.success() {
                    let gpus: Vec<serde_json::Value> = out
                        .stdout
                        .lines()
                        .filter(|l| {
                            let lower = l.to_lowercase();
                            lower.contains("vga") || lower.contains("3d controller")
                        })
                        .map(|l| {
                            serde_json::json!({
                                "name": l.split(": ").nth(1).unwrap_or(l).trim(),
                            })
                        })
                        .collect();
                    let body = if gpus.is_empty() {
                        "no display/3D devices on PCI".to_string()
                    } else {
                        gpus.iter()
                            .map(|g| g["name"].as_str().unwrap_or("?").to_string())
                            .collect::<Vec<_>>()
                            .join("\n")
                    };
                    let mut result = StepResult::ok(&spec.name, &spec.step_type, body);
                    result.data = Some(serde_json::json!({ "tier": "lspci", "gpus": gpus }));
                    return result;
                }
            }
        }

        // No tier answered — informational, never a failure.
        let mut result = StepResult::ok(
            &spec.name,
            &spec.step_type,
            "no GPU probe available on this host",
        );
        result.data = Some(serde_json::json!({ "tier": "none", "gpus": [] }));
        result
    }
}

#[cfg(test)]
mod tests {
    use agent_bridle_core::{Caveats, Scope};
    use modulex_core::exec::test_support::{gate_with, MockSpawner};
    use modulex_core::Config;

    use super::*;

    fn cx_with(
        outputs: Vec<modulex_core::ExecOutput>,
        allow: &[&str],
    ) -> (RunContext, Arc<MockSpawner>) {
        let spawner = Arc::new(MockSpawner::with_outputs(outputs));
        let granted = Caveats {
            exec: Scope::only(allow.iter().map(ToString::to_string)),
            ..Caveats::top()
        };
        (
            RunContext {
                config: Arc::new(Config::default()),
                dry_run: false,
                generation: 1,
                exec: gate_with(&granted, spawner.clone()),
                prior: Vec::new(),
                store: None,
            },
            spawner,
        )
    }

    fn spec(toml_text: &str) -> StepSpec {
        toml::from_str(toml_text).unwrap()
    }

    const DF_OK: &str = "Filesystem 1024-blocks Used Available Capacity Mounted on\n\
                         /dev/sda1 100000000 50000000 45000000 53% /\n";

    #[test]
    fn df_parser_and_thresholds() {
        // FIXTURE-SYNC (#36): mimics `df -P /`; verified live by this
        // crate's live_disk_check_real_df test.
        let (percent, available) = parse_df(DF_OK).expect("parses");
        assert_eq!(percent, 53);
        assert!(available.contains("G free"));
        assert!(parse_df("garbage").is_none());

        assert_eq!(disk_state(53, 80, 90), "ok");
        assert_eq!(disk_state(85, 80, 90), "warn");
        assert_eq!(disk_state(95, 80, 90), "critical");
        assert_eq!(disk_state(80, 80, 90), "warn", "warn is inclusive");
    }

    #[tokio::test]
    async fn disk_check_states_and_data() {
        let (cx, _) = cx_with(
            vec![
                MockSpawner::ok(DF_OK),
                MockSpawner::ok(
                    "Filesystem 1024-blocks Used Available Capacity Mounted on\n\
                     /dev/sdb1 100 95 5 95% /data\n",
                ),
            ],
            &["df"],
        );
        let result = DiskCheck
            .run(
                &spec("name=\"d\"\ntype=\"disk-check\"\nmounts=[\"/\", \"/data\"]"),
                &cx,
            )
            .await;
        assert!(
            result.success,
            "warn/critical are report states, not failures... but error is"
        );
        let data = result.data.as_ref().unwrap();
        assert_eq!(data["mounts"][0]["state"], "ok");
        assert_eq!(data["mounts"][1]["state"], "critical");
        assert!(result.output.contains("CRITICAL /data: 95%"));
    }

    #[tokio::test]
    async fn service_check_flags_inactive() {
        // FIXTURE-SYNC (#36): mimics `systemctl is-active`; verified live by
        // live_service_check_real_systemctl.
        let (cx, spawner) = cx_with(
            vec![MockSpawner::ok("active\n"), MockSpawner::fail("", 3)],
            &["systemctl"],
        );
        // is-active prints the token on stdout even when failing:
        spawner.calls.lock().unwrap().clear();
        let (cx2, _) = cx_with(
            vec![
                MockSpawner::ok("active\n"),
                modulex_core::ExecOutput {
                    stdout: "inactive\n".into(),
                    status: Some(3),
                    ..Default::default()
                },
            ],
            &["systemctl"],
        );
        drop(cx);
        let result = ServiceCheck
            .run(
                &spec("name=\"s\"\ntype=\"service-check\"\nservices=[\"good\", \"bad\"]"),
                &cx2,
            )
            .await;
        assert!(!result.success, "an inactive service marks the step");
        let data = result.data.as_ref().unwrap();
        assert_eq!(data["services"][0]["state"], "active");
        assert_eq!(data["services"][1]["state"], "inactive");
        assert!(result.output.contains("ATTN bad: inactive"));
    }

    #[tokio::test]
    async fn gpu_check_tier1_parses_nvidia_smi() {
        // FIXTURE-SYNC (#36): mimics nvidia-smi CSV; verified live by
        // live_gpu_check_chain (on GPU hosts).
        let (cx, _) = cx_with(
            vec![MockSpawner::ok(
                "NVIDIA GeForce RTX 4090, 7, 1024, 24564, 45\n",
            )],
            &["nvidia-smi", "lspci"],
        );
        let result = GpuCheck
            .run(&spec("name=\"g\"\ntype=\"gpu-check\""), &cx)
            .await;
        assert!(result.success);
        let data = result.data.as_ref().unwrap();
        assert_eq!(data["tier"], "nvidia-smi");
        assert_eq!(data["gpus"][0]["name"], "NVIDIA GeForce RTX 4090");
        assert_eq!(data["gpus"][0]["memory_total_mib"], 24564);
    }

    #[tokio::test]
    async fn gpu_check_falls_back_when_smi_absent() {
        // MockSpawner with nvidia-smi missing → the chain drops through.
        let spawner = Arc::new(
            MockSpawner::with_outputs(vec![MockSpawner::ok(
                "00:02.0 VGA compatible controller: Fancy GPU Corp Device 1234\n",
            )])
            .missing(["nvidia-smi"]),
        );
        let granted = Caveats {
            exec: Scope::only(["nvidia-smi".to_string(), "lspci".to_string()]),
            ..Caveats::top()
        };
        let cx = RunContext {
            config: Arc::new(Config::default()),
            dry_run: false,
            generation: 1,
            exec: gate_with(&granted, spawner),
            prior: Vec::new(),
            store: None,
        };
        let result = GpuCheck
            .run(&spec("name=\"g\"\ntype=\"gpu-check\""), &cx)
            .await;
        let data = result.data.as_ref().unwrap();
        // Tier 2 (/proc) may answer on NVIDIA hosts; otherwise tier 3.
        let tier = data["tier"].as_str().unwrap();
        assert!(
            tier == "proc" || tier == "lspci",
            "expected fallback tier, got {tier}"
        );
    }

    #[test]
    fn optional_programs_feed_the_grant_not_the_probe() {
        // Regression guard for the F5 seam: gpu-check REQUIRES nothing
        // (never skip-probed) but declares its fallback tools as optional
        // (so the default grant covers them).
        let step = spec("name=\"g\"\ntype=\"gpu-check\"");
        assert!(GpuCheck.required_programs(&step).is_empty());
        assert_eq!(
            GpuCheck.optional_programs(&step),
            vec!["nvidia-smi".to_string(), "lspci".to_string()]
        );
    }
}
