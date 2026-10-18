"""Scenario loader tests for the parity harness's asynchronous step forms."""

import pathlib
import re
from typing import Any

import pytest
import yaml

from rust.parity import scenario_file


def _load(tmp_path: pathlib.Path, steps: list[dict[str, Any]], **fields: Any) -> scenario_file.Scenario:
  document: dict[str, Any] = {"stage": "unit", "name": "loader", "steps": steps}
  document.update(fields)
  path = tmp_path / "scenario.yaml"
  path.write_text(yaml.safe_dump(document, sort_keys=False), encoding="utf-8")
  return scenario_file.load(path)


def _fake_agent() -> dict[str, Any]:
  return {"starts": []}


@pytest.mark.parametrize(
    "step,fields", [
        ({
            "name": "http step",
            "method": "GET",
            "path": "/api/auth/status",
            "defer_files": True
        }, {}),
        (
            {
                "kind": "wait",
                "name": "event wait",
                "timeout_seconds": 1,
                "until":
                    {
                        "event":
                            {
                                "session": "synthetic-session",
                                "type": "run_finished",
                                "fields": {
                                    "outcome": "success"
                                },
                                "count": 2,
                            },
                    }
            }, {}),
        (
            {
                "kind": "wait",
                "name": "file wait",
                "timeout_seconds": 1,
                "until":
                    {
                        "file_field":
                            {
                                "path": "sessions/synthetic/triggers/one.json",
                                "field": "status",
                                "equals": "fired"
                            },
                    }
            }, {}),
        (
            {
                "kind": "wait",
                "name": "gate wait",
                "timeout_seconds": 1,
                "until": {
                    "at_gate": "ready"
                }
            }, {
                "fake_agent": _fake_agent(),
                "expect_starts": 0,
            }),
        ({
            "kind": "gate",
            "name": "gate step",
            "open": "ready"
        }, {
            "fake_agent": _fake_agent(),
            "expect_starts": 0,
        }),
        (
            {
                "kind": "handover",
                "name": "handover step",
                "agent_after_stop": "alive"
            }, {
                "fake_agent": _fake_agent(),
                "expect_starts": 0,
                "directions": ["python-to-candidate"],
            }),
    ])
def test_new_step_kinds_and_keys_load(tmp_path: pathlib.Path, step: dict[str, Any], fields: dict[str, Any]) -> None:
  scenario = _load(tmp_path, [step], **fields)
  assert scenario.steps[0].name == step["name"]


@pytest.mark.parametrize(
    "step", [
        {
            "kind": "wait",
            "name": "bad wait",
            "timeout_seconds": 1
        },
        {
            "kind": "wait",
            "name": "missing timeout",
            "until": {
                "at_gate": "ready"
            }
        },
        {
            "kind": "wait",
            "name": "infinite timeout",
            "timeout_seconds": float("inf"),
            "until": {
                "at_gate": "ready"
            }
        },
        {
            "kind": "wait",
            "name": "bad condition",
            "timeout_seconds": 1,
            "until": {
                "unknown": "x"
            }
        },
        {
            "kind": "wait",
            "name": "bad count",
            "timeout_seconds": 1,
            "until": {
                "event": {
                    "session": "s",
                    "type": "run_finished",
                    "count": 0
                },
            }
        },
        {
            "kind": "gate",
            "name": "missing open"
        },
        {
            "kind": "handover",
            "name": "bad state",
            "agent_after_stop": "running"
        },
        {
            "name": "bad defer",
            "method": "GET",
            "path": "/api/auth/status",
            "defer_files": "yes"
        },
        {
            "kind": "wait",
            "name": "unknown wait key",
            "timeout_seconds": 1,
            "until": {
                "at_gate": "ready"
            },
            "open": "ready"
        },
    ])
def test_invalid_step_names_the_step(tmp_path: pathlib.Path, step: dict[str, Any]) -> None:
  fields: dict[str, Any] = {}
  if step.get("kind") in ("gate", "handover") or (step.get("until") and "at_gate" in step["until"]):
    fields.update(fake_agent=_fake_agent(), expect_starts=0)
  if step.get("kind") == "handover":
    fields["directions"] = ["python-to-candidate"]
  with pytest.raises(scenario_file.ScenarioError, match=step["name"]):
    _load(tmp_path, [step], **fields)


