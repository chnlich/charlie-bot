"""CLI restart-crossing contract: bounded connect retry, server rejection, and
readback determinism, incl. the two readback scenarios that nothing else in
the tree constructs.

All tests here are in-process: no CLI subprocess, no real CharlieBot server.
Network-shaped tests either mock the ``_request_post``/``_request_get``
transport adapters directly (gaps 1, 2, and the local-file readbacks) or run a
tiny stub HTTP listener on 127.0.0.1 (the `plan` readback, which needs a real
GET response) — never a real port scan, process name, or pgrep.
"""

from __future__ import annotations

import http.server
import json
import pathlib
import sys
import threading

import conftest
import pytest

from src.infra import config
from src.runtime import control_events
from src.runtime.cli import common
from src.runtime.cli import session as session_module


def _cfg(tmp_path: pathlib.Path, **overrides: object) -> config.CharlieBotConfig:
  return config.CharlieBotConfig(charliebot_home=tmp_path / "home", **overrides)


class _FakeClock:
  """Replaces ``src.runtime.cli.common.time`` so retry backoff never sleeps for real."""

  def __init__(self) -> None:
    self.now = 0.0
    self.sleeps: list[float] = []

  def monotonic(self) -> float:
    return self.now

  def sleep(self, seconds: float) -> None:
    self.sleeps.append(seconds)
    self.now += seconds


def _connect_refused() -> common._ConnectPhaseError:
  return common._ConnectPhaseError("[Errno 111] Connection refused")


def _reset_after_send() -> common._SentButLostError:
  """A connection that completed its handshake before dying: sent-but-lost, never a retry class."""
  return common._SentButLostError("[Errno 104] Connection reset by peer")


