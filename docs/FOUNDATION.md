# The Foundation Pass

**Why this project exists, and what must be built first.**

Tracking issues: [#26] (data contract) · [#32] (progressive disclosure) ·
[#10] (plugin crate model). Everything else waits for these three.

[#26]: https://github.com/hartsock/modulex-mcp/issues/26
[#32]: https://github.com/hartsock/modulex-mcp/issues/32
[#10]: https://github.com/hartsock/modulex-mcp/issues/10

---

## 1. The problem: MCP servers bloat by default

Every MCP server faces the same gravity. Capabilities arrive as tools; tools
arrive as schemas; and **every connected agent pays for every schema in its
context window on every session, whether it uses the tool or not.** A server
with 120 tools costs an agent ~35–45k tokens before any work happens — and
tool-selection accuracy *drops* as the menu grows. The result, across the
ecosystem, is a familiar failure shape:

- **Surface bloat** — `tools/list` grows linearly with features.
- **Prose coupling** — agents parse human-formatted output with string
  matching, so every cosmetic change breaks somebody's automation.
- **Monolith creep** — features land in the core because there is no
  disciplined seam for them to land anywhere else, so every user carries
  every dependency.

Plugin ecosystems make all three worse: more contributors, more tools, more
formats, faster.

modulex exists to demonstrate the alternative: **a plugin-extensible MCP
server whose cost to agents stays constant while its capability grows** —
not by discipline and review vigilance, but **by construction**. That
property is the product. The morning-dashboard routine is the demo.

## 2. The three pillars

Bloat has three faces (context cost, format coupling, dependency creep).
Each pillar eliminates one *structurally*:

### Pillar A — The data contract ([#26])

> Reports serve humans AND agents. The markdown is for the human; every step
> also emits a stable, versioned, typed `data` payload. **An agent never
> parses prose.**

- Every step type documents its `data` schema in rustdoc and exposes it
  machine-readably.
- Schemas are versioned with the crate; a breaking shape change is a
  breaking release with a regression test that proves the old shape fails
  loudly.
- `format: "data"` on `routine_run` / `step_run` / `report_get` returns only
  the structured payloads — the agent-native view of a report.

This kills prose coupling: cosmetic report changes can never break an agent,
and schema changes are visible, versioned events.

### Pillar B — Progressive disclosure ([#32])

> An agent always sees a **small, stable index** of what is possible;
> detail — schemas, knowledge, data — is disclosed **at the moment of
> need**, never preloaded.

Four mechanisms, one principle:

1. **Steps, not tools.** Capability lands as *step types* (config-driven,
   zero MCP surface). Read paths live in routines; only **mutations** earn a
   tool.
2. **Generic store dispatch.** `store_put` / `store_query` / `store_close`
   (kind-keyed) instead of N×CRUD tools; per-kind record schemas are
   disclosed on demand, not listed.
3. **Facets.** `[mcp] expose = [...]` (with an env override, same three-tier
   sourcing as the exec leash) gates which tool groups a connection sees —
   deny-by-default beyond `core`. One binary can also mount as a narrow
   single-facet server.
4. **Discovery meta-tools.** `tool_search` → `tool_describe` →
   `tool_invoke`: the long tail is searchable and callable without ever
   appearing in `tools/list`. Plus `routine_eval` — one tool that accepts an
   inline routine definition, so arbitrary composition costs one schema.

**The budget: the default `tools/list` is ≤ 12 entries, forever, pinned by a
CI test.** Growing it is a deliberate, reviewed change to a named constant —
not a side effect of adding a feature.

This kills surface bloat: context cost is proportional to what an agent
*uses*, not to what the server *has*.

### Pillar C — The plugin crate model ([#10])

> Plugins are crates with a narrow, declared seam — not patches to the core.

- A plugin is a `modulex-plugin-<name>` crate exporting
  `register(registry)`: its step types, its (facet-scoped) tool specs, its
  store migrations.
- **Feature-gated linkage**: each plugin's dependencies stay out of every
  build that doesn't ask for it (`--no-default-features` → lean engine;
  `full` → everything). CI builds the matrix.
- **Declared authority**: plugins declare the programs they spawn and the
  hosts they reach; the declarations feed the default deny-all-except
  exec/net grants. A plugin cannot quietly widen the leash.
- **Owned store slices**: per-plugin schema versions in the store's `meta`
  table; plugins migrate their own tables and never touch core's.

This kills monolith creep: the core stays a small engine; capability is
opt-in at compile time, disclosed at runtime, and leashed at execution time.

## 3. How the pillars interlock

A new capability, end to end, under all three pillars:

```
capability  =  step type        (B1: zero tool surface)
            +  data schema      (A: versioned, machine-readable)
            +  [mutation tool]  (B: facet-scoped, discovered not listed)
            +  plugin crate     (C: feature-gated deps, declared leash)
```

The costs that normally grow linearly become flat or opt-in:

| Cost | Naive MCP server | modulex |
|---|---|---|
| Agent context (tools/list) | O(features) | **O(1)** — ≤12 entries, CI-pinned |
| Agent context (per task) | all schemas | only the 1–2 schemas it pulls |
| Format breakage risk | every report tweak | **zero** — data contract versioned |
| Binary size / dep graph | every user carries all | feature-gated, opt-in |
| Leash surface | grows silently | declared per plugin, deny-by-default |

That table is the pitch. Nothing in it depends on reviewer vigilance; every
row is enforced by a test, a type, or a build flag.

## 4. Implementation plan (the foundation pass)

Ordered, small PRs. Nothing from the plugin backlog starts until F5 is done.

### F1 — Data contract core (#26)

- `StepData` documentation convention + per-step-type JSON schema, exposed
  via an extended `steps_list` (name, description, data schema).
- Populate `data` for every existing builtin (git steps → typed per-repo
  status enums; deadline/countdown → numeric days; reminders already typed).
- `format: "data"` on the three report-bearing tools.
- Tests: every builtin has a schema; every schema validates against the
  step's real output in the existing test suite; a schema-stability
  regression harness (golden schemas, breaking change = failing test).

### F2 — Tool registry refactor (prerequisite for B and C)

- Replace the match-based tool dispatch with a `ToolSpec` registry: name,
  description, input schema, **`mutates: bool`** (declared, not guessed),
  facet, handler.
- Builtins re-register through it; behavior identical (existing server tests
  must pass unchanged — that's the regression net).

### F3 — Progressive disclosure (#32)

- Facets: `[mcp] expose` + env override, provenance banner (leash-style);
  `tools/list` filtered by facet; `notifications/tools/list_changed`.
- Discovery trio: `tool_search`, `tool_describe`, `tool_invoke` (facet- and
  mutates-aware dispatch).
- Store dispatch trio: `store_put`/`store_query`/`store_close`; existing
  specific tools become aliases inside a non-default `store-classic` facet.
- **The budget test**: default-facet tool count pinned in CI.

### F4 — `routine_eval`

- Inline routine definitions (steps array, identical semantics to config),
  same leash, same soft-failure report, generation-stamped like any run.
- Declared-default grant interaction: inline steps may only use programs
  already in the grant — `routine_eval` discloses capability, never widens
  authority.

### F5 — Reference plugin (#10 proven end to end)

- Extract/build ONE plugin under the model (`modulex-plugin-health` is the
  candidate: no network, few deps, obviously useful) with its own README,
  tests, feature flag, facet, data schemas, declared programs.
- Write `docs/PLUGIN_AUTHORING.md` from the experience — the contract every
  backlog issue (#11–#31) then follows.
- CI: feature-matrix builds (`--no-default-features`, default, `full`).

### Definition of done for the pass

- [ ] An agent runs the morning routine and acts on **every** section
      without one string-match against markdown (`format: "data"` only).
- [ ] `tools/list` ≤ 12 with all foundation features merged — and a CI test
      fails if it grows.
- [ ] `tool_describe`/`tool_invoke` reach every registered tool, including
      the reference plugin's, with facets enforced.
- [ ] `cargo build --no-default-features` yields a lean engine without the
      reference plugin's deps; `full` includes it.
- [ ] `docs/PLUGIN_AUTHORING.md` exists and the reference plugin's PR was
      written against it.

## 5. The anti-bloat rules (standing law)

These outlive the pass; PR review enforces them; most are CI-enforced:

1. **Steps before tools.** New capability is a step type unless it mutates
   something on the user's behalf. Tool additions justify themselves in the
   PR description.
2. **The budget is a constant.** The default tool surface is a named,
   CI-pinned number. Changing it is its own commit with its own reasoning.
3. **Schemas are contracts.** `data` shapes version with the crate;
   breaking one without the regression-harness dance is a blocked PR.
4. **Plugins declare their authority.** Programs and hosts in the manifest
   seam, surfaced by `doctor`, folded into deny-by-default grants.
5. **Disclosure tier stated in every PR.** Each addition names where it
   lands: step / store kind / facet tool / discovered tool. "Default
   surface" is an exceptional answer that needs an exceptional reason.
6. All existing hard rules hold (leashed exec/net, unserializable secrets,
   generation counters not wall-clock, soft failures, TDD + regression
   tests, 80% coverage floor, README per crate).

## 6. What this gives the community

A worked, tested answer to the question every MCP server hits at scale:
*how do you grow capability without growing the bill every agent pays?*
The pattern — *data contract + progressive disclosure + declared-authority
plugins* — is general. modulex is its reference implementation: small
enough to read, disciplined enough to copy, and demonstrated end-to-end on
a routine real people run every morning.
