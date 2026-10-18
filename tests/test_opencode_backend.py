import asyncio
import json
import os
import pathlib
import signal
import sys
import time
from collections.abc import AsyncIterator
from unittest import mock

import httpx
import pytest
import pytest_asyncio

from src.backends.opencode import opencode, opencode_limits, server
from src.infra import event_types as ET

# A stand-in for the opencode 2.0 binary: serve (the shared server's HTTP surface) and run (one-shot).
FAKE_OPENCODE = pathlib.Path(__file__).with_name("fake_opencode_serve.py")
MODEL = "fakeprov/fake-model"
BASE_ENV = {"PATH": os.environ.get("PATH", "/usr/bin:/bin")}


def _read_log(path: pathlib.Path) -> list[dict]:
  return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []


@pytest.fixture
def fake_oc(tmp_path, monkeypatch):
  """The backend pointed at the fake 2.0 program, in its own profile and server registry."""
  wrapper = tmp_path / "opencode"
  wrapper.write_text(f'#!/bin/sh\nexec {sys.executable} {FAKE_OPENCODE} "$@"\n')
  wrapper.chmod(0o755)
  monkeypatch.setattr(server, "OPENCODE_BIN", str(wrapper))
  monkeypatch.setattr(server, "_servers", {})
  monkeypatch.setenv("CHARLIEBOT_HOME", str(tmp_path / "profile"))
  monkeypatch.setenv("FAKE_OC_LOG", str(tmp_path / "requests.jsonl"))
  monkeypatch.delenv("FAKE_OC_SCRIPT", raising=False)
  monkeypatch.delenv("FAKE_OC_VERSION", raising=False)
  for name in ("dir", "a", "b"):
    (tmp_path / name).mkdir()
  return tmp_path


def _script(fake_oc: pathlib.Path, monkeypatch, script: dict) -> None:
  path = fake_oc / "script.json"
  path.write_text(json.dumps(script))
  monkeypatch.setenv("FAKE_OC_SCRIPT", str(path))


@pytest_asyncio.fixture
async def stop_servers(fake_oc):
  """Stop every shared server a test started, so no fake process outlives its test."""
  yield
  for shared in list(server._servers.values()):
    if shared.process_alive:
      await shared._shutdown()


def _requests(fake_oc: pathlib.Path, method: str, path_prefix: str) -> list[dict]:
  return [
      r for r in _read_log(fake_oc / "requests.jsonl")
      if r.get("method") == method and r.get("path", "").startswith(path_prefix)
  ]


async def _loaded_directories(shared: server.OpenCodeServer) -> set[str]:
  response = await shared.client.get("/api/debug/location")
  return {entry["directory"] for entry in response.json()}


async def _run_turn(backend: opencode.OpenCodeBackend,
                    prompt: str,
                    cwd: pathlib.Path,
                    env: dict | None = None) -> list[dict]:
  return [event async for event in backend.run(prompt, str(cwd), env or BASE_ENV)]


def _text_events(text: str, message: str = "m1", ordinal: int = 0) -> list[dict]:
  return [{"type": "session.text.ended", "data": {"assistantMessageID": message, "ordinal": ordinal, "text": text}}]


def _usage_step(input_tokens: int, output_tokens: int) -> dict:
  return {
      "type": "session.step.ended",
      "data":
          {
              "tokens":
                  {
                      "input": input_tokens,
                      "output": output_tokens,
                      "reasoning": 0,
                      "cache": {
                          "read": 2,
                          "write": 1
                      }
                  },
              "cost": 0.25
          }
  }


# ---- Unit tests: configuration, translation, and model reading, with no process.


def test_headless_config_denies_the_three_tools_and_pins_bash():
  config = server.headless_config()
  assert config["shell"] == "bash"
  assert config["permission"]["question"] == "deny"
  assert config["permission"]["subagent"] == "deny"
  assert config["permission"]["opencode_session_move"] == "deny"


def test_server_env_drops_the_run_identity_and_points_xdg_at_the_store(fake_oc, monkeypatch):
  monkeypatch.setenv("CHARLIEBOT_RUN_TOKEN", "turn-token")
  monkeypatch.setenv("CHARLIEBOT_SESSION_ID", "session-x")
  # The no-proxy case must be hermetic: a host shell (or CI) may carry its own proxy variables.
  for name in ("HTTP_PROXY", "HTTPS_PROXY", "NO_PROXY", "http_proxy", "https_proxy", "no_proxy"):
    monkeypatch.delenv(name, raising=False)
  env = server.OpenCodeServer(None).server_env("pw")
  assert "CHARLIEBOT_RUN_TOKEN" not in env
  assert "CHARLIEBOT_SESSION_ID" not in env
  assert env["XDG_DATA_HOME"] == str(fake_oc / "profile" / "opencode" / "data")
  assert env["OPENCODE_SERVER_PASSWORD"] == "pw"
  assert env["OPENCODE_DISABLE_FFF"] == "1"
  assert "HTTP_PROXY" not in env
  proxied = server.OpenCodeServer("http://proxy.test:8080").server_env("pw")
  assert proxied["HTTPS_PROXY"] == "http://proxy.test:8080"
  assert proxied["NO_PROXY"].split(",")[-1] == "::1"


