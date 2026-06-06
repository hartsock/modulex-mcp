# modulex-plugin-health

Host health steps for the [modulex](https://github.com/hartsock/modulex-mcp)
routine engine: disk pressure, service states, and accelerator detection
with graceful degradation. **This is the reference plugin** — the worked
example behind `docs/PLUGIN_AUTHORING.md`.

## Step types

| Type | What it reports | Spawns |
|---|---|---|
| `disk-check` | usage per mount vs `warn_percent`/`crit_percent` thresholds | `df` |
| `service-check` | systemd unit states for a configured list | `systemctl` (read-only `is-active`) |
| `gpu-check` | accelerators via a 3-tier fallback: `nvidia-smi` → `/proc/driver/nvidia` → `lspci`; reports which tier answered | optional: `nvidia-smi`, `lspci` |

Zero MCP tools — health is read in routines (steps before tools).

## Usage

```toml
[[routines.morning.steps]]
name = "disk"
type = "disk-check"
mounts = ["/", "/home"]
warn_percent = 80
crit_percent = 90

[[routines.morning.steps]]
name = "services"
type = "service-check"
services = ["sshd", "k3s"]

[[routines.morning.steps]]
name = "gpu"
type = "gpu-check"
```

Every step emits a typed `data` payload (see the schemas in
`tests/golden/`); thresholds breached show as `warn`/`critical` states —
real probe errors mark the step failed, missing tools degrade or skip
softly.

Enabled by default in the `modulex` / `modulex-mcp` binaries (cargo feature
`plugin-health`); `--no-default-features` builds a lean engine without it.
