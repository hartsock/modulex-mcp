"""pytest suite for the modulex_py wheel.

Run after `maturin develop` in a venv:

    cd crates/modulex-py && maturin develop && pytest
"""

import json
import textwrap

import pytest

modulex_py = pytest.importorskip("modulex_py")


@pytest.fixture()
def config_path(tmp_path):
    config = tmp_path / "modulex.toml"
    config.write_text(
        textwrap.dedent(
            """
            [[routines.demo.steps]]
            name = "notes"
            type = "standup-notes"

            [[routines.demo.steps]]
            name = "deadlines"
            type = "deadline-calc"
            """
        )
    )
    return str(config)


def test_python_step_runs_inside_routine(config_path):
    engine = modulex_py.Engine.from_config(config_path)

    @engine.step("standup-notes")
    def standup(spec, ctx):
        assert spec["name"] == "notes"
        assert ctx["generation"] >= 1
        return {"success": True, "output": "- did things"}

    report = engine.run_routine("demo")
    assert report.success
    assert "- did things" in report.to_text()
    payload = json.loads(report.to_json())
    assert payload["step_results"][0]["step_type"] == "standup-notes"


def test_string_and_none_returns(config_path):
    engine = modulex_py.Engine.from_config(config_path)
    engine.register_step("standup-notes", lambda spec, ctx: "plain text body")
    report = engine.run_routine("demo")
    assert report.success
    assert "plain text body" in report.to_text()


def test_exception_is_a_soft_failure(config_path):
    engine = modulex_py.Engine.from_config(config_path)

    @engine.step("standup-notes")
    def boom(spec, ctx):
        raise RuntimeError("kaput")

    report = engine.run_routine("demo")
    assert not report.success
    payload = json.loads(report.to_json())
    assert "kaput" in payload["step_results"][0]["error"]
    # The routine continued: the second step still ran.
    assert len(payload["step_results"]) == 2


def test_register_after_build_is_an_error(config_path):
    engine = modulex_py.Engine.from_config(config_path)
    engine.register_step("standup-notes", lambda s, c: None)
    engine.run_routine("demo", dry_run=True)
    with pytest.raises(RuntimeError):
        engine.register_step("late", lambda s, c: None)


def test_generations_are_monotonic(config_path):
    engine = modulex_py.Engine.from_config(config_path)
    engine.register_step("standup-notes", lambda s, c: None)
    first = engine.run_routine("demo", dry_run=True)
    second = engine.run_routine("demo", dry_run=True)
    assert (first.generation, second.generation) == (1, 2)
