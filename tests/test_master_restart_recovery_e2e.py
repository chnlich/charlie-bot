"""Master-side end-to-end restart recovery: re-attach, drain, replay, queue drain, read-back.

Two-process A/B protocol (mirrors test_restart_recovery_e2e.py):
  A is a short-lived driver subprocess that runs a real master turn through
  ``master_cc.run_message`` against a fake `claude` shim (claude-shaped NDJSON,
  prompt on stdin) and is then SIGKILLed — exactly like a crashed server.
  B is this test process: a fresh CharlieBotConfig at the same CHARLIEBOT_HOME
  runs startup crash recovery against the truth on disk.

Scenarios:
  - re-attach: the server dies, the agent keeps running. Recovery follows the
    recorded turn's raw log to its end; no second spawn, no replay.
  - completed: the agent FINISHED while the server was dead. Recovery drains
    the bytes after the cursor through the follower and closes the round
    exactly once — no spawn, no replay, cursor at file size.
  - delegate wake variants: a turn with no user event (user_event_id=None).
    A completed one drains like any other; one killed mid-run drains as a
    failure (no user message exists that pass 2 could replay).
  - stalled: alive but silent beyond the report threshold. Recovery
    re-attaches exactly like the RUNNING row, without the worker side's
    chat report.
  - dead turn: server and agent die together. Recovery replays the user
    message with the replay marker.
  - undrainable rows: raw log missing (legacy transport), never started, and
    uncovered transports clear the record; a turn with a user message is then
    answered by the replay pass, never by a drain.
  - graceful teardown: a real event-loop shutdown mid-turn detaches the
    covered child instead of killing it; the next boot re-attaches and the
    round is answered exactly once.
  - queued: A running + B queued when the server dies. Recovery re-attaches A
    (excluded from replay) and replays B AFTER A drains.
  - idempotency: a replayed delegate call re-crossing the restart hits the
    CLI read-back contract and lands on the already-created thread.
"""

from __future__ import annotations

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
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest
from conftest import (
    MASTER_RECOVERY_TASK_PREFIXES,
    await_recovery_tasks,
    patch_instructions_content,
    read_chat_events,
)
from structlog.testing import capture_logs
from test_restart_recovery_e2e import _wait_for

from src.agents import master_cc, master_cc_queue
from src.core import init as init_module
from src.core import runs
from src.core.config import CharlieBotConfig
from src.core.message_aggregator import MessageAggregator
from src.core.models import BackendOption, CreateSessionRequest, MasterRunRecord
from src.core.process import kill_process_group
from src.core.sessions import SessionManager
from src.core.timeouts import NO_OUTPUT_REPORT_THRESHOLD

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

from src.agents import master_cc, master_cc_run
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
          BackendOption(id="fake", label="Fake", type="cc-claude", model="fake-model", cli_binary=shim, prompt_overlay="none")],
  )
  # Prompt assembly is orthogonal to this protocol; keep the turn minimal.
  master_cc_run._build_instructions_content = lambda session_meta, cfg, prompt_overlay: "instructions"
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
  # kind == "wake": a turn with no user event in the chat log (delegate /
  # cron / improve wake): the record's user_event_id stays None, so the
  # replay pass has nothing to redeliver for it.
  skip_user_event = kind == "wake"
  task_a = asyncio.create_task(
      master_cc.run_message(cfg, meta, "message A", callbacks, skip_user_event=skip_user_event))
  if not skip_user_event:
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

