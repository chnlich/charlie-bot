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
import socket
import struct
import subprocess
import sys
import threading
import time
from pathlib import Path
from unittest.mock import MagicMock

import pytest
from conftest import (
    CLI_COMMON_CONNECT_TOTAL_TIMEOUT_PATCH_TARGET,
    CLI_COMMON_GET_CONFIG_PATCH_TARGET,
    CLI_COMMON_MAYBE_VERSION_SKEW_HINT_PATCH_TARGET,
    CLI_COMMON_TRANSPORT_POST_PATCH_TARGET,
    CONFIG_GET_CONFIG_PATCH_TARGET,
    ROOT,
    make_json_response,
    write_trigger,
)

from src.cli import common
from src.cli import improve as improve_module
from src.cli import plan as plan_module
from src.cli import schedule_trigger as schedule_trigger_module
from src.cli.plan import _PLAN_REMINDER
from src.core.config import CharlieBotConfig
from src.core.models import PendingTrigger, TriggerStatus


def _cfg(tmp_path: Path, **overrides: object) -> CharlieBotConfig:
  return CharlieBotConfig(charliebot_home=tmp_path / "home", **overrides)


def _write_thread(cfg: CharlieBotConfig, session_id: str, thread_id: str, **fields: object) -> None:
  thread_dir = cfg.sessions_dir / session_id / "threads" / thread_id
  thread_dir.mkdir(parents=True, exist_ok=True)
  meta = {"id": thread_id, "session_id": session_id, "description": "d", "created_at": "2024-01-01T00:00:00+00:00"}
  meta.update(fields)
  (thread_dir / "metadata.json").write_text(json.dumps(meta), encoding="utf-8")


class _FakeClock:
  """Replaces ``src.cli.common.time`` so retry backoff never sleeps for real."""

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