@pytest.mark.parametrize(
    "steps,fields,message", [
        (
            [{
                "kind": "handover",
                "name": "handover",
                "agent_after_stop": "alive"
            }], {
                "fake_agent": _fake_agent(),
                "expect_starts": 0,
            }, "step 'handover' is a handover, so directions are required"),
        (
            [{
                "name": "http",
                "method": "GET",
                "path": "/api/auth/status"
            }], {
                "fake_agent": _fake_agent(),
                "expect_starts": 0,
                "directions": ["python-to-candidate"],
            }, "directions are refused when no handover step is present"),
        (
            [{
                "kind": "handover",
                "name": "handover",
                "agent_after_stop": "alive"
            }], {
                "fake_agent": _fake_agent(),
                "expect_starts": 0,
                "directions": ["both"],
            }, "directions must be a subset of"),
        (
            [{
                "kind": "handover",
                "name": "handover",
                "agent_after_stop": "alive"
            }], {
                "fake_agent": _fake_agent(),
                "expect_starts": 0,
                "directions": None,
            }, "directions must be a non-empty list"),
    ])
def test_invalid_directions_fail_loading(
    tmp_path: pathlib.Path, steps: list[dict[str, Any]], fields: dict[str, Any], message: str) -> None:
  with pytest.raises(scenario_file.ScenarioError, match=re.escape(message)):
    _load(tmp_path, steps, **fields)


@pytest.mark.parametrize(
    "steps,fields,message", [
        ([{
            "kind": "gate",
            "name": "gate step",
            "open": "ready"
        }], {}, "step 'gate step' needs a fake_agent script"),
        (
            [{
                "kind": "wait",
                "name": "gate wait",
                "timeout_seconds": 1,
                "until": {
                    "at_gate": "ready"
                }
            }], {}, "step 'gate wait' needs a fake_agent script"),
        (
            [{
                "name": "http",
                "method": "GET",
                "path": "/api/auth/status"
            }], {
                "expect_starts": 0
            }, "fake_agent and expect_starts must be provided together"),
        (
            [{
                "name": "http",
                "method": "GET",
                "path": "/api/auth/status"
            }], {
                "fake_agent": _fake_agent()
            }, "fake_agent and expect_starts must be provided together"),
        (
            [{
                "name": "http",
                "method": "GET",
                "path": "/api/auth/status"
            }], {
                "fake_agent": _fake_agent(),
                "expect_starts": -1
            }, "expect_starts must be a non-negative integer"),
        (
            [{
                "name": "http",
                "method": "GET",
                "path": "/api/auth/status"
            }], {
                "fake_agent": {
                    "starts": "claude"
                },
                "expect_starts": 0
            }, "fake_agent must contain a starts list"),
        (
            [{
                "name": "http",
                "method": "GET",
                "path": "/api/auth/status"
            }], {
                "fake_agent": {
                    "starts": [{
                        "name": "claude",
                        "steps": []
                    }]
                },
                "expect_starts": 0
            }, "fake_agent start 1: requires name, steps and exit_code"),
        (
            [{
                "name": "http",
                "method": "GET",
                "path": "/api/auth/status"
            }], {
                "fake_agent": {
                    "starts": [{
                        "name": "claude",
                        "steps": [],
                        "exit_code": 256
                    }]
                },
                "expect_starts": 0
            }, "fake_agent start 1: exit_code must be an integer from 0 to 255"),
        (
            [{
                "name": "http",
                "method": "GET",
                "path": "/api/auth/status"
            }], {
                "fake_agent": {
                    "starts": [{
                        "name": "claude",
                        "steps": [{
                            "emit": "text"
                        }],
                        "exit_code": 0
                    }]
                },
                "expect_starts": 0
            }, "fake_agent start 1 step 1: emit must be an object"),
        (
            [{
                "name": "http",
                "method": "GET",
                "path": "/api/auth/status"
            }], {
                "fake_agent": {
                    "starts": [{
                        "name": "claude",
                        "steps": [{
                            "gate": ""
                        }],
                        "exit_code": 0
                    }]
                },
                "expect_starts": 0
            }, "fake_agent start 1 step 1: emit must be an object or gate must be a non-empty string"),
    ])
def test_invalid_script_fails_loading(
    tmp_path: pathlib.Path, steps: list[dict[str, Any]], fields: dict[str, Any], message: str) -> None:
  with pytest.raises(scenario_file.ScenarioError, match=re.escape(message)):
    _load(tmp_path, steps, **fields)