# A second driver protocol (mirrors the worker side's GRACEFUL_DRIVER): a real
# child process runs asyncio.run(...), starts a real master turn, then cancels
# the consumer task and exits 0 — exactly what a closing event loop does. What
# the change avoids is the transport's default kill when the loop closes its
# handle, so only a real loop teardown proves it.
GRACEFUL_DRIVER = """import asyncio
import contextlib
import json
import sys
from pathlib import Path

from src.agents import master_cc, master_cc_run, master_cc_state
from src.core.config import CharlieBotConfig
from src.core.models import BackendOption, CreateSessionRequest
from src.core.sessions import SessionManager


async def main() -> None:
  home = Path(sys.argv[1])
  shim = sys.argv[2]
  cfg = CharlieBotConfig(
      charliebot_home=home,
      worktree_dir=str(home / "worktrees"),
      backend_options=[
          BackendOption(id="fake", label="Fake", type="cc-claude", model="fake-model", cli_binary=shim, prompt_overlay="none")],
  )
  # Prompt assembly is orthogonal to this protocol; keep the turn minimal.
  master_cc_run._build_instructions_content = lambda session_meta, cfg, prompt_overlay: "instructions"
  session_mgr = SessionManager(cfg)
  meta = await session_mgr.create_session(CreateSessionRequest(name="master-graceful"))
  (home / "driver_ids.json").write_text(json.dumps({"session": meta.id}))

  callbacks = session_mgr.callbacks()
  turn = asyncio.create_task(master_cc.run_message(cfg, meta, "message A", callbacks))

  # The mid-run state a graceful shutdown interrupts. Waiting for assistant
  # output durable in CHAT (then a beat) guarantees the read cursor already
  # advanced past it, so the cancel can land nowhere near the
  # persist/cursor window — never a forfeited or duplicated marker.
  chat_path = home / "sessions" / meta.id / "data" / "chat_events.jsonl"
  while not (chat_path.exists()
             and "ASSISTANT-INV-1" in chat_path.read_text(encoding="utf-8", errors="replace")):
    await asyncio.sleep(0.05)
  await asyncio.sleep(0.2)

  # Graceful shutdown: exactly what the closing event loop does to the
  # consumer task that owns the turn.
  consumer = master_cc_state._session_consumers[meta.id]
  consumer.cancel()
  with contextlib.suppress(asyncio.CancelledError):
    await consumer
  turn.cancel()
  with contextlib.suppress(asyncio.CancelledError):
    await turn
  (home / "driver_done.json").write_text("{}")


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
          BackendOption(
              id="fake",
              label="Fake",
              type="cc-claude",
              model="fake-model",
              cli_binary=str(shim),
              prompt_overlay="none")
      ],
  )


def _session_meta(home: Path, session_id: str) -> dict:
  return json.loads((home / "sessions" / session_id / "metadata.json").read_text(encoding="utf-8"))


def _raw_logs(home: Path, session_id: str) -> list[Path]:
  runs_dir = home / "sessions" / session_id / "data" / "master_runs"
  if not runs_dir.is_dir():
    return []
  return sorted(runs_dir.glob(f"*/{runs.RAW_LOG_NAME}"))


def _shim_prompt(state: Path, n: int) -> str:
  return (state / f"inv-{n}.prompt").read_text(encoding="utf-8")


async def _await_recovery_tasks() -> None:
  await await_recovery_tasks(MASTER_RECOVERY_TASK_PREFIXES)


def _install_shim(tmp_path: Path) -> tuple[Path, Path]:
  shim_dir = tmp_path / "shim"
  shim_dir.mkdir()
  shim = shim_dir / "claude"
  shim.write_text(FAKE_SHIM, encoding="utf-8")
  shim.chmod(0o755)
  state = tmp_path / "shim_state"
  state.mkdir()
  return shim, state


def _launch_driver(
    tmp_path: Path,
    home: Path,
    shim: Path,
    kind: str,
    shim_mode: str,
    extra_args: list[str] | None = None,
    extra_env: dict[str, str] | None = None) -> tuple[subprocess.Popen, str]:
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
      [sys.executable, str(driver), str(home),
       str(shim), kind, *(extra_args or [])],
      stdout=subprocess.DEVNULL,
      stderr=subprocess.DEVNULL,
      env=env,
  )
  ids_file = home / "driver_ids.json"
  _wait_for(ids_file.exists, timeout=20.0, what="driver did not create session / persist user events")
  return proc, json.loads(ids_file.read_text(encoding="utf-8"))["session"]


def _launch_master_graceful_driver(tmp_path: Path, home: Path, shim: Path) -> tuple[subprocess.Popen, str]:
  """Run the graceful driver to completion: start a master turn, cancel the
  consumer like a closing event loop, exit 0."""
  driver = tmp_path / "shim" / "graceful_driver.py"
  driver.write_text(GRACEFUL_DRIVER, encoding="utf-8")
  home.mkdir(exist_ok=True)
  env = dict(os.environ)
  env["PYTHONPATH"] = str(REPO_ROOT)
  env["SHIM_MODE"] = "sleep_first"
  env["SHIM_STATE"] = str(tmp_path / "shim_state")
  env["SHIM_SLEEP"] = "5"
  proc = subprocess.Popen(
      [sys.executable, str(driver), str(home), str(shim)],
      stdout=subprocess.DEVNULL,
      stderr=subprocess.DEVNULL,
      env=env,
  )
  done = home / "driver_done.json"
  _wait_for(done.exists, timeout=30.0, what="graceful master driver never finished cancelling")
  proc.wait(timeout=10)
  ids = json.loads((home / "driver_ids.json").read_text(encoding="utf-8"))
  return proc, ids["session"]


def _master_pid(home: Path, session_id: str) -> int:
  """The recorded master agent pid, or fail the wait if no record exists yet."""
  record = _session_meta(home, session_id)["master_run"]
  assert record is not None and record["pid"] is not None
  return record["pid"]


def _kill_agent_only(home: Path, session_id: str) -> None:
  """SIGKILL the recorded master agent's process group (it runs its own session)."""
  pid = _master_pid(home, session_id)
  kill_process_group(pid, signal.SIGKILL)


def _turn_finished_on_disk(home: Path, session_id: str, marker: str) -> bool:
  """Producer exited with its trailing bytes durable and the record still set.

  The recovery-relevant state for a COMPLETED row: the raw log carries
  ``marker``, and the recorded (pid, pid_start, started_at) identity is dead.
  """
  raws = _raw_logs(home, session_id)
  if not raws or marker not in raws[0].read_text(encoding="utf-8", errors="replace"):
    return False
  record = _session_meta(home, session_id)["master_run"]
  if record is None:
    return False
  started_at = datetime.fromisoformat(record["started_at"])
  return not runs.is_run_alive(record["pid"], record["pid_start"], started_at, runs.read_host_boot_time())


