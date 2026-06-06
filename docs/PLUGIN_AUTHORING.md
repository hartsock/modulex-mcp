# Plugin Authoring

How to add capability to modulex without adding cost. This guide was
written from building the reference plugin
([`modulex-plugin-health`](../crates/modulex-plugin-health)) — read its
source alongside this document; every rule below is demonstrated there.

The non-negotiables come from [FOUNDATION.md](FOUNDATION.md): capability
lands as **step types** (zero MCP surface), every step carries a
**versioned data schema**, dependencies are **feature-gated**, and
authority is **declared**. The plugin backlog (#11–#31) follows this
contract.

## 1. The crate

```
crates/modulex-plugin-<name>/
├── Cargo.toml          # workspace versioning; deps: modulex-core (+ leaf deps)
├── README.md           # REQUIRED — step table, usage TOML, what it spawns
├── src/lib.rs          # register() + handlers; #![forbid(unsafe_code)]
└── tests/
    ├── golden/         # pinned data schemas, one JSON per step type
    └── data_contract.rs# golden harness + conformance + live tier
```

One public entry point:

```rust
pub fn register(registry: &mut StepRegistry) {
    registry.register(Arc::new(DiskCheck));
    // ...
}
```

## 2. Steps before tools

Read paths are step types — they cost agents nothing (the routine is the
compression). Only **mutations on the user's behalf** earn an MCP tool,
and tools register facet-scoped with a declared `mutates` flag, never on
the default surface (the budget is CI-pinned at `DEFAULT_TOOL_BUDGET`).
The reference plugin ships three step types and **zero tools** — that is
the normal shape.

State a **disclosure tier** in your PR description: step / store kind /
facet tool / discovered tool. "Default surface" requires changing the
pinned budget table — an exceptional answer needing an exceptional reason.

## 3. The step contract

Implement `StepHandler` fully (the compiler holds you to it):

- `type_name()` — kebab-case, globally unique.
- `description()` — one line; shown by `steps_list`.
- `data_schema()` — JSON Schema for your `data` payload. **Schemas are
  versioned contracts**: pin them in `tests/golden/` with the harness
  (copy `data_contract.rs` from the reference plugin); a shape change is a
  breaking release and shows up as a golden diff in review.
- `required_programs(spec)` — programs you WILL spawn. Feeds the
  deny-by-default exec grant AND the soft-skip probe (all must be present
  or the step skips).
- `optional_programs(spec)` — programs you MAY spawn (fallback chains).
  Grant only, never skip-probed; your handler degrades gracefully when
  one is absent (see `gpu-check`'s 3-tier chain).
- `run(spec, cx)` — **never return Err.** Failure semantics:
  - missing tool → the engine soft-skips for you (via required_programs)
  - per-item problems → mark the item, keep going (state enums in `data`)
  - real step failure → `success: false` with an `error` the user can act on
  - dry run → describe, spawn nothing
  - executed, non-skipped results MUST populate `data` per your schema

Configuration comes from step params (`spec.param_str` /
`param_str_list` / `param_int`) — plugins do not extend the core config.
Sensible defaults for everything (`disk-check` defaults to `/`, 80/90).

## 4. Authority is declared, execution is leashed

Every spawn goes through `cx.exec.spawn(ExecRequest::new(program)...)` —
the agent-bridle leash checks the program against the grant BEFORE any
process exists. Your declarations are what put your programs in the
default grant; nothing else does. Probe availability with
`cx.exec.program_available()` (the spawner seam — host-independent under
test). Network, when you need it, goes through the net-leashed fetcher
(`steps/web.rs` is the precedent), never a raw HTTP client.

Keep steps **read-only unless the step's whole point is the mutation** —
and say so in the README.

## 5. The three test tiers

1. **Mocked logic** (unit + conformance): `MockSpawner` (the
   `test-support` feature of modulex-core) with canned outputs; pure
   parser functions tested directly; the conformance test drives every
   step and validates `data` against the schema (`jsonschema`,
   dev-dependency only). No real processes, no host dependence — the
   skip-probe answers from the mock (`.missing([..])` to script absence).
2. **Golden schemas**: the pinned-schema test; regenerate with
   `UPDATE_GOLDEN_SCHEMAS=1` and treat the diff as the breaking change it
   is.
3. **Live contract** (#36): `MODULEX_LIVE_TESTS=1`-gated tests running
   the real tools with harmless invocations, asserting only the output
   shape your parsers key on. Mock fixtures that mimic a real tool carry
   a `FIXTURE-SYNC` comment naming their live test.

## 6. Feature wiring

Plugins are opt-in at compile time:

- `crates/modulex-cli/Cargo.toml` and `crates/modulex-mcp/Cargo.toml`
  gain `plugin-<name> = ["dep:modulex-plugin-<name>"]` (add to `default`
  when the plugin is broadly useful); the binaries' `full_registry()`
  composition gets a `#[cfg(feature = "plugin-<name>")] register(...)`
  line.
- CI's lean-build step (`--no-default-features`) proves the engine builds
  without you; keep your dependencies out of everyone else's graph.

## 7. Store usage (when you need persistence)

Prefer step params and report data. When a plugin genuinely needs
persistent state, it owns its tables: create them idempotently keyed by a
`schema:<plugin>` version row in the store's `meta` table, never touch
core's `user_version`, and make the state exportable (the sovereignty
rule). The first backlog plugin to need this (#14's review tracker)
establishes the worked example.

## 8. Checklist for the PR

- [ ] README.md in the crate (step table, usage TOML, spawned programs)
- [ ] All handlers implement the full contract; schemas golden-pinned
- [ ] Conformance + mocked logic tests green with NO host dependence
- [ ] Live-contract tests for every real tool you parse
- [ ] required vs optional programs correct (fallbacks are optional)
- [ ] Feature flag wired in both binaries; lean build still passes
- [ ] Example config updated if the step belongs in the flagship routine
- [ ] PR states the disclosure tier
- [ ] `just check` green (fmt, clippy -D warnings, all tests)
