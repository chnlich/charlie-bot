"""Master-side end-to-end restart recovery: re-attach, replay, queue drain, read-back.

Two-process A/B protocol (mirrors test_restart_recovery_e2e.py):
  A is a short-lived driver subprocess that runs a real master turn through
  ``master_cc.run_message`` against a fake `claude` shim (claude-shaped NDJSON,
  prompt on stdin) and is then SIGKILLed — exactly like a crashed server.
  B is this test process: a fresh CharlieBotConfig at the same CHARLIEBOT_HOME
  runs startup crash recovery against the truth on disk.

Scenarios:
  - re-attach: the server dies, the agent keeps running. Recovery follows the
    recorded turn's raw log to its end; no second spawn, no replay.
  - dead turn: server and agent die together. Recovery replays the user
    message with the replay marker.
  - queued: A running + B queued when the server dies. Recovery re-attaches A
    (excluded from replay) and replays B AFTER A drains.
  - idempotency: a replayed delegate call re-crossing the restart hits the
    CLI read-back contract and lands on the already-created thread.
"""

from __future__ import annotations

import asyncio
import json
import os
import select
import signal
import socket
import struct
import subprocess
import sys
import threading
import time
from datetime import datetime, timedelta, timezone
from pathlib import Path

import pytest

from src.agents import master_cc
from src.core import init as init_module
from src.core import runs
from src.core.config import CharlieBotConfig
from src.core.models import BackendOption
from src.core.process import kill_process_group

REPO_ROOT = Path(__file__).resolve().parents[1]

FAKE_SHIM = r"""#!/bin/sh
# Fake `claude`: records argv + stdin prompt, emits claude-shaped NDJSON under
# SHIM_MODE control. Invocation counters live under $SHIM_STATE/inv-<n>.*.
mode="$SHIM_MODE"
state="$SHIM_STATE"
mkdir -p "$state"
n=1
while [ -e "$state/inv-$n.argv" ]; do
  n=$((n + 1))
done
printf '%s\n' "$@" > "$state/inv-$n.argv"
cat > "$state/inv-$n.prompt"
echo "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"ASSISTANT-INV-$n\"}]}}"
case "$mode" in
  hang)
    while :; do sleep 60; done
    ;;
  sleep_first)
    sleep "$SHIM_SLEEP"
    ;;
esac
if [ "$mode" = "delegate" ]; then
  python -m src.cli.delegate --session "$SHIM_DELEGATE_SESSION" --repo "$SHIM_DELEGATE_REPO" \
      --task-spec-file "$SHIM_DELEGATE_SPEC" --base-branch main --keep-worktree 0 \
      > "$state/delegate.stdout" 2> "$state/delegate.stderr"
  echo "$?" > "$state/delegate.rc"
fi
echo "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"RESULT-INV-$n\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}"
exit 0
"""

DRIVER = """import asyncio
import json
import sys
from pathlib import Path

from src.agents import master_cc
from src.core.config import CharlieBotConfig
from src.core.models import BackendOption, CreateSessionRequest, TaskType, ThreadStatus
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager


async def main() -> None:
  home = Path(sys.argv[1])
  shim = sys.argv[2]
  kind = sys.argv[3]
  cfg = CharlieBotConfig(
      charliebot_home=home,
      worktree_dir=str(home / "worktrees"),
      backend_options=[
          BackendOption(id="fake", label="Fake", type="cc-claude", model="fake-model", cli_binary=shim)],
  )
  # Prompt assembly is orthogonal to this protocol; keep the turn minimal.
  master_cc._build_instructions_content = lambda session_meta, cfg: "instructions"
  session_mgr = SessionManager(cfg)
  thread_mgr = ThreadManager(cfg)
  meta = await session_mgr.create_session(CreateSessionRequest(name="master-e2e"))

  if kind == "delegate":
    # Reconstruct "turn 1 delegated and the effect landed, then the response
    # was lost": the worker thread for this exact spec already exists on disk,
    # terminally, with its finalize effects present.
    spec = Path(sys.argv[4]).read_text(encoding="utf-8")
    thread = await thread_mgr.create_thread(meta, spec, task_type=TaskType.IMPLEMENT)
    await thread_mgr.update_status(meta.id, thread.id, ThreadStatus.FAILED)
    await session_mgr.save_chat_event(
        meta.id,
        {"type": "worker_summary", "thread_id": thread.id, "status": "failed", "content": "worker failed"})
    await session_mgr.save_chat_event(
        meta.id,
        {"type": "assistant",
         "message": {"role": "assistant", "content": [{"type": "text", "text": "prior master output"}]}})

  callbacks = session_mgr.callbacks()
  task_a = asyncio.create_task(master_cc.run_message(cfg, meta, "message A", callbacks))
  while not any(e.get("content") == "message A" for e in session_mgr.load_chat_events_sync(meta.id)):
    await asyncio.sleep(0.01)
  extra_tasks = []
  if kind == "queued":
    # Strict A-before-B enqueue ordering: A's user event is already on disk.
    task_b = asyncio.create_task(master_cc.run_message(cfg, meta, "message B", callbacks))
    extra_tasks.append(task_b)
    while not any(e.get("content") == "message B" for e in session_mgr.load_chat_events_sync(meta.id)):
      await asyncio.sleep(0.01)

  # Handshake for the harness: user event(s) (and any seed) are durable.
  (home / "driver_ids.json").write_text(json.dumps({"session": meta.id}))
  await asyncio.gather(task_a, *extra_tasks)  # never returns in killed scenarios


asyncio.run(main())
"""