def test_translation_maps_text_tool_and_usage_events(fake_oc):
  backend = opencode.OpenCodeBackend(model=MODEL)
  backend._reset_run_state()
  assert backend._translate_event({"type": "session.step.started", "data": {"started": 1234}}) == []
  assert backend.model_request_started_ms == 1234
  assert backend._translate_event(
      {
          "type": "session.text.delta",
          "data": {
              "assistantMessageID": "m1",
              "ordinal": 0,
              "delta": "hel"
          }
      }) == [{
          "type": ET.ASSISTANT,
          "message": {
              "content": [{
                  "type": "text",
                  "text": "hel"
              }]
          }
      }]
  # A text part that streamed already is not repeated at its end; one that never streamed is.
  assert backend._translate_event(
      {
          "type": "session.text.ended",
          "data": {
              "assistantMessageID": "m1",
              "ordinal": 0,
              "text": "hel"
          }
      }) == []
  assert backend._translate_event(
      {
          "type": "session.text.ended",
          "data": {
              "assistantMessageID": "m1",
              "ordinal": 1,
              "text": "later"
          }
      }) != []
  backend._translate_event({"type": "session.tool.input.started", "data": {"id": "c1", "name": "shell"}})
  call = backend._translate_event(
      {
          "type": "session.tool.called",
          "data": {
              "id": "c1",
              "input": {
                  "command": "ls"
              },
              "executed": True
          }
      })
  assert call[0]["message"]["content"][0] == {
      "type": ET.TOOL_USE,
      "name": "shell",
      "id": "c1",
      "input": {
          "command": "ls"
      }
  }
  result = backend._translate_event(
      {
          "type": "session.tool.success",
          "data": {
              "id": "c1",
              "content": [{
                  "type": "text",
                  "text": "ok"
              }],
              "executed": True
          }
      })
  assert result == [{"type": ET.TOOL_RESULT, "tool_use_id": "c1", "content": "ok"}]
  backend._translate_event(_usage_step(10, 4))
  final = backend._make_accumulated_result()
  assert final["usage"]["input_tokens"] == 10
  assert final["usage"]["output_tokens"] == 4
  assert final["usage"]["cache_read_input_tokens"] == 2
  assert final["context_snapshot"]["tokens"]["input"] == 10


def test_tool_call_without_a_started_name_fails_the_translation(fake_oc):
  backend = opencode.OpenCodeBackend(model=MODEL)
  backend._reset_run_state()
  with pytest.raises(RuntimeError, match="without a tool name"):
    backend._translate_event({"type": "session.tool.called", "data": {"id": "c9", "input": {}, "executed": True}})


def test_model_limit_is_read_from_the_2_0_model_list():
  payload = {
      "location": {
          "directory": "/x"
      },
      "data": [{
          "id": "m",
          "providerID": "p",
          "limit": {
              "context": 9,
              "output": 3
          }
      }]
  }
  assert opencode.OpenCodeBackend._extract_model_limit(payload, "p", "m") == {"context": 9, "output": 3}
  assert opencode.OpenCodeBackend._extract_model_limit(payload, "p", "absent") is None


def test_image_attachments_become_data_urls_and_other_files_are_skipped(tmp_path):
  image = tmp_path / "a.png"
  image.write_bytes(b"\x89PNG")
  refs = [
      {
          "filename": "a.png",
          "path": str(image)
      }, {
          "filename": "notes.txt",
          "path": str(tmp_path / "n.txt")
      }, {
          "filename": "gone.jpg",
          "path": str(tmp_path / "gone.jpg")
      }
  ]
  attachments = opencode._image_file_attachments(refs)
  assert attachments == [{"uri": "data:image/png;base64,iVBORw==", "name": "a.png"}]


class _Locations:
  """A MockTransport handler for the debug-location routes: a loaded set, and the calls made against it."""

  def __init__(self, loaded: list[str], status: int = 204) -> None:
    self.loaded = loaded
    self.status = status
    self.calls: list[str] = []

  def __call__(self, request: httpx.Request) -> httpx.Response:
    if request.method == "GET" and request.url.path == "/api/debug/location":
      self.calls.append("list")
      return httpx.Response(200, json=[{"directory": directory} for directory in self.loaded])
    if request.method == "DELETE" and request.url.path == "/api/debug/location":
      directory = request.url.params["location[directory]"]
      self.calls.append(f"release {directory}")
      return httpx.Response(self.status)
    raise AssertionError(f"unexpected request {request.method} {request.url}")


