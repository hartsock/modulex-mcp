# modulex-mcp

Stdio MCP server over the [modulex](https://github.com/hartsock/modulex-mcp)
routine engine: newline-delimited JSON-RPC 2.0, MCP protocol `2024-11-05`,
five tools (`routine_run`, `routine_list`, `step_run`, `report_get`,
`steps_list`), serial dispatch. Failure semantics are deliberate: `isError`
is reserved for engine faults (unknown routine/tool, leash denial of the
run); per-step failures are *data inside the report*, so an agent can read
which step failed and why. Reports are identified by a monotonic generation
counter, never a timestamp.

Ships as a library plus a thin `modulex-mcp` binary, so the Python bindings
(`modulex-py`) can run the same server loop with Python-registered step
handlers.

Part of [modulex-mcp](https://github.com/hartsock/modulex-mcp), a
deterministic, pluggable routine engine for agents exposed as a CLI and a
stdio MCP server.

## License

MIT OR Apache-2.0, at your option.
