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
                "kind": "ws",
                "name": "wait for provider",
                "path": "/ws/voice/synthetic-session",
                "send": [{
                    "text": "{bad"
                }],
                "until": {
                    "frames": 1
                },
                "timeout_seconds": 1,
                "wait_for_fake_handshake": "gemini",
            }, {
                "fakes": {
                    "gemini": {}
                }
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
        (
            {
                "kind": "log_event",
                "name": "log event step",
                "event": "speech_models_ready",
                "fields": {
                    "ok": True
                },
                "timeout_seconds": 5,
            }, {}),
    ])
def test_new_step_kinds_and_keys_load(tmp_path: pathlib.Path, step: dict[str, Any], fields: dict[str, Any]) -> None:
  scenario = _load(tmp_path, [step], **fields)
  assert scenario.steps[0].name == step["name"]
  if step.get("wait_for_fake_handshake"):
    assert scenario.steps[0].wait_for_fake_handshake == step["wait_for_fake_handshake"]


def test_log_event_step_keeps_its_event_fields_and_timeout(tmp_path: pathlib.Path) -> None:
  step = {
      "kind": "log_event",
      "name": "ext usage fetch fails",
      "event": "ext_usage_fetched",
      "fields": {
          "ok": False,
          "attempt": 2
      },
      "timeout_seconds": 5,
  }
  parsed = _load(tmp_path, [step]).steps[0]
  assert parsed.route == "(log_event)"
  assert parsed.log_event == "ext_usage_fetched"
  assert parsed.log_fields == {"ok": "false", "attempt": "2"}
  assert parsed.timeout_seconds == 5.0


def test_ws_fake_handshake_requires_a_declared_fake(tmp_path: pathlib.Path) -> None:
  step = {
      "kind": "ws",
      "name": "wait for provider",
      "path": "/ws/voice/synthetic-session",
      "send": [{
          "text": "{bad"
      }],
      "until": {
          "frames": 1
      },
      "timeout_seconds": 1,
      "wait_for_fake_handshake": "gemini",
  }
  with pytest.raises(scenario_file.ScenarioError, match="not declared in fakes"):
    _load(tmp_path, [step])


@pytest.mark.parametrize(
    "fields", [
        {
            "config": {
                "paths": {
                    "workspace_dirs": ["${tree.root}/workspace"]
                }
            }
        },
        {
            "credentials": {
                "synthetic": {
                    "home": "${tree.home}"
                }
            }
        },
        {
            "fakes": {
                "slack": {},
                "discord": {},
                "gemini": {},
                "muse": {}
            }
        },
        {
            "fixture_extras": ["sample_files"]
        },
        {
            "voice_models": True
        },
    ])
def test_new_scenario_keys_load(tmp_path: pathlib.Path, fields: dict[str, Any]) -> None:
  scenario = _load(tmp_path, [{"name": "key scenario", "method": "GET", "path": "/api/auth/status"}], **fields)
  assert scenario.name == "loader"


@pytest.mark.parametrize(
    "step,fields", [
        (
            {
                "name": "raw body file",
                "method": "POST",
                "path": "/api/voice/${sessions.atlas}/confirm",
                "body_file": "audio/not-wave.bin",
            }, {}),
        (
            {
                "name": "multipart body",
                "method": "POST",
                "path": "/api/voice/${sessions.atlas}",
                "multipart": {
                    "audio": {
                        "file": "audio/silence.wav",
                        "content_type": "audio/wav"
                    },
                    "backend": "gemini",
                },
            }, {}),
        (
            {
                "kind": "ws",
                "name": "websocket frame forms",
                "path": "/ws/anything",
                "send": [{
                    "json": {
                        "type": "hello"
                    }
                }, {
                    "text": "plain"
                }, {
                    "binary_file": "audio/silence.pcm"
                }],
                "collect": {
                    "where": {
                        "type": "partial"
                    },
                    "field": "text",
                    "decode": "none"
                },
                "until": {
                    "frames": 1
                },
                "timeout_seconds": 1,
            }, {}),
        (
            {
                "kind": "ws",
                "name": "websocket rejected upgrade",
                "path": "/ws/anything",
                "auth": "none",
                "send": [],
                "until": {
                    "frames": 0
                },
                "timeout_seconds": 1,
                "expect_reject": 403,
            }, {}),
        (
            {
                "kind": "fake",
                "name": "fake inbound message",
                "fake": "slack",
                "action": "socket_event",
                "payload": {
                    "type": "message"
                },
            }, {
                "fakes": {
                    "slack": {}
                }
            }),
    ])