SPEC_TEXT = """## Goal
Implement the widget.

## Source Files
- (none)

## Required Behavior
The widget renders.

## Acceptance Tests
Look at the widget.

## Reviewer Checklist
The widget exists.

## Out of Scope
Everything else.
"""


def _cfg(home: Path, shim: Path) -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=home,
      worktree_dir=str(home / "worktrees"),
      backend_options=[
          BackendOption(id="fake", label="Fake", type="cc-claude", model="fake-model", cli_binary=str(shim))],
  )


def _wait_for(predicate, timeout: float, what: str) -> None:
  deadline = time.monotonic() + timeout
  while time.monotonic() < deadline:
    if predicate():
      return
    time.sleep(0.05)
  raise TimeoutError(what)


def _session_meta(home: Path, session_id: str) -> dict:
  return json.loads((home / "sessions" / session_id / "metadata.json").read_text(encoding="utf-8"))


def _chat_events(home: Path, session_id: str) -> list[dict]:
  path = home / "sessions" / session_id / "data" / "chat_events.jsonl"
  if not path.exists():
    return []
  return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]


def _raw_logs(home: Path, session_id: str) -> list[Path]:
  runs_dir = home / "sessions" / session_id / "data" / "master_runs"
  if not runs_dir.is_dir():
    return []
  return sorted(runs_dir.glob(f"*/{runs.RAW_LOG_NAME}"))


def _shim_prompt(state: Path, n: int) -> str:
  return (state / f"inv-{n}.prompt").read_text(encoding="utf-8")


async def _await_recovery_tasks() -> None:
  prefixes = ("resume-", "respawn-", "recomplete-", "master-resume-", "master-replay-", "master-consumer-")
  current = asyncio.current_task()
  while True:
    pending = [
        t for t in asyncio.all_tasks()
        if t is not current and not t.done() and t.get_name().startswith(prefixes)
    ]
    if not pending:
      return
    await asyncio.gather(*pending)


def _install_shim(tmp_path: Path) -> tuple[Path, Path]:
  shim_dir = tmp_path / "shim"
  shim_dir.mkdir()
  shim = shim_dir / "claude"
  shim.write_text(FAKE_SHIM, encoding="utf-8")
  shim.chmod(0o755)
  state = tmp_path / "shim_state"
  state.mkdir()
  return shim, state


def _launch_driver(tmp_path: Path, home: Path, shim: Path, kind: str, shim_mode: str,
                   extra_args: list[str] | None = None, extra_env: dict[str, str] | None = None) -> tuple[subprocess.Popen, str]:
  shim_dir = tmp_path / "shim"
  driver = shim_dir / "driver.py"
  driver.write_text(DRIVER, encoding="utf-8")
  home.mkdir(exist_ok=True)
  env = dict(os.environ)
  env["PYTHONPATH"] = str(REPO_ROOT)
  env["SHIM_MODE"] = shim_mode
  env["SHIM_STATE"] = str(tmp_path / "shim_state")
  env["SHIM_SLEEP"] = "3"
  if extra_env:
    env.update(extra_env)
  proc = subprocess.Popen(
      [sys.executable, str(driver), str(home), str(shim), kind, *(extra_args or [])],
      stdout=subprocess.DEVNULL,
      stderr=subprocess.DEVNULL,
      env=env,
  )
  ids_file = home / "driver_ids.json"
  _wait_for(ids_file.exists, timeout=20.0, what="driver did not create session / persist user events")
  return proc, json.loads(ids_file.read_text(encoding="utf-8"))["session"]


