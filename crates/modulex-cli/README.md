# modulex-cli

The human CLI (`modulex`) over the
[modulex](https://github.com/hartsock/modulex-mcp) routine engine. One-shot by
design: load config (`--config`, `$MODULEX_CONFIG`, `./modulex.toml`, or
`~/.modulex/config.toml`), resolve the agent-bridle exec leash, run, print the
report, exit. Subcommands cover running a routine (`run`, with `--only`,
`--skip`, and `--dry-run`), running a single step, listing routines and step
types, and `doctor` for config/leash/tool diagnostics.

The long-lived agent-facing surface is the sibling `modulex-mcp` binary; this
crate is for terminals.

Part of [modulex-mcp](https://github.com/hartsock/modulex-mcp), a
deterministic, pluggable routine engine for agents exposed as a CLI and a
stdio MCP server.

## License

MIT OR Apache-2.0, at your option.
