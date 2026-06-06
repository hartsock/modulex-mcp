<p align="center">
  <img src="docs/logos/modulex_logo_256x256.png" alt="modulex logo" width="160" />
</p>

# modulex

**A deterministic, pluggable routine engine for agents — CLI + MCP server.**

Modulex runs *routines*: ordered, configurable sequences of steps (repo health,
deadline countdowns, review queues, external tools) that produce a single
structured report. The same routine runs identically from a terminal, from
Claude Code, from newt-agent, or from any other MCP client — deterministically,
with no credentials in config and every subprocess gated by a capability leash.

**Why it exists:** every MCP server bloats by default — tool schemas flood
agent context, agents end up string-matching prose, and features pile into a
monolith. Modulex is built so that *cannot happen*: a versioned **data
contract** (agents never parse prose), **progressive disclosure** (the
default tool surface is ≤ 12 entries forever, CI-pinned; everything else is
discovered at the moment of need), and a **declared-authority plugin model**
(capability arrives as feature-gated step types, not core patches). Agent
cost stays constant while capability grows — **by construction, not by
vigilance**. See [docs/FOUNDATION.md](docs/FOUNDATION.md).

The flagship routine is the **good-morning dashboard**: tend your repos, count
your deadlines, scan your boards, list the reviews waiting on you — one
invocation, one report.

> Computer Science has as much to do with Computers as Astronomy does with
> Telescopes. Modulex is not about MCP or Rust — it's about starting your day
> with context, no matter which agent (or human) asks for it.

## Status

Early development — the [foundation pass](docs/FOUNDATION.md) (data contract
[#26], progressive disclosure [#32], plugin crate model [#10]) lands before
the plugin backlog (#11–#31). See `modulex.toml.example` for the
configuration surface.

[#26]: https://github.com/hartsock/modulex-mcp/issues/26
[#32]: https://github.com/hartsock/modulex-mcp/issues/32
[#10]: https://github.com/hartsock/modulex-mcp/issues/10

## Quick start

```bash
cp modulex.toml.example ~/.modulex/config.toml   # then edit
modulex doctor                # config path, leash, tool availability
modulex run morning --dry-run # describe without side effects
modulex run morning           # the real thing
```

### As an MCP server

```bash
claude mcp add modulex -- modulex-mcp
```

or in newt's `~/.newt/config.toml`:

```toml
[[mcp_servers]]
name = "modulex"
command = "modulex-mcp"
```

Tools: `routine_run`, `routine_list`, `step_run`, `report_get`, `steps_list`.
Per-step failures are *data inside the report*; `isError` is reserved for
engine faults (unknown routine, config errors, leash denial). Reports are
identified by a monotonic generation counter, never a timestamp.

```bash
modulex-mcp --probe   # dry-run the first routine and exit (sanity check)
modulex-mcp --tools   # print the tool specs
```

## Extending with Python

Two tiers:

**1. Plugin protocol** (`type = "python"`, any language, leashed subprocess):
the engine writes one JSON object to stdin, reads one from stdout — see
`examples/standup_notes.py` and the `modulex-plugin/1` spec in
`crates/modulex-core/src/steps/python.rs`.

**2. In-process via `modulex-py`** (`pip install modulex-py`) — Python hosts
the engine, so Python handlers run inside routines exactly like builtins,
including over MCP:

```python
import modulex_py

engine = modulex_py.Engine.from_config()

@engine.step("standup-notes")
def standup(spec: dict, ctx: dict) -> dict:
    return {"success": True, "output": "- shipped the leash"}

report = engine.run_routine("morning", dry_run=True)
print(report.to_text())

engine.serve_stdio()   # MCP on stdio, Python steps included
```

## Design pillars

- **Deterministic**: a routine is config-defined data, not agent improvisation.
  Reports are identified by a monotonic generation counter, never wall-clock.
- **Pluggable**: builtin step types plus a language-agnostic plugin protocol;
  Python handlers register in-process via `modulex-py` (PyO3).
- **No credentials at rest**: config holds *references* (`{env=..}`,
  `{file=..}`, `{cmd=..}`) resolved at spawn time and injected into the step's
  environment. Secrets are unprintable and unserializable by construction.
- **Leashed**: every subprocess passes an
  [agent-bridle](https://crates.io/crates/agent-bridle-core) `check_exec`
  gate. Default grant: only the programs your configured steps declare.

## Crates

| Crate | Role |
|---|---|
| `modulex-core` | the engine: config, steps, registry, reports, exec gate |
| `modulex-mcp`  | stdio MCP server binary |
| `modulex-cli`  | human CLI binary (`modulex`) |
| `modulex-py`   | PyO3 bindings — embed the engine, register Python steps |

## License

MIT OR Apache-2.0, at your option.
