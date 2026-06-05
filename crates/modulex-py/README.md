# modulex-py

Python bindings for the [modulex](https://github.com/hartsock/modulex-mcp)
routine engine: embed the engine, register Python step handlers in-process,
run routines, and serve them to any MCP client.

## Usage

```python
import modulex_py

engine = modulex_py.Engine.from_config()   # $MODULEX_CONFIG → ./modulex.toml → ~/.modulex/config.toml

@engine.step("standup-notes")              # handler for `type = "standup-notes"` steps
def standup(spec: dict, ctx: dict) -> dict:
    return {"success": True, "output": "- shipped the leash\n- reviewed PR #7"}

report = engine.run_routine("morning", dry_run=True)
print(report.to_text())

engine.serve_stdio()                       # MCP on stdio, Python steps included
```

Handler contract: `fn(spec, ctx) -> dict | str | None`. Dict returns use the
plugin-protocol response shape (`success`/`skipped`/`output`/`error`/`data`);
a str is the report section body; `None` is success with no output. Raised
exceptions become failed steps — the routine continues (soft failure).

Register steps **before** the first `run_routine()`/`serve_stdio()` — the
exec leash is resolved once, at engine build.