def _round_transported_events(events: list[dict], *, skip_user_event: bool) -> list[dict]:
  """The round's raw-log-transported events, stripped of server-injected keys.

  The boundaries of "the round" are its user event (when one exists) and its
  MASTER_DONE — the transported events are everything the agent's raw stream
  contributed between them.
  """
  done_idx = next(i for i, e in enumerate(events) if e.get("type") == "master_done")
  start = 0
  if skip_user_event:
    start = next(i for i, e in enumerate(events) if e.get("type") == "user") + 1
  return [{k: v for k, v in e.items() if k not in ("id", "timestamp", "event_index")} for e in events[start:done_idx]]


def _full_projection(home: Path, session_id: str, cfg: CharlieBotConfig) -> list[dict]:
  """Project the recorded turn's whole raw log from offset 0, fresh translate."""
  raw = _raw_logs(home, session_id)[0]
  option = cfg.get_backend_option(_session_meta(home, session_id)["backend"])
  return runs.project_raw_events(runs.parse_raw_lines(raw.read_bytes()), master_cc._build_fresh_translate(cfg, option))


def _assert_round_operable(events: list[dict]) -> None:
  """The round closes with a separator whose event_index is present — the
  render condition for Clone to here / Elon-e / Recap."""
  messages = [d["message"] for d in MessageAggregator().feed_all(events) if d.get("type") == "message"]
  separators = [m for m in messages if m.get("role") == "separator"]
  assert separators, "no separator row projected for the recovered round"
  assert all(m.get("event_index") is not None for m in separators)


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
    return bool(raw) and record is not None and "ASSISTANT-INV-1" in raw[0].read_text(
        encoding="utf-8", errors="replace")

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
  rec_started = datetime.fromisoformat(rec["started_at"])
  backdated = rec_started - timedelta(seconds=600)
  rec["started_at"] = backdated.isoformat()
  meta_path.write_text(json.dumps(meta), encoding="utf-8")
  # Keep the run "live": is_run_alive requires started_at to postdate the most
  # recent host boot (nothing survives a reboot), and _reconcile_master_runs
  # funnels this same value into the liveness closure it hands the re-attach.
  monkeypatch.setattr(runs, "read_host_boot_time", lambda: backdated - timedelta(hours=1))

  patch_instructions_content(monkeypatch)
  monkeypatch.setenv("SHIM_MODE", "immediate")  # any hypothetical respawn exits fast
  monkeypatch.setenv("SHIM_STATE", str(state))
  await init_module.run_crash_recovery(_cfg(home, shim), datetime.now(UTC))
  await _await_recovery_tasks()

  # The decisive re-attach proof: exactly one shim invocation ever happened —
  # the turn was followed, never respawned nor replayed.
  assert not (state / "inv-2.argv").exists(), "a second master process was spawned"
  assert "message A" in _shim_prompt(state, 1)
  events = read_chat_events(home, session_id)
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
  assert master_done[0].get(
      "thinking_seconds",
      0) >= 600, (f"interval must count the backdated start; got {master_done[0].get('thinking_seconds')}s")


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

  patch_instructions_content(monkeypatch)
  monkeypatch.setenv("SHIM_MODE", "immediate")
  monkeypatch.setenv("SHIM_STATE", str(state))
  await init_module.run_crash_recovery(_cfg(home, shim), datetime.now(UTC))
  await _await_recovery_tasks()

  # The replay spawned exactly one new agent, with the replay marker + the
  # original content, and the original user event was not rewritten.
  assert (state / "inv-2.argv").exists(), "replayed turn never spawned"
  replayed_prompt = _shim_prompt(state, 2)
  assert replayed_prompt.startswith(master_cc_queue._REPLAY_MARKER)
  assert "message A" in replayed_prompt
  events = read_chat_events(home, session_id)
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
      lambda: _session_meta(home, session_id)["master_run"] is not None and any(
          "ASSISTANT-INV-1" in r.read_text(encoding="utf-8", errors="replace") for r in _raw_logs(home, session_id)),
      timeout=20.0,
      what="turn A did not start/persist identity and first output")
  proc.kill()
  proc.wait(timeout=10)

  patch_instructions_content(monkeypatch)
  monkeypatch.setenv("SHIM_MODE", "immediate")
  monkeypatch.setenv("SHIM_STATE", str(state))
  await init_module.run_crash_recovery(_cfg(home, shim), datetime.now(UTC))
  await _await_recovery_tasks()

  # A: re-attached, prompt unmarked. B: one new spawn, marked, only after A.
  assert "message A" in _shim_prompt(state, 1)
  assert not _shim_prompt(state, 1).startswith(master_cc_queue._REPLAY_MARKER)
  assert (state / "inv-2.argv").exists(), "queued message B was never replayed"
  replayed_prompt = _shim_prompt(state, 2)
  assert replayed_prompt.startswith(master_cc_queue._REPLAY_MARKER)
  assert "message B" in replayed_prompt

  events = read_chat_events(home, session_id)
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
async def test_replayed_delegate_readback_lands_on_existing_thread(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A replayed turn re-issues the same `delegate` call across the restart;
  the sent-but-lost CLI read-back deterministically resolves to the thread the
  first attempt already created (thread count stays 1)."""
  home = tmp_path / "home"
  shim, state = _install_shim(tmp_path)
  spec_file = tmp_path / "task_spec.md"
  spec_file.write_text(SPEC_TEXT, encoding="utf-8")
  proc, session_id = _launch_driver(tmp_path, home, shim, "delegate", "hang", extra_args=[str(spec_file)])
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

    patch_instructions_content(monkeypatch)
    monkeypatch.setenv("SHIM_MODE", "delegate")
    monkeypatch.setenv("SHIM_STATE", str(state))
    monkeypatch.setenv("SHIM_DELEGATE_SESSION", session_id)
    monkeypatch.setenv("SHIM_DELEGATE_REPO", str(REPO_ROOT))
    monkeypatch.setenv("SHIM_DELEGATE_SPEC", str(spec_file))
    monkeypatch.setenv("CHARLIEBOT_HOME", str(home))
    monkeypatch.setenv("PYTHONPATH", str(REPO_ROOT))
    await init_module.run_crash_recovery(_cfg(home, shim), datetime.now(UTC))
    await _await_recovery_tasks()
  finally:
    black_hole.close()

  # The replayed turn carried the marker and completed.
  assert (state / "inv-2.argv").exists(), "replayed turn never spawned"
  assert _shim_prompt(state, 2).startswith(master_cc_queue._REPLAY_MARKER)
  assert (state / "delegate.rc").read_text(encoding="utf-8").strip() == "0", (
      f"delegate CLI failed: {(state / 'delegate.stderr').read_text(encoding='utf-8', errors='replace')}")

  # Read-back determinism: the call resolved to the seeded thread, and no
  # second thread for the same spec exists.
  delegate_out = json.loads((state / "delegate.stdout").read_text(encoding="utf-8"))
  threads_dir = home / "sessions" / session_id / "threads"
  thread_metas = [
      json.loads((p / "metadata.json").read_text(encoding="utf-8"))
      for p in threads_dir.iterdir()
      if (p / "metadata.json").exists()
  ]
  matching = [m for m in thread_metas if m.get("description") == SPEC_TEXT]
  assert len(matching) == 1, "the replayed delegate spawned a duplicate worker thread"
  assert delegate_out["thread_id"] == matching[0]["id"]
  assert _session_meta(home, session_id)["master_run"] is None


@pytest.mark.asyncio
async def test_completed_turn_drained_after_server_kill(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The turn's final result landed on disk inside the server-down window:
  recovery resolves the COMPLETED row, drains the bytes after the cursor
  through the follower, and closes the round exactly once."""
  home = tmp_path / "home"
  shim, state = _install_shim(tmp_path)
  proc, session_id = _launch_driver(tmp_path, home, shim, "chat", "sleep_first")
  _wait_for(
      lambda: _session_meta(home, session_id)["master_run"] is not None and any(
          "ASSISTANT-INV-1" in r.read_text(encoding="utf-8", errors="replace") for r in _raw_logs(home, session_id)),
      timeout=20.0,
      what="turn A did not start/persist identity and first output")
  # Kill the server; the producer then finishes while nobody is consuming.
  proc.kill()
  proc.wait(timeout=10)
  _wait_for(
      lambda: _turn_finished_on_disk(home, session_id, "RESULT-INV-1"),
      timeout=20.0,
      what="agent did not finish during the server-down window")

  patch_instructions_content(monkeypatch)
  monkeypatch.setenv("SHIM_MODE", "immediate")  # any hypothetical respawn exits fast
  monkeypatch.setenv("SHIM_STATE", str(state))
  cfg = _cfg(home, shim)
  await init_module.run_crash_recovery(cfg, datetime.now(UTC))
  await _await_recovery_tasks()

  # Exactly one answer: MASTER_DONE landed once, and the user message was NOT
  # replayed (a replay would have spawned a second agent). Both would mean
  # the COMPLETED row was drained AND replayed; neither would mean it was
  # cleared unanswered — the regression this row prevents.
  assert not (state / "inv-2.argv").exists(), "recovering a completed turn must start no agent process"
  events = read_chat_events(home, session_id)
  assert len([e for e in events if e.get("type") == "user"]) == 1
  master_done = [e for e in events if e.get("type") == "master_done"]
  assert len(master_done) == 1
  assert master_done[0].get("exit_code") == 0
  assert _session_meta(home, session_id)["master_run"] is None

  # Lossless, duplicate-free: the round's transported events equal a full
  # projection of the raw log from offset 0 under a fresh translate, and the
  # cursor ends exactly at the file size.
  assert _round_transported_events(events, skip_user_event=True) == _full_projection(home, session_id, cfg)
  raw = _raw_logs(home, session_id)[0]
  assert runs.read_raw_cursor(raw.parent / runs.CURSOR_NAME) == raw.stat().st_size

  # The recovered round is operable (separator with event_index).
  _assert_round_operable(events)

  # Idempotent: re-running recovery over the same on-disk state is a no-op.
  chat_path = home / "sessions" / session_id / "data" / "chat_events.jsonl"
  before = chat_path.read_bytes()
  await init_module.run_crash_recovery(cfg, datetime.now(UTC))
  await _await_recovery_tasks()
  assert chat_path.read_bytes() == before
  assert _session_meta(home, session_id)["master_run"] is None


@pytest.mark.asyncio
async def test_missing_pid_start_turn_reattached_after_server_kill(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The recorded turn's pid_start is scrubbed (death unprovable): recovery
  keeps it on the RUNNING channel — re-attached with a constant-true probe,
  master_run kept while following, the user message never replayed — and the
  turn's real result event then closes the round exactly once."""
  home = tmp_path / "home"
  shim, state = _install_shim(tmp_path)
  # sleep_first: assistant line, ~3s of silence, then the result event.
  proc, session_id = _launch_driver(tmp_path, home, shim, "chat", "sleep_first")
  _wait_for(
      lambda: _session_meta(home, session_id)["master_run"] is not None and any(
          "ASSISTANT-INV-1" in r.read_text(encoding="utf-8", errors="replace") for r in _raw_logs(home, session_id)),
      timeout=20.0,
      what="turn A did not start/persist identity and first output")
  proc.kill()
  proc.wait(timeout=10)

  # Scrub pid_start from the recorded turn: the agent is alive, but the
  # recorded identity can no longer prove death.
  meta_path = home / "sessions" / session_id / "metadata.json"
  meta = json.loads(meta_path.read_text(encoding="utf-8"))
  record = meta["master_run"]
  assert record is not None and record["pid"] is not None
  record["pid_start"] = None
  meta_path.write_text(json.dumps(meta), encoding="utf-8")

  patch_instructions_content(monkeypatch)
  monkeypatch.setenv("SHIM_MODE", "immediate")  # any hypothetical respawn exits fast
  monkeypatch.setenv("SHIM_STATE", str(state))
  # With a constant-true probe the follow ends on the post-result timeout;
  # keep it fast.
  monkeypatch.setattr("src.agents.backends.base.AgentBackend._POST_RESULT_TIMEOUT", 1.0)
  await init_module.run_crash_recovery(_cfg(home, shim), datetime.now(UTC))

  # Mid-state (the shim is still sleeping toward its result): re-attached via
  # the RUNNING channel — the record is kept, no replay was dispatched, and
  # no master-side report was posted for a mounted row.
  assert _session_meta(home, session_id)["master_run"] is not None
  assert not (state / "inv-2.argv").exists()

  await _await_recovery_tasks()

  # The turn's real result event closed the round: exactly one answer, the
  # original user event unreplayed, the record cleared by the normal path.
  assert not (state / "inv-2.argv").exists(), "a second master process was spawned"
  assert not _shim_prompt(state, 1).startswith(master_cc_queue._REPLAY_MARKER)
  events = read_chat_events(home, session_id)
  assert len([e for e in events if e.get("type") == "user"]) == 1
  master_done = [e for e in events if e.get("type") == "master_done"]
  assert len(master_done) == 1
  assert master_done[0].get("exit_code") == 0
  assert _session_meta(home, session_id)["master_run"] is None
  assert not any(e.get("source") == "crash_recovery" for e in events)


@pytest.mark.asyncio
async def test_completed_delegate_wake_drained_after_server_kill(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A delegate/cron wake has no user event to replay: a result that landed
  during downtime is still drained and closed — never left silent."""
  home = tmp_path / "home"
  shim, state = _install_shim(tmp_path)
  proc, session_id = _launch_driver(tmp_path, home, shim, "wake", "sleep_first")
  _wait_for(
      lambda: _session_meta(home, session_id)["master_run"] is not None and any(
          "ASSISTANT-INV-1" in r.read_text(encoding="utf-8", errors="replace") for r in _raw_logs(home, session_id)),
      timeout=20.0,
      what="wake turn did not start/persist identity and first output")
  proc.kill()
  proc.wait(timeout=10)
  _wait_for(
      lambda: _turn_finished_on_disk(home, session_id, "RESULT-INV-1"),
      timeout=20.0,
      what="agent did not finish during the server-down window")

  patch_instructions_content(monkeypatch)
  monkeypatch.setenv("SHIM_MODE", "immediate")
  monkeypatch.setenv("SHIM_STATE", str(state))
  cfg = _cfg(home, shim)
  await init_module.run_crash_recovery(cfg, datetime.now(UTC))
  await _await_recovery_tasks()

  # Exactly one answer, delivered by the drain; the replay pass had nothing
  # to redeliver (the chat log holds no user event for this turn).
  assert not (state / "inv-2.argv").exists(), "recovering a completed turn must start no agent process"
  events = read_chat_events(home, session_id)
  assert not [e for e in events if e.get("type") == "user"]
  master_done = [e for e in events if e.get("type") == "master_done"]
  assert len(master_done) == 1
  assert master_done[0].get("exit_code") == 0
  assert _session_meta(home, session_id)["master_run"] is None

  # Lossless, duplicate-free drain, cursor at file size, round operable, idempotent.
  assert _round_transported_events(events, skip_user_event=False) == _full_projection(home, session_id, cfg)
  raw = _raw_logs(home, session_id)[0]
  assert runs.read_raw_cursor(raw.parent / runs.CURSOR_NAME) == raw.stat().st_size
  _assert_round_operable(events)

  chat_path = home / "sessions" / session_id / "data" / "chat_events.jsonl"
  before = chat_path.read_bytes()
  await init_module.run_crash_recovery(cfg, datetime.now(UTC))
  await _await_recovery_tasks()
  assert chat_path.read_bytes() == before


@pytest.mark.asyncio
async def test_stalled_turn_reattached_after_server_kill(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Alive but silent beyond the report threshold: re-attached exactly like the
  RUNNING row. Unlike the worker side, the master side posts no stall report."""
  home = tmp_path / "home"
  shim, state = _install_shim(tmp_path)
  proc, session_id = _launch_driver(tmp_path, home, shim, "chat", "hang")
  _wait_for(
      lambda: _session_meta(home, session_id)["master_run"] is not None and any(
          "ASSISTANT-INV-1" in r.read_text(encoding="utf-8", errors="replace") for r in _raw_logs(home, session_id)),
      timeout=20.0,
      what="turn A did not start/persist identity and first output")
  # Only the server dies; the agent hangs on, silent.
  proc.kill()
  proc.wait(timeout=10)
  raw = _raw_logs(home, session_id)[0]
  silent_since = time.time() - (NO_OUTPUT_REPORT_THRESHOLD + 60)
  os.utime(raw, (silent_since, silent_since))

  patch_instructions_content(monkeypatch)
  monkeypatch.setenv("SHIM_MODE", "immediate")
  monkeypatch.setenv("SHIM_STATE", str(state))
  await init_module.run_crash_recovery(_cfg(home, shim), datetime.now(UTC))

  # Re-attached, not cleared: the record is still filed (a clear happens
  # synchronously inside recovery), and no replay was dispatched.
  assert _session_meta(home, session_id)["master_run"] is not None
  assert not (state / "inv-2.argv").exists()

  # The producer dies; the re-attached follower then closes the round.
  _kill_agent_only(home, session_id)
  await _await_recovery_tasks()

  events = read_chat_events(home, session_id)
  assert len([e for e in events if e.get("type") == "user"]) == 1
  master_done = [e for e in events if e.get("type") == "master_done"]
  assert len(master_done) == 1
  # Drained without a trailing result: same failure code as the worker side's
  # died-mid-run (-1), never a spawned turn's 0.
  assert master_done[0].get("exit_code") == -1
  assert _session_meta(home, session_id)["master_run"] is None
  # No worker-side STALLED chat report on the master path.
  assert not any(e.get("source") == "crash_recovery" for e in events)


@pytest.mark.asyncio
async def test_midrun_death_delegate_wake_drained_as_failure(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A delegate/cron wake killed mid-run (no result): no user message exists
  that pass 2 could replay, so the leftover bytes are drained and the round
  closes as a failure — exactly one answer, no spawn, no replay."""
  home = tmp_path / "home"
  shim, state = _install_shim(tmp_path)
  proc, session_id = _launch_driver(tmp_path, home, shim, "wake", "hang")
  _wait_for(
      lambda: _session_meta(home, session_id)["master_run"] is not None and any(
          "ASSISTANT-INV-1" in r.read_text(encoding="utf-8", errors="replace") for r in _raw_logs(home, session_id)),
      timeout=20.0,
      what="wake turn did not start/persist identity and first output")
  proc.kill()
  proc.wait(timeout=10)
  _kill_agent_only(home, session_id)

  patch_instructions_content(monkeypatch)
  monkeypatch.setenv("SHIM_MODE", "immediate")
  monkeypatch.setenv("SHIM_STATE", str(state))
  await init_module.run_crash_recovery(_cfg(home, shim), datetime.now(UTC))
  await _await_recovery_tasks()

  assert not (state / "inv-2.argv").exists(), "a mid-run-dead wake must not spawn a new agent"
  events = read_chat_events(home, session_id)
  assert not [e for e in events if e.get("type") == "user"]
  master_done = [e for e in events if e.get("type") == "master_done"]
  assert len(master_done) == 1
  assert master_done[0].get("exit_code") == -1
  assert _session_meta(home, session_id)["master_run"] is None
  raw = _raw_logs(home, session_id)[0]
  assert runs.read_raw_cursor(raw.parent / runs.CURSOR_NAME) == raw.stat().st_size
  _assert_round_operable(events)


@pytest.mark.asyncio
@pytest.mark.parametrize("with_user_message", [True, False], ids=["user-wake", "delegate-wake"])
async def test_uncovered_transport_turn_cleared_not_drained(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, with_user_message: bool) -> None:
  """An interrupted turn on an uncovered backend transport (opencode /
  antigravity / tui-cli) is drained NEVER: with the pid_start pin present the
  dead instance's death is provable, so the record resolves DIED with the
  transport reason, clears WITHOUT any uncovered-alive report, and the user
  message — when one exists — is answered by the replay pass. No user
  message, nothing to answer: the round simply closes."""
  home = tmp_path / "home"
  shim, state = _install_shim(tmp_path)
  cfg = CharlieBotConfig(
      charliebot_home=home,
      worktree_dir=str(home / "worktrees"),
      backend_options=[
          BackendOption(id="fake", label="Fake", type="cc-claude", model="fake-model", cli_binary=str(shim)),
          BackendOption(id="oc", label="OC", type="opencode", model="oc-model", prompt_overlay="none"),
      ],
  )
  session_mgr = SessionManager(cfg)
  meta = await session_mgr.create_session(CreateSessionRequest(name="t"), backend="oc")
  user_event_id = None
  if with_user_message:
    user_event = {"type": "user", "content": "message A"}
    await session_mgr.save_chat_event(meta.id, user_event)
    user_event_id = user_event["id"]
  record = MasterRunRecord(
      pid=999999,
      pid_start="1",
      started_at=datetime.now(UTC) - timedelta(seconds=5),
      raw_log=str(home / "sessions" / meta.id / "data" / "master_runs" / "gone" / runs.RAW_LOG_NAME),
      user_event_id=user_event_id,
  )
  await session_mgr.persist_master_run(meta.id, record)

  # Capture the replay instead of spawning the uncovered backend: the marker
  # application lives in replay_user_message, which stays real.
  replays: list[dict] = []

  async def _capture_run_message(cfg, session_meta, user_content, callbacks, **kwargs) -> None:
    replays.append({"content": user_content, "user_event_id": kwargs.get("user_event_id")})

  monkeypatch.setattr(master_cc_queue, "run_message", _capture_run_message)
  patch_instructions_content(monkeypatch)

  with capture_logs() as logs:
    await init_module.run_crash_recovery(cfg, datetime.now(UTC))
  await _await_recovery_tasks()

  # Provable death, not liveness limbo: DIED carrying the transport reason —
  # never an uncovered-alive report, which is the pin-less record's judgment.
  resolved = [e for e in logs if e.get("event") == "master_run_resolved"]
  assert len(resolved) == 1
  assert resolved[0]["outcome"] == runs.RunOutcome.DIED.value
  assert resolved[0]["reason"] == runs.TRANSPORT_NOT_COVERED_REASON
  assert _session_meta(home, meta.id)["master_run"] is None
  events = read_chat_events(home, meta.id)
  assert not any(e.get("source") == "crash_recovery" for e in events)
  # The drain invariant: no drain ran, so this turn never produced a MASTER_DONE.
  assert not any(e.get("type") == "master_done" for e in events)
  if with_user_message:
    assert len(replays) == 1
    assert replays[0]["content"].startswith(master_cc_queue._REPLAY_MARKER)
    assert "message A" in replays[0]["content"]
    assert replays[0]["user_event_id"] == user_event_id
  else:
    assert not replays
  assert not (state / "inv-1.argv").exists(), "recovery must not spawn any agent for this row"


@pytest.mark.asyncio
async def test_uncovered_transport_alive_turn_reported_kept_not_replayed(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The provably-ALIVE counterpart of the dead pinned row: a pinned
  opencode-flavored master_run whose recorded process still lives resolves
  RUNNING uncovered-alive — reported exactly once, record kept, user message
  excluded from replay and judged again on the next restart."""
  home = tmp_path / "home"
  shim, state = _install_shim(tmp_path)
  cfg = CharlieBotConfig(
      charliebot_home=home,
      worktree_dir=str(home / "worktrees"),
      backend_options=[
          BackendOption(id="fake", label="Fake", type="cc-claude", model="fake-model", cli_binary=str(shim)),
          BackendOption(id="oc", label="OC", type="opencode", model="oc-model", prompt_overlay="none"),
      ],
  )
  session_mgr = SessionManager(cfg)
  meta = await session_mgr.create_session(CreateSessionRequest(name="t"), backend="oc")
  user_event = {"type": "user", "content": "message A"}
  await session_mgr.save_chat_event(meta.id, user_event)

  # A live stand-in for the persisted turn's process instance: reconcile must
  # prove liveness off the REAL (pid, pid_start) pair, never off the test's say-so.
  live_shim = subprocess.Popen(["sleep", "60"])
  try:
    stat_pair = runs.read_pid_stat(live_shim.pid)
    assert stat_pair is not None
    record = MasterRunRecord(
        pid=live_shim.pid,
        pid_start=stat_pair[0],
        started_at=datetime.now(UTC) - timedelta(seconds=5),
        raw_log=str(home / "sessions" / meta.id / "data" / "master_runs" / "live" / runs.RAW_LOG_NAME),
        user_event_id=user_event["id"],
    )
    await session_mgr.persist_master_run(meta.id, record)

    replays: list[dict] = []

    async def _capture_run_message(cfg, session_meta, user_content, callbacks, **kwargs) -> None:
      replays.append({"content": user_content, "user_event_id": kwargs.get("user_event_id")})

    monkeypatch.setattr(master_cc_queue, "run_message", _capture_run_message)
    patch_instructions_content(monkeypatch)

    await init_module.run_crash_recovery(cfg, datetime.now(UTC))
    await _await_recovery_tasks()

    events = read_chat_events(home, meta.id)
    reports = [e for e in events if e.get("source") == "crash_recovery"]
    assert len(reports) == 1
    assert runs.UNCOVERED_ALIVE_REASON in reports[0]["content"]
    assert _session_meta(home, meta.id)["master_run"] is not None
    assert not replays
    assert not (state / "inv-1.argv").exists(), "a report-only row must not spawn any agent"
  finally:
    live_shim.kill()
    live_shim.wait(timeout=10)


@pytest.mark.asyncio
@pytest.mark.parametrize(
    ("pid", "pid_start"),
    [(999999, "1"), (None, None)],
    ids=["legacy-raw-missing", "never-started"],
)
async def test_undrainable_dead_turn_replayed_with_marker(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, pid, pid_start) -> None:
  """Raw log missing (pre-transport record) or turn never spawned: nothing is
  drainable, the record clears, and the user message is replayed with the
  marker — exactly one answer, by replay and only by replay."""
  home = tmp_path / "home"
  shim, state = _install_shim(tmp_path)
  cfg = _cfg(home, shim)
  session_mgr = SessionManager(cfg)
  meta = await session_mgr.create_session(CreateSessionRequest(name="t"))
  user_event = {"type": "user", "content": "message A"}
  await session_mgr.save_chat_event(meta.id, user_event)
  record = MasterRunRecord(
      pid=pid,
      pid_start=pid_start,
      started_at=datetime.now(UTC) - timedelta(seconds=5),
      raw_log=str(home / "sessions" / meta.id / "data" / "master_runs" / "gone" / runs.RAW_LOG_NAME),
      user_event_id=user_event["id"],
  )
  await session_mgr.persist_master_run(meta.id, record)

  patch_instructions_content(monkeypatch)
  monkeypatch.setenv("SHIM_MODE", "immediate")
  monkeypatch.setenv("SHIM_STATE", str(state))
  await init_module.run_crash_recovery(cfg, datetime.now(UTC))
  await _await_recovery_tasks()

  # Exactly one answer: one new agent, carrying the marker + the original
  # content; the original user event was not rewritten nor duplicated.
  assert (state / "inv-1.argv").exists(), "replayed turn never spawned"
  assert not (state / "inv-2.argv").exists()
  replayed_prompt = _shim_prompt(state, 1)
  assert replayed_prompt.startswith(master_cc_queue._REPLAY_MARKER)
  assert "message A" in replayed_prompt
  events = read_chat_events(home, meta.id)
  assert len([e for e in events if e.get("type") == "user"]) == 1
  assert sum(1 for e in events if e.get("type") == "master_done") == 1
  assert _session_meta(home, meta.id)["master_run"] is None


@pytest.mark.asyncio
async def test_graceful_teardown_detached_turn_reattached_and_answered_once(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A REAL event-loop teardown mid-turn (the driver cancels the consumer and
  exits 0) leaves the master agent child alive, the master_run record on
  disk, and no terminal state behind; a fresh reconcile then re-attaches and
  the round is answered exactly once. This is the only proof the transport's
  default kill on loop close is avoided — a SIGKILL driver cannot exercise the
  detach, and an in-process cancel cannot exercise the loop teardown."""
  home = tmp_path / "home"
  shim, state = _install_shim(tmp_path)
  proc, session_id = _launch_master_graceful_driver(tmp_path, home, shim)
  assert proc.returncode == 0

  # The child outlived the closing event loop — asserted BEFORE any reconcile
  # runs, so this cannot pass by the reconcile respawning something.
  record = _session_meta(home, session_id)["master_run"]
  assert record is not None, "teardown cleared the record a boot needs"
  os.kill(record["pid"], 0)
  events = read_chat_events(home, session_id)
  assert not any(e.get("type") == "master_done" for e in events)
  assert not _session_meta(home, session_id).get("has_unread"), "teardown wrote terminal state"

  # Next boot: re-attach and finish — the round is answered exactly once.
  patch_instructions_content(monkeypatch)
  monkeypatch.setenv("SHIM_MODE", "immediate")  # any hypothetical respawn exits fast
  monkeypatch.setenv("SHIM_STATE", str(state))
  await init_module.run_crash_recovery(_cfg(home, shim), datetime.now(UTC))
  await _await_recovery_tasks()

  assert not (state / "inv-2.argv").exists(), "a second master process was spawned"
  events = read_chat_events(home, session_id)
  markers = [e for e in events if "ASSISTANT-INV-1" in json.dumps(e)]
  assert len(markers) == 1, "the shim's assistant marker must land exactly once"
  master_done = [e for e in events if e.get("type") == "master_done"]
  assert len(master_done) == 1
  assert master_done[0].get("exit_code") == 0
  assert _session_meta(home, session_id)["master_run"] is None