def _patch_readback_env(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> CharlieBotConfig:
  """Point the config reads at a fresh config, make every POST a sent-but-lost reset, and
  return the config.

  ``common``'s forwarder and the verbs' deferred imports both read the ``get_config``
  attribute on ``src.core.config`` at call time, so patching it beside ``common``'s own
  name covers every verb shape (import-scope binding or call-scope import) or the
  readback would read the host's profile. The transport patch lands on ``common``'s
  adapter, which ``_request_with_contract`` reads at call time.
  """
  cfg = _cfg(tmp_path)
  monkeypatch.setattr(CLI_COMMON_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  monkeypatch.setattr(CONFIG_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  monkeypatch.setattr(
      CLI_COMMON_TRANSPORT_POST_PATCH_TARGET, lambda *a, **k: (_ for _ in ()).throw(_reset_after_send()))
  return cfg


# ---------------------------------------------------------------------------
# Gap 1 — server_unavailable and the bounded retry
# ---------------------------------------------------------------------------


def test_connect_never_established_retries_with_backoff_then_exhausts(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _cfg(tmp_path)
  monkeypatch.setattr(CLI_COMMON_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  clock = _FakeClock()
  monkeypatch.setattr(common, "time", clock)
  monkeypatch.setattr(CLI_COMMON_CONNECT_TOTAL_TIMEOUT_PATCH_TARGET, 2.0)

  call_count = 0

  def fake_post(*args: object, **kwargs: object) -> None:
    nonlocal call_count
    call_count += 1
    raise _connect_refused()

  monkeypatch.setattr(CLI_COMMON_TRANSPORT_POST_PATCH_TARGET, fake_post)

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


def test_connect_never_established_bounded_wall_clock_with_real_clock(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  """Same exhaustion path with the REAL clock (not the fake one): shrinking
  CLI_CONNECT_TOTAL_TIMEOUT to sub-second keeps the actual wall-clock wait
  small and bounded — never anywhere near the real 60 s default."""
  cfg = _cfg(tmp_path)
  monkeypatch.setattr(CLI_COMMON_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  monkeypatch.setattr(CLI_COMMON_CONNECT_TOTAL_TIMEOUT_PATCH_TARGET, 0.3)
  monkeypatch.setattr(CLI_COMMON_TRANSPORT_POST_PATCH_TARGET, lambda *a, **k: (_ for _ in ()).throw(_connect_refused()))

  started = time.monotonic()
  with pytest.raises(SystemExit) as exc_info:
    common.post_internal_api("/api/internal/x", {"a": 1})
  elapsed = time.monotonic() - started

  assert exc_info.value.code == 1
  assert elapsed < 1.0
  assert json.loads(capsys.readouterr().err)["code"] == "server_unavailable"


def test_listener_absent_then_appears_mid_budget_succeeds(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The operationally important case: no listener yet, one appears mid-retry, call succeeds."""
  cfg = _cfg(tmp_path)
  monkeypatch.setattr(CLI_COMMON_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  clock = _FakeClock()
  monkeypatch.setattr(common, "time", clock)
  monkeypatch.setattr(CLI_COMMON_CONNECT_TOTAL_TIMEOUT_PATCH_TARGET, 5.0)

  attempts = 0

  def fake_post(*args: object, **kwargs: object) -> common._CliResponse:
    nonlocal attempts
    attempts += 1
    if attempts < 3:
      raise _connect_refused()
    return common._CliResponse(200, "OK", json.dumps({"ok": True}).encode())

  monkeypatch.setattr(CLI_COMMON_TRANSPORT_POST_PATCH_TARGET, fake_post)

  result = common.post_internal_api("/api/internal/x", {"a": 1})

  assert result == {"ok": True}
  assert attempts == 3
  assert len(clock.sleeps) == 2  # two retries before the listener answered
  assert sum(clock.sleeps) < 5.0


# ---------------------------------------------------------------------------
# Gap 2 — server_error keeps today's exit code and hint
# ---------------------------------------------------------------------------


def _rejection(status_code: int, detail: str) -> MagicMock:
  return make_json_response({"detail": detail}, status_code=status_code)


def test_server_rejection_reports_full_triple_and_hint(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _cfg(tmp_path)
  monkeypatch.setattr(CLI_COMMON_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  monkeypatch.setattr(CLI_COMMON_TRANSPORT_POST_PATCH_TARGET, lambda *a, **k: _rejection(409, "stale version"))
  monkeypatch.setattr(
      CLI_COMMON_MAYBE_VERSION_SKEW_HINT_PATCH_TARGET,
      lambda cfg: "server running abc123, repo at def456 — server restart may be required")

  with pytest.raises(SystemExit) as exc_info:
    common.post_internal_api("/api/internal/x", {"a": 1})

  # Today's behavior: no rejection_exit_codes mapping means exit code 1.
  assert exc_info.value.code == 1
  error = json.loads(capsys.readouterr().err)
  assert error == {
      "error": "stale version",
      "code": "server_error",
      "effect": "none",
      "hint": "server running abc123, repo at def456 — server restart may be required",
  }


def test_server_rejection_exit_code_override_keeps_code_and_effect(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  """schedule-trigger's 422 -> 2 contract: the override changes only the exit code."""
  cfg = _cfg(tmp_path)
  monkeypatch.setattr(CLI_COMMON_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  monkeypatch.setattr(CLI_COMMON_TRANSPORT_POST_PATCH_TARGET, lambda *a, **k: _rejection(422, "no such target"))
  monkeypatch.setattr(CLI_COMMON_MAYBE_VERSION_SKEW_HINT_PATCH_TARGET, lambda cfg: None)

  with pytest.raises(SystemExit) as exc_info:
    common.post_internal_api("/api/internal/x", {"a": 1}, rejection_exit_codes={422: 2})

  assert exc_info.value.code == 2
  error = json.loads(capsys.readouterr().err)
  assert error == {"error": "no such target", "code": "server_error", "effect": "none"}


# ---------------------------------------------------------------------------
# Gap 3(a) — readback determinism for improve, schedule-trigger, and plan
# ---------------------------------------------------------------------------


def test_improve_readback_resolves_to_seeded_loop_on_sent_but_lost(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _patch_readback_env(monkeypatch, tmp_path)

  session_id = "sess-improve"
  goal_text = "Improve the widget end to end"
  loop_dir = cfg.sessions_dir / session_id / "loops" / "3"
  loop_dir.mkdir(parents=True)
  (loop_dir / "goal.md").write_text(goal_text, encoding="utf-8")
  _write_thread(cfg, session_id, "t1", description=f"Goal: {goal_text}", task_type="implement")

  goal_file = tmp_path / "goal.md"
  goal_file.write_text(goal_text, encoding="utf-8")
  repo_dir = tmp_path / "repo"
  repo_dir.mkdir()

  monkeypatch.setattr(
      sys, "argv", [
          "charliebot-improve", "--session", session_id, "--repo",
          str(repo_dir), "--goal-file",
          str(goal_file), "--base-branch", "main"
      ])

  improve_module.main()

  out = json.loads(capsys.readouterr().out)
  assert out["status"] == "started"
  assert out["session_id"] == session_id
  assert out["loop_id"] == 3


def test_improve_readback_reports_outcome_unknown_when_nothing_matches(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  _patch_readback_env(monkeypatch, tmp_path)

  session_id = "sess-improve-miss"
  goal_file = tmp_path / "goal.md"
  goal_file.write_text("A goal nobody launched", encoding="utf-8")
  repo_dir = tmp_path / "repo"
  repo_dir.mkdir()

  monkeypatch.setattr(
      sys, "argv", [
          "charliebot-improve", "--session", session_id, "--repo",
          str(repo_dir), "--goal-file",
          str(goal_file), "--base-branch", "main"
      ])

  with pytest.raises(SystemExit) as exc_info:
    improve_module.main()

  assert exc_info.value.code == 1
  error = json.loads(capsys.readouterr().err)
  assert error["code"] == "outcome_unknown"
  assert error["effect"] == "unknown"


def test_schedule_trigger_readback_resolves_to_seeded_trigger_on_sent_but_lost(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _patch_readback_env(monkeypatch, tmp_path)

  session_id = "sess-trigger"
  write_trigger(
      cfg.sessions_dir / session_id / "triggers" / "trg1.json",
      PendingTrigger(
          id="trg1",
          session_id=session_id,
          message="Check the job",
          fire_at="2024-01-01T00:00:00+00:00",
          created_at="2024-01-01T00:00:00+00:00",
      ))

  monkeypatch.setattr(
      sys, "argv",
      ["charliebot-schedule-trigger", "--session", session_id, "--max-wait", "60", "--message", "Check the job"])

  schedule_trigger_module.main()

  out = json.loads(capsys.readouterr().out)
  assert out == {"trigger_id": "trg1", "fire_at": "2024-01-01T00:00:00Z"}


def test_schedule_trigger_readback_reports_outcome_unknown_when_nothing_matches(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  _patch_readback_env(monkeypatch, tmp_path)

  session_id = "sess-trigger-miss"
  monkeypatch.setattr(
      sys, "argv",
      ["charliebot-schedule-trigger", "--session", session_id, "--max-wait", "60", "--message", "Nothing seeded"])

  with pytest.raises(SystemExit) as exc_info:
    schedule_trigger_module.main()

  assert exc_info.value.code == 1
  error = json.loads(capsys.readouterr().err)
  assert error["code"] == "outcome_unknown"
  assert error["effect"] == "unknown"


def test_schedule_trigger_readback_ignores_fired_trigger_reports_outcome_unknown(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  """A self-renewing watch reuses the identical message on every renewal, so a
  previous leg's fired-but-not-deleted trigger file can match on message +
  watch targets. It must not count as proof the new call landed."""
  cfg = _patch_readback_env(monkeypatch, tmp_path)

  session_id = "sess-trigger-fired-only"
  write_trigger(
      cfg.sessions_dir / session_id / "triggers" / "trg-fired.json",
      PendingTrigger(
          id="trg-fired",
          session_id=session_id,
          message="renew the watch",
          fire_at="2024-01-01T00:00:00+00:00",
          created_at="2024-01-01T00:00:00+00:00",
          status=TriggerStatus.FIRED,
      ))

  monkeypatch.setattr(
      sys, "argv",
      ["charliebot-schedule-trigger", "--session", session_id, "--max-wait", "60", "--message", "renew the watch"])

  with pytest.raises(SystemExit) as exc_info:
    schedule_trigger_module.main()

  assert exc_info.value.code == 1
  error = json.loads(capsys.readouterr().err)
  assert error["code"] == "outcome_unknown"
  assert error["effect"] == "unknown"


def test_schedule_trigger_readback_picks_pending_over_fired_historical_leg(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  """A previous (fired) leg and the newly-armed (pending) leg of the same
  self-renewing watch share the identical message + targets; readback must
  bind to the pending one, never the historical fired file."""
  cfg = _patch_readback_env(monkeypatch, tmp_path)

  session_id = "sess-trigger-fired-plus-pending"
  write_trigger(
      cfg.sessions_dir / session_id / "triggers" / "trg-old.json",
      PendingTrigger(
          id="trg-old",
          session_id=session_id,
          message="renew the watch",
          fire_at="2024-01-01T00:00:00+00:00",
          created_at="2024-01-01T00:00:00+00:00",
          status=TriggerStatus.FIRED,
      ))
  write_trigger(
      cfg.sessions_dir / session_id / "triggers" / "trg-new.json",
      PendingTrigger(
          id="trg-new",
          session_id=session_id,
          message="renew the watch",
          fire_at="2024-02-01T00:00:00+00:00",
          created_at="2024-02-01T00:00:00+00:00",
      ))

  monkeypatch.setattr(
      sys, "argv",
      ["charliebot-schedule-trigger", "--session", session_id, "--max-wait", "60", "--message", "renew the watch"])

  schedule_trigger_module.main()

  out = json.loads(capsys.readouterr().out)
  assert out == {"trigger_id": "trg-new", "fire_at": "2024-02-01T00:00:00Z"}


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


class _StubPlanListener(_StubListener):
  """A sibling of test_master_restart_recovery_e2e.py's _BlackHoleServer: POST is
  accepted then reset (sent-but-lost), GET answers with a crafted plans listing —
  exactly the shape ``plan``'s readback needs (a real GET response, not a mock).
  """

  def __init__(self, plans_payload: dict) -> None:
    payload = plans_payload

    class Handler(_QuietHandler):

      def do_GET(self) -> None:
        body = json.dumps(payload).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

      def do_POST(self) -> None:
        # Black hole: accept, then RST without reading or responding.
        self.connection.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
        self.connection.close()

    super().__init__(Handler)


def test_plan_readback_resolves_to_seeded_plan_on_sent_but_lost(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  plans_payload = {
      "plans":
          [
              {
                  "id": 7,
                  "title": "My Plan",
                  "state": "open",
                  "takeoff": None,
                  "closed": None,
                  "versions": [{
                      "v": 1,
                      "file": "artifacts/plan_01.html"
                  }],
              }
          ]
  }
  stub = _StubPlanListener(plans_payload)
  try:
    cfg = _cfg(tmp_path, server={"port": stub.port})
    monkeypatch.setattr(CLI_COMMON_GET_CONFIG_PATCH_TARGET, lambda: cfg)

    plan_module.main(["present", "--session", "sess-plan", "--file", "artifacts/plan_01.html", "--title", "My Plan"])

    out = json.loads(capsys.readouterr().out)
    assert out == {"plan": 7, "v": 1, "state": "open", "reminder": _PLAN_REMINDER}
  finally:
    stub.close()


def test_plan_readback_reports_outcome_unknown_when_nothing_matches(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  stub = _StubPlanListener({"plans": []})
  try:
    cfg = _cfg(tmp_path, server={"port": stub.port})
    monkeypatch.setattr(CLI_COMMON_GET_CONFIG_PATCH_TARGET, lambda: cfg)

    with pytest.raises(SystemExit) as exc_info:
      plan_module.main(
          ["present", "--session", "sess-plan-miss", "--file", "artifacts/plan_01.html", "--title", "My Plan"])

    assert exc_info.value.code == 1
    error = json.loads(capsys.readouterr().err)
    assert error["code"] == "outcome_unknown"
    assert error["effect"] == "unknown"
  finally:
    stub.close()


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
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  stub = _CapturePostListener()
  try:
    cfg = _cfg(tmp_path, server={"port": stub.port})
    monkeypatch.setattr(CLI_COMMON_GET_CONFIG_PATCH_TARGET, lambda: cfg)

    result = common._request_with_contract(
        "POST", "/api/internal/x", payload={"a": 1}, params={"k": "v"}, unknown_effect="none")

    assert result == {"ok": True}
    assert stub.received["path"] == "/api/internal/x?k=v"
    assert stub.received["content_type"] == "application/json"
    assert stub.received["authorization"] is None  # scratch config carries no access key
    assert stub.received["body"] == {"a": 1}
  finally:
    stub.close()


def test_plain_http_request_never_loads_the_http_client_stack() -> None:
  """The plain-HTTP verb request runs the minimal socket client: http.client (+ssl,
  email.parser inside it, ~16 ms of every verb's wall) must stay out of the process —
  the M97 landing's remaining client stack (docs/perf_baseline.md). A raw-socket stub
  serves the response because http.server's own import would load http.client in the
  probe, defeating the assertion."""
  response = (
      b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n"
      b"Content-Length: 13\r\nConnection: close\r\n\r\n" + b'{"request":1}')

  probe = '''
import json, socket, sys, threading
from src.cli.common import _send_request

server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
server.bind(("127.0.0.1", 0))
server.listen(1)
served = {}

def serve():
    conn, _ = server.accept()
    served["request"] = conn.recv(65536).decode("latin-1")
    conn.sendall(STUB_RESPONSE)
    conn.close()
    server.close()

threading.Thread(target=serve, daemon=True).start()
resp = _send_request("POST", f"http://127.0.0.1:{server.getsockname()[1]}/api/internal/x?a=b",
                     payload={"k": 1}, params=None, headers={}, timeout=5.0)
print(json.dumps({
    "status": resp.status_code,
    "reason": resp._reason,
    "body": json.loads(resp._body),
    "request": served["request"],
    "http_client_loaded": "http.client" in sys.modules,
    "ssl_loaded": "ssl" in sys.modules,
    "email_loaded": "email.parser" in sys.modules,
}))
'''.replace("STUB_RESPONSE", repr(response))
  result = subprocess.run(
      [sys.executable, "-c", probe],
      cwd=ROOT,
      capture_output=True,
      text=True,
      timeout=60,
      check=True,
  )
  out = json.loads(result.stdout)
  assert out["status"] == 200 and out["reason"] == "OK" and out["body"] == {"request": 1}
  request_head, _, request_body = out["request"].partition("\r\n\r\n")
  assert request_head.splitlines()[0] == "POST /api/internal/x?a=b HTTP/1.1"
  assert "Content-Type: application/json" in request_head
  assert "Content-Length:" in request_head
  assert request_body == json.dumps({"k": 1})
  assert out["http_client_loaded"] is False
  assert out["ssl_loaded"] is False
  assert out["email_loaded"] is False


# ---------------------------------------------------------------------------
# Gap 3(b) — readback determinism at the matcher level: concurrent identical
# specs, and a verify thread never satisfying an implement call's readback.
# ---------------------------------------------------------------------------


def test_find_local_thread_concurrent_identical_specs_resolves_to_newest(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Two identical (description, task_type) threads in flight: readback must give a
  definite answer (the newest), not ambiguity — so no second worker gets spawned."""
  cfg = _cfg(tmp_path)
  monkeypatch.setattr(CLI_COMMON_GET_CONFIG_PATCH_TARGET, lambda: cfg)

  session_id = "sess-concurrent"
  _write_thread(
      cfg,
      session_id,
      "older",
      description="do the thing",
      task_type="implement",
      status="running",
      created_at="2024-01-01T00:00:00+00:00")
  _write_thread(
      cfg,
      session_id,
      "newer",
      description="do the thing",
      task_type="implement",
      status="running",
      created_at="2024-01-01T00:05:00+00:00")

  match = common.find_local_thread(session_id, description="do the thing", task_type="implement")

  assert match is not None
  assert match["id"] == "newer"


def test_find_local_thread_verify_and_implement_never_cross_match(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Matching requires description AND task_type: a verify thread must never satisfy
  an implement call's readback, nor the reverse, even with an identical description."""
  cfg = _cfg(tmp_path)
  monkeypatch.setattr(CLI_COMMON_GET_CONFIG_PATCH_TARGET, lambda: cfg)

  session_id = "sess-verify-vs-implement"
  _write_thread(cfg, session_id, "verify-thread", description="check the plan", task_type="verify", status="running")

  # The verify thread must not satisfy an implement call's readback...
  assert common.find_local_thread(session_id, description="check the plan", task_type="implement") is None
  # ...but does satisfy its own verify call.
  match = common.find_local_thread(session_id, description="check the plan", task_type="verify")
  assert match is not None
  assert match["id"] == "verify-thread"

  # Symmetric case: an implement thread must not satisfy a verify call's readback.
  _write_thread(
      cfg, session_id, "implement-thread", description="check the plan 2", task_type="implement", status="running")
  assert common.find_local_thread(session_id, description="check the plan 2", task_type="verify") is None
  match = common.find_local_thread(session_id, description="check the plan 2", task_type="implement")
  assert match is not None
  assert match["id"] == "implement-thread"
