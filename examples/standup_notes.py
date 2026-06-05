#!/usr/bin/env python3
"""Example modulex plugin (protocol modulex-plugin/1).

Wire it into a routine:

    [[routines.morning.steps]]
    name = "standup notes"
    type = "python"
    script = "~/.modulex/plugins/standup_notes.py"

The engine writes one JSON object to stdin and reads one JSON object from
stdout. Credentials arrive ONLY via injected environment variables (the
`env = { NAME = {...} }` references on the step) — never in the JSON.
"""

import json
import sys


def main() -> int:
    request = json.load(sys.stdin)
    assert request["protocol"] == "modulex-plugin/1"

    if request["dry_run"]:
        response = {
            "protocol": "modulex-plugin/1",
            "output": "[dry-run] would gather standup notes",
        }
    else:
        repos = request["shared"]["repos"]
        response = {
            "protocol": "modulex-plugin/1",
            "success": True,
            "output": f"- tracked {len(repos)} repos\n- generation {request['generation']}",
            "data": {"repo_count": len(repos)},
        }

    json.dump(response, sys.stdout)
    return 0


if __name__ == "__main__":
    sys.exit(main())