def _master_pid(home: Path, session_id: str) -> int:
  """The recorded master agent pid, or fail the wait if no record exists yet."""
  record = _session_meta(home, session_id)["master_run"]
  assert record is not None and record["pid"] is not None
  return record["pid"]


def _kill_agent_only(home: Path, session_id: str) -> None:
  """SIGKILL the recorded master agent's process group (it runs its own session)."""
  pid = _master_pid(home, session_id)
  kill_process_group(pid, signal.SIGKILL)


class _BlackHoleServer:
  """Accept-then-RST server: the request is sent, the response is lost.

  Deterministically produces the sent-but-lost failure class for the CLI
  contract (never a connect failure: the TCP handshake always completes).
  """

  def __init__(self) -> None:
    self._sock = socket.socket()
    self._sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    self._sock.bind(("127.0.0.1", 0))
    self._sock.listen()
    self.port = self._sock.getsockname()[1]
    self._stop = threading.Event()
    self._thread = threading.Thread(target=self._pump, daemon=True)
    self._thread.start()

  def _pump(self) -> None:
    while not self._stop.is_set():
      ready, _, _ = select.select([self._sock], [], [], 0.2)
      if not ready:
        continue
      try:
        conn, _ = self._sock.accept()
      except OSError:
        continue
      try:
        conn.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
        conn.close()
      except OSError:
        pass

  def close(self) -> None:
    self._stop.set()
    self._sock.close()