def _release_server(locations: _Locations) -> server.OpenCodeServer:
  """A shared server whose process reads as running and whose client answers from *locations*; no process starts."""
  shared = server.OpenCodeServer(None)
  shared._proc = mock.Mock(pid=1, returncode=None)
  shared._client = httpx.AsyncClient(base_url="http://opencode.test", transport=httpx.MockTransport(locations))
  return shared


@pytest.mark.asyncio
async def test_release_unloads_each_unclaimed_directory_and_keeps_the_claimed_ones():
  locations = _Locations(["/w/a", "/w/b", "/w/c"])
  shared = _release_server(locations)
  await shared.claim("/w/a")
  await shared.claim("/w/b")
  await shared.release_claim("/w/b")
  assert locations.calls == ["list", "release /w/b", "release /w/c"]
  assert shared._claims == {"/w/a": 1}


@pytest.mark.asyncio
@pytest.mark.parametrize("state", ["stopping", "exited", "starting"])
async def test_release_is_skipped_unless_the_server_is_running(state):
  locations = _Locations(["/w/a"])
  shared = _release_server(locations)
  await shared.claim("/w/a")
  if state == "stopping":
    shared._stopping = True
  elif state == "exited":
    shared._proc.returncode = 0
  else:
    shared._client = None
  await shared.release_claim("/w/a")
  assert locations.calls == []
  assert not shared._claims


@pytest.mark.asyncio
async def test_failed_release_names_the_directory_and_the_status():
  shared = _release_server(_Locations(["/w/a"], status=500))
  await shared.claim("/w/b")
  with pytest.raises(RuntimeError, match=r"release of directory /w/a returned HTTP 500"):
    await shared.release_claim("/w/b")


@pytest.mark.asyncio
async def test_turn_end_steps_drop_the_claim_once_even_when_the_turn_steps_fail():
  backend = opencode.OpenCodeBackend(model=MODEL)
  backend._reset_run_state()
  shared = mock.Mock(release_claim=mock.AsyncMock())
  backend._claim = (shared, "/w/a")
  # finish() fails once; its second call is the idempotent no-op it is on a real turn.
  backend._turn = mock.Mock(finish=mock.AsyncMock(side_effect=[RuntimeError("interrupt failed"), None]))
  with pytest.raises(RuntimeError, match="interrupt failed"):
    await backend._turn_end_steps()
  shared.release_claim.assert_awaited_once_with("/w/a")
  await backend._turn_end_steps()
  backend._turn.finish.assert_awaited_once()
  shared.release_claim.assert_awaited_once_with("/w/a")


@pytest.mark.asyncio
async def test_claim_is_held_before_the_server_starts_and_dropped_when_the_start_fails(fake_oc, monkeypatch):
  backend = opencode.OpenCodeBackend(model=MODEL)
  shared = server.OpenCodeServer(None)
  directory = str(fake_oc / "dir")
  claims_at_start: list[int] = []

  async def failing_start() -> None:
    claims_at_start.append(shared._claims[directory])
    raise RuntimeError("opencode 2.0 binary not found")

  shared.ensure_running = failing_start
  monkeypatch.setattr(server, "server_for", lambda _proxy_url: shared)
  monkeypatch.setattr(backend, "_prepare_cwd", mock.Mock())
  monkeypatch.setattr(backend, "_end_turn_process", mock.AsyncMock())

  async def spawn_turn_process(_cwd: str, _env: dict) -> None:
    backend._exit_task = asyncio.get_running_loop().create_future()

  monkeypatch.setattr(backend, "_spawn_turn_process", spawn_turn_process)

  events = await _run_turn(backend, "x", fake_oc / "dir")

  assert claims_at_start == [1]
  assert events[-1]["type"] == ET.ERROR and "binary not found" in events[-1]["message"]
  assert shared._claims[directory] == 0


