"""CLI restart-crossing contract: bounded connect retry, server rejection, and
readback determinism (plan r11 section 4.1 CLI table + the two readback
scenarios from section 4.2 that nothing else in the tree constructs).

All tests here are in-process: no CLI subprocess, no real CharlieBot server.
Network-shaped tests either mock ``requests.post``/``requests.get`` directly
(gaps 1, 2, and the local-file readbacks) or run a tiny stub HTTP listener on
127.0.0.1 (the `plan` readback, which needs a real GET response) — never a
real port scan, process name, or pgrep.
"""

from __future__ import annotations

import http.server
import json
import socket
import struct
import sys
import threading
import time
from pathlib import Path

import pytest
import requests

from src.cli import common
from src.cli import improve as improve_module
from src.cli import plan as plan_module
from src.cli import schedule_trigger as schedule_trigger_module
from src.core.config import CharlieBotConfig


def _cfg(tmp_path: Path, **overrides) -> CharlieBotConfig:
  return CharlieBotConfig(charliebot_home=tmp_path / "home", **overrides)


def _write_thread(cfg: CharlieBotConfig, session_id: str, thread_id: str, **fields) -> None:
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


def _connect_refused() -> requests.exceptions.ConnectionError:
  return requests.exceptions.ConnectionError(
      "HTTPConnectionPool(host='localhost', port=1): Failed to establish a new connection: "
      "[Errno 111] Connection refused")


def _reset_after_send() -> requests.exceptions.ConnectionError:
  """A connection that completed its handshake before dying: sent-but-lost, never a retry class."""
  return requests.exceptions.ConnectionError(
      "('Connection aborted.', ConnectionResetError(104, 'Connection reset by peer'))")


# ---------------------------------------------------------------------------
# Gap 1 — server_unavailable and the bounded retry
# ---------------------------------------------------------------------------