@pytest.mark.asyncio
async def test_master_reattach_after_server_kill(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Server died; agent kept running: re-attach, no second spawn, no replay."""
  home = tmp_path / "home"
  shim, state = _install_shim(tmp_path)
  proc, session_id = _launch_driver(tmp_path, home, shim, "chat", "sleep_first")

  # Wait until the turn is recorded and its first output is durable.
  def turn_started() -> bool:
    raw = _raw_logs(home, session_id)
    record = _session_meta(home, session_id)["master_run"]
    return bool(raw) and record is not None and "ASSISTANT-INV-1" in raw[0].read_text(encoding="utf-8",
                                                                                      errors="replace")

  _wait_for(turn_started, timeout=20.0, what="master turn did not start/persist identity")
  proc.kill()
  proc.wait(timeout=10)

  # Backdate the persisted turn's recorded start 600s, so a correct re-attach
  # reports a thinking interval beginning before the restart's recovery window.
  # The mechanism under test—taking the interval start from the record—must
  # surface that 600s on the MASTER_DONE thinking_seconds; a restart-fresh
  # interval would report ~0s.
  meta_path = home / "sessions" / session_id / "metadata.json"
  meta = json.loads(meta_path.read_text(encoding="utf-8"))
  rec = meta["master_run"]
  assert rec is not None
  rec_started = datetime.fromisoformat(rec["started_at"].replace("Z", "+00:00"))
  backdated = rec_started - timedelta(seconds=600)
  rec["started_at"] = backdated.isoformat()
  meta_path.write_text(json.dumps(meta), encoding="utf-8")
  # Keep the run "live": is_run_alive requires started_at to postdate the most
  # recent host boot (nothing survives a reboot), and _reconcile_master_runs
  # funnels this same value into the liveness closure it hands the re-attach.
  monkeypatch.setattr(init_module.runs, "read_host_boot_time",
                      lambda: backdated - timedelta(hours=1))

  monkeypatch.setattr(master_cc, "_build_instructions_content", lambda session_meta, cfg: "instructions")
  monkeypatch.setenv("SHIM_MODE", "immediate")  # any hypothetical respawn exits fast
  monkeypatch.setenv("SHIM_STATE", str(state))
  await init_module.run_crash_recovery(_cfg(home, shim), datetime.now(timezone.utc))
  await _await_recovery_tasks()

  # The decisive re-attach proof: exactly one shim invocation ever happened —
  # the turn was followed, never respawned nor replayed.
  assert not (state / "inv-2.argv").exists(), "a second master process was spawned"
  assert "message A" in _shim_prompt(state, 1)
  events = _chat_events(home, session_id)
  assert sum(1 for e in events if e.get("type") == "master_done") == 1
  user_events = [e for e in events if e.get("type") == "user"]
  assert len(user_events) == 1
  assert _session_meta(home, session_id)["master_run"] is None
  # The cursor drained exactly to the end of the raw log.
  raw = _raw_logs(home, session_id)[0]
  cursor = raw.parent / runs.CURSOR_NAME
  assert runs.read_raw_cursor(cursor) == raw.stat().st_size

  # The re-attached turn's MASTER_DONE counts the whole interval — including
  # the backdated 600s before the restart — because the interval start came
  # from the persisted record, not from the restart's enqueue.
  master_done = [e for e in events if e.get("type") == "master_done"]
  assert len(master_done) == 1
  assert master_done[0].get("thinking_seconds", 0) >= 600, (
      f"interval must count the backdated start; got {master_done[0].get('thinking_seconds')}s")


@pytest.mark.asyncio
async def test_master_replay_when_master_killed_with_server(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Server and agent died together: the message is replayed with the marker."""
  home = tmp_path / "home"
  shim, state = _install_shim(tmp_path)
  proc, session_id = _launch_driver(tmp_path, home, shim, "chat", "hang")
  _wait_for(
      lambda: _session_meta(home, session_id)["master_run"] is not None,
      timeout=20.0,
      what="master turn identity was never recorded")
  proc.kill()
  proc.wait(timeout=10)
  _kill_agent_only(home, session_id)

  monkeypatch.setattr(master_cc, "_build_instructions_content", lambda session_meta, cfg: "instructions")
  monkeypatch.setenv("SHIM_MODE", "immediate")
  monkeypatch.setenv("SHIM_STATE", str(state))
  await init_module.run_crash_recovery(_cfg(home, shim), datetime.now(timezone.utc))
  await _await_recovery_tasks()

  # The replay spawned exactly one new agent, with the replay marker + the
  # original content, and the original user event was not rewritten.
  assert (state / "inv-2.argv").exists(), "replayed turn never spawned"
  replayed_prompt = _shim_prompt(state, 2)
  assert replayed_prompt.startswith(master_cc._REPLAY_MARKER)
  assert "message A" in replayed_prompt
  events = _chat_events(home, session_id)
  assert len([e for e in events if e.get("type") == "user"]) == 1
  assert sum(1 for e in events if e.get("type") == "master_done") == 1
  assert _session_meta(home, session_id)["master_run"] is None


@pytest.mark.asyncio
async def test_queued_message_answered_after_restart(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A running + B queued at kill: A is re-attached (not replayed), B is
  replayed with the marker and answered only after A drains."""
  home = tmp_path / "home"
  shim, state = _install_shim(tmp_path)
  proc, session_id = _launch_driver(tmp_path, home, shim, "queued", "sleep_first")
  _wait_for(
      lambda: _session_meta(home, session_id)["master_run"] is not None
      and any("ASSISTANT-INV-1" in r.read_text(encoding="utf-8", errors="replace") for r in _raw_logs(home, session_id)),
      timeout=20.0,
      what="turn A did not start/persist identity and first output")
  proc.kill()
  proc.wait(timeout=10)

  monkeypatch.setattr(master_cc, "_build_instructions_content", lambda session_meta, cfg: "instructions")
  monkeypatch.setenv("SHIM_MODE", "immediate")
  monkeypatch.setenv("SHIM_STATE", str(state))
  await init_module.run_crash_recovery(_cfg(home, shim), datetime.now(timezone.utc))
  await _await_recovery_tasks()

  # A: re-attached, prompt unmarked. B: one new spawn, marked, only after A.
  assert "message A" in _shim_prompt(state, 1)
  assert not _shim_prompt(state, 1).startswith(master_cc._REPLAY_MARKER)
  assert (state / "inv-2.argv").exists(), "queued message B was never replayed"
  replayed_prompt = _shim_prompt(state, 2)
  assert replayed_prompt.startswith(master_cc._REPLAY_MARKER)
  assert "message B" in replayed_prompt

  events = _chat_events(home, session_id)
  assert len([e for e in events if e.get("type") == "user"]) == 2
  assert sum(1 for e in events if e.get("type") == "master_done") == 2

  def text_of(ev: dict) -> str:
    msg = ev.get("message", {})
    return "".join(b.get("text", "") for b in msg.get("content", []) if isinstance(b, dict))

  idx_a = [i for i, e in enumerate(events) if "ASSISTANT-INV-1" in text_of(e)]
  idx_b = [i for i, e in enumerate(events) if "ASSISTANT-INV-2" in text_of(e)]
  assert idx_a and idx_b, "both turns must have persisted assistant output"
  assert max(idx_a) < min(idx_b), "queued message B was answered before A drained"
  assert _session_meta(home, session_id)["master_run"] is None


@pytest.mark.asyncio
async def test_replayed_delegate_readback_lands_on_existing_thread(tmp_path: Path,
                                                                   monkeypatch: pytest.MonkeyPatch) -> None:
  """A replayed turn re-issues the same `delegate` call across the restart;
  the sent-but-lost CLI read-back deterministically resolves to the thread the
  first attempt already created (thread count stays 1)."""
  home = tmp_path / "home"
  shim, state = _install_shim(tmp_path)
  spec_file = tmp_path / "task_spec.md"
  spec_file.write_text(SPEC_TEXT, encoding="utf-8")
  proc, session_id = _launch_driver(
      tmp_path, home, shim, "delegate", "hang", extra_args=[str(spec_file)])
  _wait_for(
      lambda: _session_meta(home, session_id)["master_run"] is not None,
      timeout=20.0,
      what="master turn identity was never recorded")
  proc.kill()
  proc.wait(timeout=10)
  _kill_agent_only(home, session_id)

  # The replayed delegate call targets a black-hole listener: the request is
  # accepted then the connection is reset — the sent-but-lost class.
  black_hole = _BlackHoleServer()
  try:
    (home / "config.yaml").write_text(f"server_port: {black_hole.port}\n", encoding="utf-8")

    monkeypatch.setattr(master_cc, "_build_instructions_content", lambda session_meta, cfg: "instructions")
    monkeypatch.setenv("SHIM_MODE", "delegate")
    monkeypatch.setenv("SHIM_STATE", str(state))
    monkeypatch.setenv("SHIM_DELEGATE_SESSION", session_id)
    monkeypatch.setenv("SHIM_DELEGATE_REPO", str(REPO_ROOT))
    monkeypatch.setenv("SHIM_DELEGATE_SPEC", str(spec_file))
    monkeypatch.setenv("CHARLIEBOT_HOME", str(home))
    monkeypatch.setenv("PYTHONPATH", str(REPO_ROOT))
    await init_module.run_crash_recovery(_cfg(home, shim), datetime.now(timezone.utc))
    await _await_recovery_tasks()
  finally:
    black_hole.close()

  # The replayed turn carried the marker and completed.
  assert (state / "inv-2.argv").exists(), "replayed turn never spawned"
  assert _shim_prompt(state, 2).startswith(master_cc._REPLAY_MARKER)
  assert (state / "delegate.rc").read_text(encoding="utf-8").strip() == "0", (
      f"delegate CLI failed: {(state / 'delegate.stderr').read_text(encoding='utf-8', errors='replace')}")

  # Read-back determinism: the call resolved to the seeded thread, and no
  # second thread for the same spec exists.
  delegate_out = json.loads((state / "delegate.stdout").read_text(encoding="utf-8"))
  threads_dir = home / "sessions" / session_id / "threads"
  thread_metas = [
      json.loads((p / "metadata.json").read_text(encoding="utf-8"))
      for p in threads_dir.iterdir() if (p / "metadata.json").exists()
  ]
  matching = [m for m in thread_metas if m.get("description") == SPEC_TEXT]
  assert len(matching) == 1, "the replayed delegate spawned a duplicate worker thread"
  assert delegate_out["thread_id"] == matching[0]["id"]
  assert _session_meta(home, session_id)["master_run"] is None