@pytest.mark.asyncio
async def test_failed_release_is_reported_as_a_turn_error_event(monkeypatch) -> None:
  backend = opencode.OpenCodeBackend(model=MODEL)
  turn = mock.Mock()
  turn.finish = mock.AsyncMock()
  turn.drop_earlier_interrupts = mock.Mock()
  shared = mock.Mock()
  shared.ensure_running = mock.AsyncMock()
  shared.claim = mock.AsyncMock()
  shared.release_claim = mock.AsyncMock(
      side_effect=RuntimeError("opencode release of directory /w/a returned HTTP 500"))
  shared.register = mock.Mock(return_value=turn)
  shared.wait_interrupts_settled = mock.AsyncMock()
  monkeypatch.setattr(server, "server_for", lambda _proxy_url: shared)
  monkeypatch.setattr(backend, "_prepare_cwd", mock.Mock())

  async def spawn_turn_process(_cwd: str, _env: dict) -> None:
    backend._exit_task = asyncio.get_running_loop().create_future()

  async def consume_turn(_turn: server.Turn) -> AsyncIterator[dict]:
    yield {"type": ET.RESULT}

  async def end_turn_process() -> None:
    backend._exit_task.cancel()

  monkeypatch.setattr(backend, "_spawn_turn_process", spawn_turn_process)
  monkeypatch.setattr(backend, "_open_session", mock.AsyncMock(return_value="ses"))
  monkeypatch.setattr(backend, "_fetch_model_limit", mock.AsyncMock(return_value=None))
  monkeypatch.setattr(backend, "_put_environment", mock.AsyncMock())
  monkeypatch.setattr(backend, "_send_prompt", mock.AsyncMock())
  monkeypatch.setattr(backend, "_consume_turn", consume_turn)
  monkeypatch.setattr(backend, "_end_turn_process", end_turn_process)

  events = [event async for event in backend.run("prompt", "/w/a", BASE_ENV)]

  assert events[-1]["type"] == ET.ERROR
  assert "directory /w/a" in events[-1]["message"]
  assert "HTTP 500" in events[-1]["message"]


@pytest.mark.asyncio
async def test_dispatch_routes_to_the_claimed_turn_and_settles_pending_interrupts(fake_oc):
  shared = server.OpenCodeServer(None)
  turn = shared.register("ses_1", str(fake_oc))
  shared._add_pending("ses_1")
  shared._dispatch({"type": "session.text.delta", "data": {"sessionID": "ses_1", "delta": "x"}})
  shared._dispatch({"type": "session.execution.interrupted", "data": {"sessionID": "ses_1"}})
  assert turn.events.qsize() == 2
  assert shared._settled_event("ses_1").is_set()
  shared._dispatch({"type": "session.text.delta", "data": {"sessionID": "ses_other", "delta": "y"}})
  assert turn.events.qsize() == 2
  with pytest.raises(RuntimeError, match="already has a running turn"):
    shared.register("ses_1", str(fake_oc))


@pytest.mark.asyncio
async def test_unclaimed_execution_is_interrupted(fake_oc):
  """An execution that starts with no turn registered earns an interrupt, which stays pending."""
  shared = server.OpenCodeServer(None)
  response = mock.MagicMock()
  response.json.return_value = {"interrupted": True}
  shared._client = mock.AsyncMock()
  shared._client.post.return_value = response
  shared._dispatch({"type": "session.execution.started", "data": {"sessionID": "ses_x"}})
  for task in list(shared._tasks):
    await task
  shared._client.post.assert_awaited_once_with("/api/session/ses_x/interrupt")
  assert shared._pending["ses_x"] == 1


@pytest.mark.asyncio
async def test_waiting_for_earlier_interrupts_times_out(fake_oc, monkeypatch):
  """A pending earlier interrupt that never settles fails the wait, naming the session."""
  monkeypatch.setattr(opencode_limits, "OPENCODE_INTERRUPT_SETTLE_TIMEOUT", 0.1)
  shared = server.OpenCodeServer(None)
  shared._add_pending("ses_x")
  with pytest.raises(RuntimeError, match="ses_x"):
    await shared.wait_interrupts_settled("ses_x")


# ---- Integration tests: real processes, the fake 2.0 program, and the shared server's HTTP surface.


