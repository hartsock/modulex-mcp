# CLAUDE.md

Guidance for Claude Code when working in this repository.

## What modulex is

A deterministic, pluggable routine engine for agents. Routines are
config-defined step sequences producing one structured report; the engine is
exposed as a CLI (`modulex`), a stdio MCP server (`modulex-mcp`), and Python
bindings (`modulex-py`). The flagship routine is a good-morning dashboard.

It is a telescope, not the sky: the point is *starting the day with context*,
identically for every agent and human that asks. Don't grow product features
into the engine — grow step types and plugins.

## Hard rules

1. **All subprocess spawns go through `ExecGate::spawn`**
   (`crates/modulex-core/src/exec.rs`). Adding a raw
   `std::process::Command::new` / `tokio::process::Command::new` anywhere
   else is a review-blocking violation — it bypasses the agent-bridle leash.
2. **`Secret` stays unserializable.** No `Serialize` impl, no `Debug` that
   prints the value, ever. A `compile_fail` doctest guards this.
3. **No credentials in config, code, tests, or examples.** Config carries
   references (`{env=..}`, `{file=..}`, `{cmd=..}`) only.
4. **Never use wall-clock time as a coordination primitive.** Report
   identity is the engine's generation counter. Dates are display math only.
5. **Soft failures.** A step that fails or can't run marks the report; it
   never aborts the routine. `isError`/process-exit failures are reserved
   for engine faults (bad config, unknown routine, leash denial of the run).
6. **Unit tests never spawn real processes.** Use
   `exec::test_support::MockSpawner`. Real-subprocess coverage lives in the
   dedicated gated integration test only.

## Build & validate

```bash
just check          # fmt --check + clippy -D warnings + tests (the gate)
just demo           # dry-run the example morning routine
just install-hooks  # REQUIRED after clone: installs .githooks/pre-push
```

Zero-warnings policy: `cargo clippy --all-targets --all-features -D warnings`
must be clean before any push.

## Hook / pipeline parity

`.githooks/pre-push` and `.github/workflows/ci.yml` must run the same steps.
Editing either REQUIRES auditing the other. Both carry cross-reference
comments — keep them.

## Workflow

- Branch → TDD → `just check` green → push → PR → human merges. Agents do
  not push to main and do not merge.
- Every bug fix includes a regression test that fails on the old code.
- One logical change per branch; branches live hours-to-days, not weeks.

## Versioning & release

- **0.1.x semver line** (Shawn, 2026-06-05). First publish = `0.1.0`, cut
  manually after multi-host testing. All crates lock-step in
  `[workspace.package]`; internal deps pin `=X.Y.Z` — bump them together.
- Release path: `.github/workflows/release.yml` (newt-agent lineage).
  Tag `v*` → binaries + GitHub release (draft) + crates.io
  (core → cli → mcp, 429-aware) + wheels → PyPI. `release/**` branches
  build binaries only. Needs `CARGO_REGISTRY_TOKEN` + `PYPI_API_TOKEN`
  secrets before the first tag.
- crates.io: `modulex-core`, `modulex-cli`, `modulex-mcp`. PyPI (wheels via
  maturin): `modulex-cli`, `modulex-mcp` (bin wheels), `modulex-py` (cdylib,
  publish=false on crates.io). Bare `modulex` is TAKEN on PyPI — never use
  it.

## Architecture map

| Crate | Role |
|---|---|
| `crates/modulex-core` | engine: config, credentials, caveats, exec gate, step trait/registry, report, builtin steps |
| `crates/modulex-cli` | one-shot human CLI (`modulex run/step/list/steps/doctor`) |
| `crates/modulex-mcp` | stdio MCP server (JSON-RPC 2.0, protocol 2024-11-05) — PR 2 |
| `crates/modulex-py` | PyO3: embed the engine, register Python step handlers, `serve_stdio()` — PR 4 |

Step semantics (per-repo fan-out, soft auth skips, `(clean)`/`(none)`
placeholders, report shapes) are deliberate and covered by tests — treat the
test suite as the behavioral reference when in doubt. Describe features on
their own terms; do not reference external/private projects in this repo's
docs, issues, or commit messages.

Caveats vocabulary (`Caveats`, `Scope`, `CountBound`) is canonical in
`agent-mesh-protocol`, consumed via `agent-bridle-core` re-exports. The
default leash is **deny-all-exec-except-declared** — a deliberate divergence
from agent-bridle-mcp's unconfined default. Keep it that way.
