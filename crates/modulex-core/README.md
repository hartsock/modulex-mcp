# modulex-core

The engine behind [modulex](https://github.com/hartsock/modulex-mcp): config,
credentials, step trait/registry, builtin steps, reports, and the gated exec
path. A *routine* is an ordered, config-defined sequence of steps producing one
structured report, identified by a monotonic generation counter (a causal
coordinate, never wall-clock).

Design pillars enforced here: every subprocess goes through `ExecGate::spawn`,
which passes an [agent-bridle](https://crates.io/crates/agent-bridle-core)
`check_exec` leash whose default grant is *deny everything except the programs
the configured steps declare*; config carries credential **references** only
(`{env=..}`, `{file=..}`, `{cmd=..}`), resolved at spawn time into a `Secret`
that is unprintable and unserializable by construction; step failures are soft
— they mark the report, never abort the routine.

Part of [modulex-mcp](https://github.com/hartsock/modulex-mcp), a
deterministic, pluggable routine engine for agents exposed as a CLI and a
stdio MCP server.

## License

MIT OR Apache-2.0, at your option.