@pytest.mark.integration
@pytest.mark.asyncio
async def test_turn_translates_events_sets_the_environment_and_ends_with_result(fake_oc, monkeypatch, stop_servers):
  _script(
      fake_oc, monkeypatch, {
          "prompts":
              {
                  "hello":
                      {
                          "events":
                              [
                                  {
                                      "type": "session.step.started",
                                      "data": {
                                          "started": 777
                                      }
                                  },
                                  {
                                      "type": "session.text.delta",
                                      "data": {
                                          "assistantMessageID": "m1",
                                          "ordinal": 0,
                                          "delta": "hi"
                                      }
                                  },
                                  {
                                      "type": "session.tool.input.started",
                                      "data": {
                                          "id": "c1",
                                          "name": "shell"
                                      }
                                  },
                                  {
                                      "type": "session.tool.called",
                                      "data": {
                                          "id": "c1",
                                          "input": {
                                              "command": "true"
                                          },
                                          "executed": True
                                      }
                                  },
                                  {
                                      "type": "session.tool.success",
                                      "data":
                                          {
                                              "id": "c1",
                                              "content": [{
                                                  "type": "text",
                                                  "text": "done"
                                              }],
                                              "executed": True
                                          }
                                  },
                                  _usage_step(5, 2),
                              ]
                      }
              }
      })
  backend = opencode.OpenCodeBackend(model=MODEL)
  events = await _run_turn(backend, "hello", fake_oc / "dir", {**BASE_ENV, "CHARLIEBOT_RUN_TOKEN": "turn-token"})
  kinds = [e["type"] for e in events]
  assert kinds[0] == ET.SESSION_ATTACHED
  assert events[1]["message"]["content"][0]["text"] == "hi"
  assert events[2]["message"]["content"][0]["type"] == ET.TOOL_USE
  assert events[3] == {"type": ET.TOOL_RESULT, "tool_use_id": "c1", "content": "done"}
  assert kinds[-1] == "result"
  assert events[-1]["usage"]["input_tokens"] == 5
  assert backend.exit_code == 0
  assert backend.model_request_started_ms == 777
  put = _requests(fake_oc, "PUT", "/api/session/")[0]
  assert put["body"]["variables"]["CHARLIEBOT_RUN_TOKEN"] == "turn-token"
  assert "BASH_ENV" not in put["body"]["variables"]  # no session cgroup on this host path
  created = next(
      r for r in _read_log(fake_oc / "requests.jsonl") if r.get("method") == "POST" and r.get("path") == "/api/session")
  assert created["body"]["model"] == {"id": "fake-model", "providerID": "fakeprov"}


@pytest.mark.integration
@pytest.mark.asyncio
async def test_turn_end_steps_run_in_order_and_remove_the_session_shells(fake_oc, monkeypatch, stop_servers):
  _script(
      fake_oc, monkeypatch, {
          "prompts":
              {
                  "work":
                      {
                          "shells": [{}],
                          "events":
                              [{
                                  "type": "session.execution.failed",
                                  "data": {
                                      "error": {
                                          "message": "model refused"
                                      }
                                  }
                              }],
                      }
              }
      })
  backend = opencode.OpenCodeBackend(model=MODEL)
  events = await _run_turn(backend, "work", fake_oc / "dir")
  assert events[-1]["type"] == ET.ERROR and events[-1]["message"] == "model refused"
  assert backend.exit_code == 1
  calls = [
      f"{r['method']} {r['path']}" for r in _read_log(fake_oc / "requests.jsonl")
      if r.get("method") in ("POST", "GET", "DELETE")
  ]
  interrupt = next(i for i, call in enumerate(calls) if call.endswith("/interrupt"))
  shell_list = next(i for i, call in enumerate(calls) if call.startswith("GET /api/shell"))
  shell_delete = next(i for i, call in enumerate(calls) if call.startswith("DELETE /api/shell/"))
  assert interrupt < shell_list < shell_delete
  # The directory's claim drops only after the shell delete, which would otherwise reload it.
  release = next(i for i, call in enumerate(calls) if call.startswith("DELETE /api/debug/location"))
  assert shell_delete < release
  release_request = _requests(fake_oc, "DELETE", "/api/debug/location")
  assert [r["query"]["location[directory]"] for r in release_request] == [[str(fake_oc / "dir")]]
  assert server.server_for(None).process_alive


@pytest.mark.integration
@pytest.mark.asyncio
async def test_concurrent_turns_share_one_server_process(fake_oc, monkeypatch, stop_servers):
  _script(
      fake_oc, monkeypatch,
      {"prompts": {
          "one": {
              "events": _text_events("first")
          },
          "two": {
              "events": _text_events("second")
          },
      }})
  first = opencode.OpenCodeBackend(model=MODEL)
  second = opencode.OpenCodeBackend(model=MODEL)
  await asyncio.gather(_run_turn(first, "one", fake_oc / "a"), _run_turn(second, "two", fake_oc / "b"))
  assert first.exit_code == 0 and second.exit_code == 0
  pids = {r["pid"] for r in _read_log(fake_oc / "requests.jsonl") if r.get("method") == "POST"}
  assert len(pids) == 1