def test_new_http_websocket_and_fake_forms_load(
    tmp_path: pathlib.Path, step: dict[str, Any], fields: dict[str, Any]) -> None:
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
        {
            "name": "multiple HTTP bodies",
            "method": "POST",
            "path": "/api/auth/status",
            "json": {},
            "body_file": "audio/not-wave.bin",
        },
        {
            "name": "missing HTTP body file",
            "method": "POST",
            "path": "/api/auth/status",
            "body_file": "missing.bin",
        },
        {
            "name": "invalid multipart file fields",
            "method": "POST",
            "path": "/api/auth/status",
            "multipart": {
                "audio": {
                    "file": "audio/not-wave.bin"
                }
            },
        },
        {
            "kind": "ws",
            "name": "bad websocket path",
            "path": "/terminal",
            "until": {
                "frames": 0
            },
            "timeout_seconds": 1,
        },
        {
            "kind": "ws",
            "name": "missing websocket until",
            "path": "/ws/anything",
            "timeout_seconds": 1,
        },
        {
            "kind": "ws",
            "name": "bad websocket send frame",
            "path": "/ws/anything",
            "send": [{
                "json": {},
                "text": "two forms"
            }],
            "until": {
                "frames": 0
            },
            "timeout_seconds": 1,
        },
        {
            "kind": "ws",
            "name": "collected compare needs collect",
            "path": "/ws/anything",
            "until": {
                "frames": 0
            },
            "timeout_seconds": 1,
            "compare": {
                "collected_contains": ["ready"]
            },
        },
        {
            "kind": "ws",
            "name": "reject cannot send frames",
            "path": "/ws/anything",
            "send": [{
                "text": "unexpected"
            }],
            "until": {
                "frames": 0
            },
            "timeout_seconds": 1,
            "expect_reject": 403,
        },
        {
            "kind": "fake",
            "name": "unsupported fake action",
            "fake": "slack",
            "action": "dispatch",
            "payload": {},
        },
        {
            "kind": "fake",
            "name": "missing fake declaration",
            "fake": "slack",
            "action": "socket_event",
            "payload": {},
        },
        {
            "kind": "log_event",
            "name": "log event without a name",
            "timeout_seconds": 1,
        },
        {
            "kind": "log_event",
            "name": "log event with a list field",
            "event": "speech_models_ready",
            "fields": {
                "cache_dir": ["/tmp/synthetic"]
            },
            "timeout_seconds": 1,
        },
        {
            "kind": "log_event",
            "name": "log event without a timeout",
            "event": "speech_models_ready",
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


def test_tree_references_use_route_placeholders(tmp_path: pathlib.Path) -> None:
  scenario = _load(
      tmp_path, [{
          "name": "tree paths",
          "method": "GET",
          "path": "/absolute_filepath${tree.root}/${tree.home}",
      }])
  assert scenario.steps[0].route == "GET /absolute_filepath{tree}/{home}"


@pytest.mark.parametrize(
    "fields,message", [
        (
            {
                "credentials": {
                    "charliebot": {
                        "access_key": "replaced"
                    }
                }
            }, "scenario.yaml.credentials refuses the charliebot key"),
        (
            {
                "config": {
                    "paths": "${sessions.atlas}"
                }
            }, "scenario.yaml.config only accepts ${tree.root} and ${tree.home} references"),
        ({
            "fakes": {
                "twitter": {}
            }
        }, "scenario.yaml.fakes has unknown fake 'twitter'"),
        ({
            "fakes": {
                "slack": {
                    "tokens": 1
                }
            }
        }, "scenario.yaml.fakes.slack: unknown seed keys ['tokens']"),
        ({
            "fixture_extras": ["missing_extra"]
        }, "scenario.yaml.fixture_extras names missing module 'missing_extra'"),
        ({
            "voice_models": "yes"
        }, "scenario.yaml.voice_models must be a boolean"),
    ])
def test_invalid_scenario_overlay_keys_name_the_file(
    tmp_path: pathlib.Path, fields: dict[str, Any], message: str) -> None:
  with pytest.raises(scenario_file.ScenarioError, match=re.escape(message)):
    _load(tmp_path, [{"name": "http", "method": "GET", "path": "/api/auth/status"}], **fields)


def test_websocket_send_reference_fails_only_its_step(tmp_path: pathlib.Path) -> None:
  scenario = _load(
      tmp_path, [
          {
              "kind": "ws",
              "name": "send reference",
              "path": "/ws/anything",
              "send": [{
                  "json": {
                      "session": "${sessions.missing}"
                  }
              }],
              "until": {
                  "frames": 0
              },
              "timeout_seconds": 1,
          }
      ])
  manifest = {"sessions": {"atlas": "synthetic-session"}}
  scenario_file.check_references(scenario, manifest)
  message = scenario_file.unresolved_reference(scenario, scenario.steps[0], manifest, {}, "/tree", "/tree/home")
  assert message == "unresolved reference ${sessions.missing}"


def test_unresolved_capture_fails_only_its_step(tmp_path: pathlib.Path) -> None:
  # The send response lacked input_event_id, so the capture is absent on both sides when the wait step runs.
  until = {
      "event":
          {
              "session": "${sessions.atlas}",
              "type": "run_finished",
              "fields": {
                  "input_event_ids": ["${capture.input_id}"]
              },
          }
  }
  send = {
      "name": "send",
      "method": "POST",
      "path": "/api/chat/atlas/message",
      "capture": {
          "input_id": "input_event_id"
      }
  }
  wait = {"kind": "wait", "name": "wait for the run", "timeout_seconds": 1, "until": until}
  read = {"name": "read", "method": "GET", "path": "/api/auth/status"}
  scenario = _load(tmp_path, [send, wait, read])
  manifest = {"sessions": {"atlas": "synthetic-session"}}
  scenario_file.check_references(scenario, manifest)
  messages = [
      scenario_file.unresolved_reference(scenario, step, manifest, {}, "/tree", "/tree/home") for step in scenario.steps
  ]
  # The wait names the send step: its response lacked input_event_id, so that step never recorded the capture.
  assert messages == [None, "unresolved reference ${capture.input_id}: step 'send' should have captured it", None]
  # A reference that names a dict is a malformed file, not an unresolved one: it still stops the run.
  malformed = _load(tmp_path, [{"name": "read", "method": "GET", "path": "/api/${sessions}"}])
  with pytest.raises(scenario_file.ScenarioError, match=re.escape("names a dict")):
    scenario_file.check_references(malformed, manifest)


def test_unresolved_references_in_multipart_and_collect_fail_their_steps(tmp_path: pathlib.Path) -> None:
  capture = {"name": "capture", "method": "GET", "path": "/api/auth/status", "capture": {"input_id": "id"}}
  multipart = {
      "name": "multipart",
      "method": "POST",
      "path": "/api/upload",
      "multipart": {
          "text": "${capture.input_id}"
      },
  }
  collect = {
      "kind": "ws",
      "name": "collect",
      "path": "/ws/test",
      "collect": {
          "where": {
              "text": "${capture.input_id}"
          },
          "field": "text",
          "decode": "none"
      },
      "until": {
          "frames": 0
      },
      "timeout_seconds": 1,
  }
  scenario = _load(tmp_path, [capture, multipart, collect])
  manifest = {"sessions": {"atlas": "synthetic-session"}}
  scenario_file.check_references(scenario, manifest)
  messages = [
      scenario_file.unresolved_reference(scenario, step, manifest, {}, "/tree", "/tree/home") for step in scenario.steps
  ]
  producer = "unresolved reference ${capture.input_id}: step 'capture' should have captured it"
  assert messages == [None, producer, producer]


def test_http_status_and_shape_compare_loads(tmp_path: pathlib.Path) -> None:
  step = {
      "name": "invalid json",
      "method": "POST",
      "path": "/api/sessions/",
      "json": "{bad",
      "expect_status": 422,
      "compare": "status_and_shape",
  }
  scenario = _load(tmp_path, [step])
  assert scenario.steps[0].http_compare == "status_and_shape"


@pytest.mark.parametrize(
    "step,message", [
        (
            {
                "name": "not an error status",
                "method": "GET",
                "path": "/api/auth/status",
                "expect_status": 200,
                "compare": "status_and_shape",
            },
            "compare requires an expect_status of at least 400",
        ),
        (
            {
                "name": "no expected status",
                "method": "GET",
                "path": "/api/auth/status",
                "compare": "status_and_shape",
            },
            "compare requires an expect_status of at least 400",
        ),
        (
            {
                "name": "unknown compare mode",
                "method": "GET",
                "path": "/api/auth/status",
                "expect_status": 422,
                "compare": "frames",
            },
            "compare must be status_and_shape",
        ),
    ])
def test_http_compare_refused_without_an_error_status(
    tmp_path: pathlib.Path, step: dict[str, Any], message: str) -> None:
  with pytest.raises(scenario_file.ScenarioError, match=re.escape(message)):
    _load(tmp_path, [step])