def test_connect_never_established_retries_with_backoff_then_exhausts(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _cfg(tmp_path)
  monkeypatch.setattr(common, "get_config", lambda: cfg)
  clock = _FakeClock()
  monkeypatch.setattr(common, "time", clock)
  monkeypatch.setattr(common, "CLI_CONNECT_TOTAL_TIMEOUT", 2.0)

  call_count = 0

  def fake_post(*args, **kwargs):
    nonlocal call_count
    call_count += 1
    raise _connect_refused()

  monkeypatch.setattr(common.requests, "post", fake_post)

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
  monkeypatch.setattr(common, "get_config", lambda: cfg)
  monkeypatch.setattr(common, "CLI_CONNECT_TOTAL_TIMEOUT", 0.3)
  monkeypatch.setattr(common.requests, "post", lambda *a, **k: (_ for _ in ()).throw(_connect_refused()))

  started = time.monotonic()
  with pytest.raises(SystemExit) as exc_info:
    common.post_internal_api("/api/internal/x", {"a": 1})
  elapsed = time.monotonic() - started

  assert exc_info.value.code == 1
  assert elapsed < 1.0
  assert json.loads(capsys.readouterr().err)["code"] == "server_unavailable"


def test_listener_absent_then_appears_mid_budget_succeeds(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The operationally important case: no listener yet, one appears mid-retry, call succeeds."""
  cfg = _cfg(tmp_path)
  monkeypatch.setattr(common, "get_config", lambda: cfg)
  clock = _FakeClock()
  monkeypatch.setattr(common, "time", clock)
  monkeypatch.setattr(common, "CLI_CONNECT_TOTAL_TIMEOUT", 5.0)

  attempts = 0

  def fake_post(*args, **kwargs):
    nonlocal attempts
    attempts += 1
    if attempts < 3:
      raise _connect_refused()
    resp = requests.Response()
    resp.status_code = 200
    resp._content = json.dumps({"ok": True}).encode()
    return resp

  monkeypatch.setattr(common.requests, "post", fake_post)

  result = common.post_internal_api("/api/internal/x", {"a": 1})

  assert result == {"ok": True}
  assert attempts == 3
  assert len(clock.sleeps) == 2  # two retries before the listener answered
  assert sum(clock.sleeps) < 5.0


# ---------------------------------------------------------------------------
# Gap 2 — server_error keeps today's exit code and hint
# ---------------------------------------------------------------------------


def _rejection(status_code: int, detail: str) -> requests.exceptions.HTTPError:
  resp = requests.Response()
  resp.status_code = status_code
  resp._content = json.dumps({"detail": detail}).encode()
  return requests.exceptions.HTTPError(response=resp)


def test_server_rejection_reports_full_triple_and_hint(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _cfg(tmp_path)
  monkeypatch.setattr(common, "get_config", lambda: cfg)
  monkeypatch.setattr(common.requests, "post", lambda *a, **k: (_ for _ in ()).throw(_rejection(409, "stale version")))
  monkeypatch.setattr(
      common, "_maybe_version_skew_hint", lambda cfg: "server running abc123, repo at def456 — server restart may be required")

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
  monkeypatch.setattr(common, "get_config", lambda: cfg)
  monkeypatch.setattr(common.requests, "post", lambda *a, **k: (_ for _ in ()).throw(_rejection(422, "no such target")))
  monkeypatch.setattr(common, "_maybe_version_skew_hint", lambda cfg: None)

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
  cfg = _cfg(tmp_path)
  monkeypatch.setattr(common, "get_config", lambda: cfg)
  monkeypatch.setattr(improve_module, "get_config", lambda: cfg)
  monkeypatch.setattr(common.requests, "post", lambda *a, **k: (_ for _ in ()).throw(_reset_after_send()))

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
  cfg = _cfg(tmp_path)
  monkeypatch.setattr(common, "get_config", lambda: cfg)
  monkeypatch.setattr(improve_module, "get_config", lambda: cfg)
  monkeypatch.setattr(common.requests, "post", lambda *a, **k: (_ for _ in ()).throw(_reset_after_send()))

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
  cfg = _cfg(tmp_path)
  monkeypatch.setattr(common, "get_config", lambda: cfg)
  monkeypatch.setattr(schedule_trigger_module, "get_config", lambda: cfg)
  monkeypatch.setattr(common.requests, "post", lambda *a, **k: (_ for _ in ()).throw(_reset_after_send()))

  session_id = "sess-trigger"
  triggers_dir = cfg.sessions_dir / session_id / "triggers"
  triggers_dir.mkdir(parents=True)
  (triggers_dir / "trg1.json").write_text(
      json.dumps({
          "id": "trg1",
          "message": "Check the job",
          "watch_targets": [],
          "fire_at": "2024-01-01T00:00:00+00:00",
      }),
      encoding="utf-8")

  monkeypatch.setattr(
      sys, "argv",
      ["charliebot-schedule-trigger", "--session", session_id, "--max-wait", "60", "--message", "Check the job"])

  schedule_trigger_module.main()

  out = json.loads(capsys.readouterr().out)
  assert out == {"trigger_id": "trg1", "fire_at": "2024-01-01T00:00:00+00:00"}


def test_schedule_trigger_readback_reports_outcome_unknown_when_nothing_matches(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _cfg(tmp_path)
  monkeypatch.setattr(common, "get_config", lambda: cfg)
  monkeypatch.setattr(schedule_trigger_module, "get_config", lambda: cfg)
  monkeypatch.setattr(common.requests, "post", lambda *a, **k: (_ for _ in ()).throw(_reset_after_send()))

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


class _StubPlanListener:
  """A sibling of test_master_restart_recovery_e2e.py's _BlackHoleServer: POST is
  accepted then reset (sent-but-lost), GET answers with a crafted plans listing —
  exactly the shape ``plan``'s readback needs (a real GET response, not a mock).
  """

  def __init__(self, plans_payload: dict) -> None:
    payload = plans_payload

    class Handler(http.server.BaseHTTPRequestHandler):

      def do_GET(self) -> None:  # noqa: N802 (http.server naming)
        body = json.dumps(payload).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

      def do_POST(self) -> None:  # noqa: N802
        # Black hole: accept, then RST without reading or responding.
        self.connection.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
        self.connection.close()

      def log_message(self, format: str, *args) -> None:  # noqa: A002
        pass

    self._httpd = http.server.HTTPServer(("127.0.0.1", 0), Handler)
    self.port = self._httpd.server_address[1]
    self._thread = threading.Thread(target=self._httpd.serve_forever, daemon=True)
    self._thread.start()

  def close(self) -> None:
    self._httpd.shutdown()
    self._httpd.server_close()


def test_plan_readback_resolves_to_seeded_plan_on_sent_but_lost(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  plans_payload = {
      "plans": [{
          "id": 7,
          "title": "My Plan",
          "state": "open",
          "takeoff": None,
          "closed": None,
          "versions": [{
              "v": 1,
              "file": "artifacts/plan_01.html"
          }],
      }]
  }
  stub = _StubPlanListener(plans_payload)
  try:
    cfg = _cfg(tmp_path, server_port=stub.port)
    monkeypatch.setattr(common, "get_config", lambda: cfg)

    plan_module.main(["present", "--session", "sess-plan", "--file", "artifacts/plan_01.html", "--title", "My Plan"])

    out = json.loads(capsys.readouterr().out)
    assert out == {"plan": 7, "v": 1, "state": "open"}
  finally:
    stub.close()


def test_plan_readback_reports_outcome_unknown_when_nothing_matches(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  stub = _StubPlanListener({"plans": []})
  try:
    cfg = _cfg(tmp_path, server_port=stub.port)
    monkeypatch.setattr(common, "get_config", lambda: cfg)

    with pytest.raises(SystemExit) as exc_info:
      plan_module.main(["present", "--session", "sess-plan-miss", "--file", "artifacts/plan_01.html", "--title", "My Plan"])

    assert exc_info.value.code == 1
    error = json.loads(capsys.readouterr().err)
    assert error["code"] == "outcome_unknown"
    assert error["effect"] == "unknown"
  finally:
    stub.close()


# ---------------------------------------------------------------------------
# Gap 3(b) — readback determinism at the matcher level: concurrent identical
# specs, and a verify thread never satisfying an implement call's readback.
# ---------------------------------------------------------------------------


def test_find_local_thread_concurrent_identical_specs_resolves_to_newest(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Two identical (description, task_type) threads in flight: readback must give a
  definite answer (the newest), not ambiguity — so no second worker gets spawned."""
  cfg = _cfg(tmp_path)
  monkeypatch.setattr(common, "get_config", lambda: cfg)

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
  monkeypatch.setattr(common, "get_config", lambda: cfg)

  session_id = "sess-verify-vs-implement"
  _write_thread(
      cfg, session_id, "verify-thread", description="check the plan", task_type="verify", status="running")

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