@pytest.mark.integration
@pytest.mark.asyncio
async def test_cancel_signals_only_its_turn_and_the_other_turn_completes(fake_oc, monkeypatch, stop_servers):
  _script(
      fake_oc, monkeypatch,
      {"prompts": {
          "held": {
              "events": [],
              "hold": True
          },
          "quick": {
              "events": _text_events("done")
          },
      }})
  cancelled = opencode.OpenCodeBackend(model=MODEL)
  cancelled_events: list[dict] = []

  async def drive_cancelled() -> None:
    cancelled_events.extend([event async for event in cancelled.run("held", str(fake_oc / "a"), BASE_ENV)])

  task = asyncio.create_task(drive_cancelled())
  while not any(r.get("prompt_text") == "held" for r in _read_log(fake_oc / "requests.jsonl")):
    await asyncio.sleep(0.02)
  other = opencode.OpenCodeBackend(model=MODEL)
  other_events = await _run_turn(other, "quick", fake_oc / "b")
  os.kill(cancelled._proc.pid, signal.SIGTERM)
  await asyncio.wait_for(task, timeout=15)
  assert not any(e["type"] == ET.ERROR for e in cancelled_events)
  assert cancelled.exit_code != 0
  assert other_events[-1]["type"] == "result" and other.exit_code == 0
  assert server.server_for(None).process_alive
  cancelled_interrupt = f"/api/session/{cancelled._session_id}/interrupt"
  assert any(
      r.get("path") == cancelled_interrupt and r.get("body") is None for r in _read_log(fake_oc / "requests.jsonl"))


@pytest.mark.integration
@pytest.mark.asyncio
async def test_release_keeps_claimed_directories_and_terminate_releases_before_returning(
    fake_oc, monkeypatch, stop_servers):
  _script(
      fake_oc, monkeypatch,
      {"prompts": {
          "held": {
              "events": [],
              "hold": True
          },
          "quick": {
              "events": _text_events("done")
          },
      }})
  held = opencode.OpenCodeBackend(model=MODEL)
  held_run = asyncio.create_task(_run_turn(held, "held", fake_oc / "a"))
  while not any(r.get("prompt_text") == "held" for r in _read_log(fake_oc / "requests.jsonl")):
    await asyncio.sleep(0.02)
  shared = server.server_for(None)
  # A directory loaded outside any turn holds no claim, so the next turn's end releases it.
  await shared.client.get("/api/model", params={"location[directory]": str(fake_oc / "c")})
  # A second turn in the same directory ends first: the held turn's claim keeps the directory loaded.
  quick_events = await _run_turn(opencode.OpenCodeBackend(model=MODEL), "quick", fake_oc / "a")
  assert quick_events[-1]["type"] == "result"
  assert await _loaded_directories(shared) == {str(fake_oc / "a")}
  await held.terminate()
  assert await _loaded_directories(shared) == set()
  await asyncio.wait_for(held_run, timeout=15)
  releases = [r["query"]["location[directory]"] for r in _requests(fake_oc, "DELETE", "/api/debug/location")]
  assert releases.count([str(fake_oc / "a")]) == 1


@pytest.mark.integration
@pytest.mark.asyncio
async def test_permission_request_fails_only_its_turn(fake_oc, monkeypatch, stop_servers):
  _script(
      fake_oc, monkeypatch, {
          "prompts":
              {
                  "risky":
                      {
                          "events":
                              [{
                                  "type": "permission.asked",
                                  "data": {
                                      "permission": "shell",
                                      "patterns": ["rm -rf *"]
                                  }
                              }]
                      },
                  "calm": {
                      "events": _text_events("fine")
                  },
              }
      })
  risky_events, calm_events = await asyncio.gather(
      _run_turn(opencode.OpenCodeBackend(model=MODEL), "risky", fake_oc / "a"),
      _run_turn(opencode.OpenCodeBackend(model=MODEL), "calm", fake_oc / "b"))
  assert risky_events[-1]["type"] == ET.ERROR and "interactive permission" in risky_events[-1]["message"]
  assert calm_events[-1]["type"] == "result"


@pytest.mark.integration
@pytest.mark.asyncio
async def test_manager_turn_reloads_locations_before_the_prompt(fake_oc, monkeypatch, stop_servers):
  """A manager turn rewrites the session's AGENTS.md and reloads locations before the prompt; a
  turn without instructions writes no AGENTS.md and sends no reload."""
  _script(
      fake_oc, monkeypatch,
      {"prompts": {
          "manage": {
              "events": _text_events("managed")
          },
          "work": {
              "events": _text_events("worked")
          },
      }})
  manager = opencode.OpenCodeBackend(model=MODEL, instructions_content="# Fresh instructions")
  events = await _run_turn(manager, "manage", fake_oc / "a")
  assert events[-1]["type"] == "result"
  assert (fake_oc / "a" / "AGENTS.md").read_text() == "# Fresh instructions"
  worker_events = await _run_turn(opencode.OpenCodeBackend(model=MODEL), "work", fake_oc / "b")
  assert worker_events[-1]["type"] == "result"
  calls = [f"{r['method']} {r['path']}" for r in _read_log(fake_oc / "requests.jsonl") if r.get("method")]
  assert calls.count("POST /api/location/reload") == 1
  reload_at = calls.index("POST /api/location/reload")
  prompt_at = calls.index(f"POST /api/session/{manager._session_id}/prompt")
  assert reload_at < prompt_at