def _patch_readback_env(monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path) -> config.CharlieBotConfig:
  """Point the config reads at a fresh config, make every POST a sent-but-lost reset, and
  return the config.

  ``common``'s forwarder and the verbs' deferred imports both read the ``get_config``
  attribute on ``src.infra.config`` at call time, so patching it beside ``common``'s own
  name covers every verb shape (import-scope binding or call-scope import) or the
  readback would read the host's profile. The transport patch lands on ``common``'s
  adapter, which ``_request_with_contract`` reads at call time.
  """
  cfg = _cfg(tmp_path)
  monkeypatch.setattr(conftest.CLI_COMMON_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  monkeypatch.setattr(conftest.CONFIG_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  monkeypatch.setattr(
      conftest.CLI_COMMON_TRANSPORT_POST_PATCH_TARGET, lambda *a, **k: (_ for _ in ()).throw(_reset_after_send()))
  return cfg


# ---------------------------------------------------------------------------
# Gap 1 — server_unavailable and the bounded retry
# ---------------------------------------------------------------------------


def test_connect_never_established_retries_with_backoff_then_exhausts(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _cfg(tmp_path)
  monkeypatch.setattr(conftest.CLI_COMMON_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  clock = _FakeClock()
  monkeypatch.setattr(common, "time", clock)
  monkeypatch.setattr(conftest.CLI_COMMON_CONNECT_TOTAL_TIMEOUT_PATCH_TARGET, 2.0)

  call_count = 0

  def fake_post(*args: object, **kwargs: object) -> None:
    nonlocal call_count
    call_count += 1
    raise _connect_refused()

  monkeypatch.setattr(conftest.CLI_COMMON_TRANSPORT_POST_PATCH_TARGET, fake_post)

  with pytest.raises(SystemExit) as exc_info:
    common.post_internal_api("/api/internal/x", {"a": 1})

  assert exc_info.value.code == 1
  # Retried, not failed on the first attempt.
  assert call_count > 1
  # Exponential backoff: base delay doubling until the remaining budget clamps it.
  assert clock.sleeps[:3] == [0.25, 0.5, 1.0]
  assert call_count == len(clock.sleeps) + 1
  # Bounded by the (shrunk) total budget — never exceeds it.
  assert sum(clock.sleeps) <= 2.0

  error = json.loads(capsys.readouterr().err)
  assert error == {"error": str(_connect_refused()), "code": "server_unavailable", "effect": "none"}


# ---------------------------------------------------------------------------
# Gap 3(a) — readback determinism for improve, schedule-trigger, and plan
# ---------------------------------------------------------------------------

# ---------------------------------------------------------------------------
# Session close readback -- a lost cancel/complete response reads back this
# request's own close fact from the task's chat_events.jsonl.
# ---------------------------------------------------------------------------


def test_session_cancel_readback_resolves_to_own_task_closed_fact_on_sent_but_lost(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _patch_readback_env(monkeypatch, tmp_path)

  session_id = "sess-cancel"
  request_id = "req-cancel-1"
  closed_id = control_events.stable_close_event_id(session_id, request_id)
  events_dir = cfg.sessions_dir / session_id / "data"
  events_dir.mkdir(parents=True)
  (events_dir / "chat_events.jsonl").write_text(
      "\n".join(
          [
              # An earlier request's closure shares the history; only this
              # call's own stable event id may answer it.
              json.dumps(
                  {
                      "id": control_events.stable_close_event_id(session_id, "req-earlier"),
                      "type": "task_closed",
                      "request_id": "req-earlier",
                      "outcome": "cancelled",
                  }),
              json.dumps({
                  "id": closed_id,
                  "type": "task_closed",
                  "request_id": request_id,
                  "outcome": "cancelled",
              }),
          ]) + "\n",
      encoding="utf-8")

  monkeypatch.setattr(
      sys, "argv", ["charliebot-session", "cancel", session_id, "--reason", "superseded", "--request-id", request_id])

  session_module.main()

  out = json.loads(capsys.readouterr().out)
  assert out == {"session_id": session_id, "task_state": "cancelled", "closed_event_id": closed_id}


class _QuietHandler(http.server.BaseHTTPRequestHandler):
  """Base for the stub listeners' handlers: silences the per-request stderr log
  line the stdlib writes; the base class dispatches ``log_message`` by name."""

  def log_message(self, format: str, *args: object) -> None:  # noqa: A002  stdlib signature mirror
    pass


class _StubListener:
  """Serves one handler class on a free 127.0.0.1 port from a daemon thread;
  ``close()`` shuts the server down. Subclasses define the handler."""

  def __init__(self, handler: type[http.server.BaseHTTPRequestHandler]) -> None:
    self._httpd = http.server.HTTPServer(("127.0.0.1", 0), handler)
    self.port = self._httpd.server_address[1]
    self._thread = threading.Thread(target=self._httpd.serve_forever, daemon=True)
    self._thread.start()

  def close(self) -> None:
    self._httpd.shutdown()
    self._httpd.server_close()


class _CapturePostListener(_StubListener):
  """A stub that answers 200 to POST and records the wire request: the plain-HTTP
  client's serialization (Content-Type, body bytes, query string) is this module's
  own code now, so it needs a real-socket pin."""

  def __init__(self) -> None:
    received: dict = {}
    self.received = received

    class Handler(_QuietHandler):

      def do_POST(self) -> None:
        length = int(self.headers.get("Content-Length", "0"))
        received["path"] = self.path
        received["content_type"] = self.headers.get("Content-Type")
        received["authorization"] = self.headers.get("Authorization")
        received["body"] = json.loads(self.rfile.read(length)) if length else None
        body = json.dumps({"ok": True}).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    super().__init__(Handler)


def test_post_sends_json_body_content_type_and_auth_header_over_the_real_client(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  stub = _CapturePostListener()
  try:
    cfg = _cfg(tmp_path, server={"port": stub.port})
    monkeypatch.setattr(conftest.CLI_COMMON_GET_CONFIG_PATCH_TARGET, lambda: cfg)

    result = common._request_with_contract(
        "POST", "/api/internal/x", payload={"a": 1}, params={"k": "v"}, unknown_effect="none")

    assert result == {"ok": True}
    assert stub.received["path"] == "/api/internal/x?k=v"
    assert stub.received["content_type"] == "application/json"
    assert stub.received["authorization"] is None  # scratch config carries no access key
    assert stub.received["body"] == {"a": 1}
  finally:
    stub.close()