@pytest.mark.integration
@pytest.mark.asyncio
async def test_stream_loss_ends_running_turns_before_the_server_stops(fake_oc, monkeypatch, stop_servers):
  _script(fake_oc, monkeypatch, {"prompts": {"stuck": {"events": [], "hold": True, "shells": [{}]}}})
  backend = opencode.OpenCodeBackend(model=MODEL)
  events: list[dict] = []

  async def drive() -> None:
    # Per-item append is load-bearing: the wait loop below reads events while the turn is still running.
    async for event in backend.run("stuck", str(fake_oc / "dir"), BASE_ENV):
      events.append(event)  # noqa: PERF401  (see comment above)

  task = asyncio.create_task(drive())
  while not any(e["type"] == ET.SESSION_ATTACHED for e in events) or not _requests(fake_oc, "POST", "/api/session/"):
    await asyncio.sleep(0.02)
  while not any(r.get("path", "").endswith("/prompt") for r in _read_log(fake_oc / "requests.jsonl")):
    await asyncio.sleep(0.02)
  shared = server.server_for(None)
  await shared.client.post("/__control", json={"drop_stream": True})
  await asyncio.wait_for(task, timeout=15)
  errors = [e["message"] for e in events if e["type"] == ET.ERROR]
  assert errors and "event stream" in errors[0]
  assert not shared.process_alive
  deletes = [r for r in _read_log(fake_oc / "requests.jsonl") if r.get("method") == "DELETE"]
  # Only the shell delete of the turn-end steps: the loss sends no release, and the turn's claim
  # drops after the server stopped, so that release is skipped too.
  assert len(deletes) == 1 and deletes[0]["path"].startswith("/api/shell/")
  assert not _requests(fake_oc, "GET", "/api/debug/location")
  assert not shared._claims


@pytest.mark.integration
@pytest.mark.asyncio
async def test_server_crash_fails_the_turn_with_its_exit_code_and_the_next_turn_restarts(
    fake_oc, monkeypatch, stop_servers):
  _script(
      fake_oc, monkeypatch,
      {"prompts": {
          "crash": {
              "events": [],
              "hold": True
          },
          "after": {
              "events": _text_events("back")
          }
      }})
  backend = opencode.OpenCodeBackend(model=MODEL)
  events: list[dict] = []

  async def drive() -> None:
    events.extend([event async for event in backend.run("crash", str(fake_oc / "dir"), BASE_ENV)])

  task = asyncio.create_task(drive())
  while not any(r.get("path", "").endswith("/prompt") for r in _read_log(fake_oc / "requests.jsonl")):
    await asyncio.sleep(0.02)
  first_pid = server.server_for(None).pid
  await server.server_for(None).client.post("/__control", json={"exit": 3})
  await asyncio.wait_for(task, timeout=15)
  message = next(e["message"] for e in events if e["type"] == ET.ERROR)
  assert "exited with code 3" in message
  second = opencode.OpenCodeBackend(model=MODEL)
  await _run_turn(second, "after", fake_oc / "dir")
  assert second.exit_code == 0
  assert server.server_for(None).pid != first_pid


@pytest.mark.integration
@pytest.mark.asyncio
async def test_idle_server_exits_after_the_idle_limit(fake_oc, monkeypatch, stop_servers):
  monkeypatch.setattr(opencode_limits, "OPENCODE_SERVER_IDLE_EXIT_SECONDS", 0.2)
  _script(fake_oc, monkeypatch, {"prompts": {"idle": {"events": _text_events("ok")}}})
  await _run_turn(opencode.OpenCodeBackend(model=MODEL), "idle", fake_oc / "dir")
  shared = server.server_for(None)
  deadline = time.monotonic() + 5
  while shared.process_alive and time.monotonic() < deadline:
    await asyncio.sleep(0.05)
  assert not shared.process_alive


@pytest.mark.integration
@pytest.mark.asyncio
async def test_wrong_major_version_fails_the_turn_naming_the_binary(fake_oc, monkeypatch, stop_servers):
  monkeypatch.setenv("FAKE_OC_VERSION", "1.18.18")
  _script(fake_oc, monkeypatch, {"prompts": {}})
  events = await _run_turn(opencode.OpenCodeBackend(model=MODEL), "x", fake_oc / "dir")
  assert events[-1]["type"] == ET.ERROR
  assert "1.18.18" in events[-1]["message"] and str(server.OPENCODE_BIN) in events[-1]["message"]
  assert not server.server_for(None).process_alive
  assert not server.server_for(None)._claims


@pytest.mark.integration
@pytest.mark.asyncio
async def test_missing_resume_session_fails_the_turn_naming_the_id(fake_oc, monkeypatch, stop_servers):
  _script(fake_oc, monkeypatch, {"prompts": {}})
  shared = server.server_for(None)
  await shared.ensure_running()
  # The directory is loaded by an earlier turn; the failed turn's claim must still release it.
  await shared.client.get("/api/model", params={"location[directory]": str(fake_oc / "dir")})
  events = await _run_turn(opencode.OpenCodeBackend(model=MODEL, resume_session_id="ses_gone"), "x", fake_oc / "dir")
  assert events[-1]["type"] == ET.ERROR and "ses_gone" in events[-1]["message"]
  assert not _requests(fake_oc, "POST", "/api/session")
  releases = _requests(fake_oc, "DELETE", "/api/debug/location")
  assert [r["query"]["location[directory]"] for r in releases] == [[str(fake_oc / "dir")]]
  assert not shared._claims


@pytest.mark.integration
@pytest.mark.asyncio
async def test_missing_join_script_fails_before_the_environment_is_set(fake_oc, monkeypatch, stop_servers):
  monkeypatch.setattr(server, "JOIN_SCRIPT", fake_oc / "absent.sh")
  _script(fake_oc, monkeypatch, {"prompts": {}})
  events = await _run_turn(opencode.OpenCodeBackend(model=MODEL), "x", fake_oc / "dir")
  assert events[-1]["type"] == ET.ERROR and "join script is missing" in events[-1]["message"]
  assert not _requests(fake_oc, "PUT", "/api/session/")


@pytest.mark.integration
@pytest.mark.asyncio
async def test_silent_session_fails_after_the_progress_timeout(fake_oc, monkeypatch, stop_servers):
  monkeypatch.setattr(opencode_limits, "OPENCODE_SSE_PROGRESS_TIMEOUT", 0.3)
  _script(fake_oc, monkeypatch, {"prompts": {"quiet": {"events": [], "hold": True}}})
  backend = opencode.OpenCodeBackend(model=MODEL)
  events = await _run_turn(backend, "quiet", fake_oc / "dir")
  assert events[-1]["type"] == ET.ERROR
  assert "produced no event" in events[-1]["message"]
  assert backend.exit_code == 1


@pytest.mark.integration
@pytest.mark.asyncio
async def test_prompt_waits_for_an_earlier_interrupt_to_settle(fake_oc, monkeypatch, stop_servers):
  _script(
      fake_oc, monkeypatch, {
          "interrupt_event_delay": 0.4,
          "prompts": {
              "warm": {
                  "events": _text_events("warm")
              },
              "next": {
                  "events": _text_events("answered")
              },
          }
      })
  first = opencode.OpenCodeBackend(model=MODEL)
  await _run_turn(first, "warm", fake_oc / "dir")
  session_id = first._session_id
  shared = server.server_for(None)
  # An execution starts on the idle session with no turn: the server interrupts it and the interrupt is pending.
  await shared.client.post("/__control", json={"activate": session_id})
  await shared.client.post(
      "/__control", json={"emit": {
          "type": "session.execution.started",
          "data": {
              "sessionID": session_id
          }
      }})
  while not any(r.get("path", "").endswith("/interrupt") for r in _read_log(fake_oc / "requests.jsonl")):
    await asyncio.sleep(0.02)
  started = time.monotonic()
  second = opencode.OpenCodeBackend(model=MODEL, resume_session_id=session_id)
  events = await _run_turn(second, "next", fake_oc / "dir")
  assert events[-1]["type"] == "result", events
  assert time.monotonic() - started >= 0.3


@pytest.mark.integration
@pytest.mark.asyncio
async def test_one_shot_runs_standalone_in_a_private_root_and_fails_on_an_error_line(fake_oc, monkeypatch):
  _script(
      fake_oc, monkeypatch, {
          "run_lines":
              [
                  {
                      "type": "text",
                      "part": {
                          "type": "text",
                          "text": "Hello "
                      }
                  }, {
                      "type": "text",
                      "part": {
                          "type": "text",
                          "text": "world"
                      }
                  }
              ]
      })
  backend = opencode.OpenCodeBackend(model=MODEL)
  assert await backend.one_shot_text("say hi", "system", timeout=20) == "Hello world"
  run = next(r for r in _read_log(fake_oc / "requests.jsonl") if "run_args" in r)
  assert run["run_args"][:3] == ["--standalone", "--format", "json"]
  assert json.loads(run["config"]) == {"permission": {"*": "deny"}}
  assert not pathlib.Path(run["xdg_data_home"]).exists()
  _script(fake_oc, monkeypatch, {"run_lines": [{"type": "error", "error": {"message": "FreeTierError"}}]})
  with pytest.raises(RuntimeError, match="FreeTierError"):
    await backend.one_shot_text("say hi", "system", timeout=20)
