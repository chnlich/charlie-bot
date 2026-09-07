"""Worker account relay: a delegated task keeps running when its pool login runs out of quota.

Covers the pool pin at construction (spawner_launch._construct_worker), the Worker's own
relay loop (src/agents/worker.py run/_relay), the disposition of an exhausted pool through
_stream_worker_events and spawn_worker, and the completion notice carrying the reset time.
"""

import json
from pathlib import Path
from typing import Any
from unittest.mock import AsyncMock

import pytest
from conftest import JudgmentShim, fresh_state_fixture, make_transcript, rate_limit_event, write_pool_credentials

from src.agents import worker as worker_mod
from src.agents.worker import QuotaExhaustedException, Worker
from src.core import (
    claude_accounts,
    claude_compaction,
    claude_relay,
    spawner,
    spawner_finalize,
    spawner_launch,
)
from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.models import (
    BackendOption,
    ClaudeAccount,
    SpawnRequest,
    ThreadMetadata,
    ThreadStatus,
)

FABLE = "claude-fable-5-1"
POOLED_ID = "claude-fable-5"
CC_ID = "11111111-2222-3333-4444-555555555555"

_fresh_pool_state = fresh_state_fixture(claude_accounts.reset_for_tests)


def _pool_cfg(tmp_path: Path, labels: tuple[str, ...] = ("main", "ext-1", "ext-2")) -> CharlieBotConfig:
  accounts = [ClaudeAccount(label=label, config_dir=str(tmp_path / f"claude-{label}")) for label in labels]
  for account in accounts:
    write_pool_credentials(Path(account.config_dir))
  return CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      worktree_dir=str(tmp_path / "worktrees"),
      claude_accounts=accounts,
      backend_options=[
          BackendOption(id=POOLED_ID, label="Fable", type="cc-claude", model=FABLE),
          BackendOption(
              id="pinned", label="Pinned", type="cc-claude", model=FABLE, claude_config_dir=str(tmp_path / "pinned")),
      ],
  )


def _reject(label: str) -> None:
  claude_accounts.observe_rate_limit(label, rate_limit_event("rejected", 1.0)["rate_limit_info"])


def _tool_result() -> dict:
  return {"type": ET.USER, "message": {"content": [{"type": ET.TOOL_RESULT, "tool_use_id": "t1", "content": "ok"}]}}


def _assistant(text: str, prompt_tokens: int | None = None) -> dict:
  message: dict[str, Any] = {"content": [{"type": "text", "text": text}]}
  if prompt_tokens is not None:
    message["usage"] = {"input_tokens": 10, "cache_read_input_tokens": prompt_tokens - 10}
  return {"type": ET.ASSISTANT, "message": message}


def _result() -> dict:
  return {"type": ET.RESULT, "subtype": "success", "is_error": False, "result": "done"}


class _ScriptedBackend:
  """Yields a script of events; terminate() ends the stream and records the kill."""

  def __init__(self, events: list[dict], exit_code: int, stderr_text: str = "") -> None:
    self._events = events
    self.exit_code = exit_code
    self.stderr_text = stderr_text
    self.terminated = False
    self.hang_diagnostics = None
    self.pid_start = "1-1"
    self.prompt: str | None = None
    self.env: dict | None = None
    self.cwd: str | None = None

  async def terminate(self) -> None:
    self.terminated = True
    self.exit_code = -15

  def detach(self) -> None:
    pass

  async def run(self, prompt: str, cwd: str, env: dict):
    self.prompt = prompt
    self.cwd = cwd
    self.env = env
    for event in self._events:
      if self.terminated:
        return
      yield event


def _install_backends(monkeypatch: pytest.MonkeyPatch, backends: list[_ScriptedBackend]) -> list[dict]:
  builds: list[dict] = []
  queue = list(backends)

  def fake_build_backend(option: BackendOption, cfg: CharlieBotConfig, **kwargs: Any) -> _ScriptedBackend:
    backend = queue.pop(0)
    builds.append({"option": option, "kwargs": kwargs, "backend": backend})
    return backend

  monkeypatch.setattr(worker_mod, "build_backend", fake_build_backend)
  return builds


def _thread() -> ThreadMetadata:
  return ThreadMetadata(
      id="t1", session_id="s1", description="task", backend=POOLED_ID, model=FABLE, claude_session_id=CC_ID)


def _worker(tmp_path: Path, cfg: CharlieBotConfig, label: str | None) -> Worker:
  option = cfg.get_backend_option(POOLED_ID)
  account = claude_accounts.account_by_label(cfg, label) if label else None
  if account is not None:
    option = option.model_copy(update={"claude_config_dir": account.config_dir})
  return Worker(
      _thread(),
      tmp_path / "work",
      tmp_path / "data" / "events.jsonl",
      "do the thing",
      cfg,
      backend_option=option,
      claude_account=account,
  )


def _logged_events(tmp_path: Path) -> list[dict]:
  lines = (tmp_path / "data" / "events.jsonl").read_text(encoding="utf-8").splitlines()
  return [json.loads(line) for line in lines if line.strip()]


# ---------------------------------------------------------------------------
# Pinning the account at construction
# ---------------------------------------------------------------------------


def test_pin_pool_account_picks_the_most_headroom_and_passes_pinned_entries_through(tmp_path: Path) -> None:
  cfg = _pool_cfg(tmp_path)
  claude_accounts.observe_rate_limit("main", rate_limit_event("allowed_warning", 0.95)["rate_limit_info"])
  claude_accounts.observe_rate_limit("ext-1", rate_limit_event("allowed", 0.20)["rate_limit_info"])
  claude_accounts.observe_rate_limit("ext-2", rate_limit_event("allowed", 0.60)["rate_limit_info"])

  option, account = claude_relay.pin_pool_account(cfg, cfg.get_backend_option(POOLED_ID))
  pinned, pinned_account = claude_relay.pin_pool_account(cfg, cfg.get_backend_option("pinned"))

  assert (account.label, option.claude_config_dir) == ("ext-1", str(tmp_path / "claude-ext-1"))
  assert option.id == POOLED_ID
  assert pinned_account is None and pinned.claude_config_dir == str(tmp_path / "pinned")


def test_pin_pool_account_raises_with_the_earliest_reset_when_every_account_is_rejected(tmp_path: Path) -> None:
  cfg = _pool_cfg(tmp_path)
  for label in ("main", "ext-1", "ext-2"):
    _reject(label)

  with pytest.raises(claude_relay.PoolExhaustedError, match="earliest reset"):
    claude_relay.pin_pool_account(cfg, cfg.get_backend_option(POOLED_ID))


class _ThreadManager:

  def __init__(self, events_path: Path) -> None:
    self.events_path = events_path
    self.saved: list[ThreadMetadata] = []

  async def save_metadata(self, thread: ThreadMetadata) -> None:
    self.saved.append(thread.model_copy(deep=True))

  async def get_events_log_path(self, session_id: str, thread_id: str) -> Path:
    del session_id, thread_id
    return self.events_path


@pytest.mark.asyncio
async def test_construct_worker_pins_the_pool_account_onto_the_worker_only(tmp_path: Path) -> None:
  cfg = _pool_cfg(tmp_path)
  claude_accounts.observe_rate_limit("main", rate_limit_event("allowed", 0.80)["rate_limit_info"])
  thread = ThreadMetadata(id="t1", session_id="s1", description="task")
  thread_mgr = _ThreadManager(tmp_path / "events.jsonl")
  request = SpawnRequest(resolved_backend=POOLED_ID, resolved_model=FABLE)

  worker = await spawner_launch._construct_worker("s1", thread, tmp_path / "work", "prompt", cfg, thread_mgr, request)

  assert worker.claude_account.label == "ext-1"
  assert worker._backend_option.claude_config_dir == str(tmp_path / "claude-ext-1")
  assert (thread.backend, thread.model) == (POOLED_ID, FABLE)
  assert thread_mgr.saved[-1].backend == POOLED_ID
  assert thread.claude_session_id


# ---------------------------------------------------------------------------
# The Worker's relay loop
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_worker_relays_a_rejected_run_onto_another_account(tmp_path: Path, monkeypatch) -> None:
  cfg = _pool_cfg(tmp_path)
  source_transcript = make_transcript(tmp_path / "claude-main", CC_ID)
  first = _ScriptedBackend([_assistant("working"), rate_limit_event("rejected", 1.0)], exit_code=1)
  second = _ScriptedBackend([_assistant("done"), _result()], exit_code=0)
  builds = _install_backends(monkeypatch, [first, second])
  worker = _worker(tmp_path, cfg, "main")

  exit_code = await worker.run()

  assert exit_code == 0
  assert builds[1]["option"].claude_config_dir == str(tmp_path / "claude-ext-1")
  assert builds[1]["kwargs"]["extra_flags"] == ["--resume", CC_ID]
  assert "claude_session_id" not in builds[1]["kwargs"]
  assert builds[0]["kwargs"]["claude_session_id"] == CC_ID
  assert second.prompt == claude_relay.CONTINUATION_PROMPT
  assert (tmp_path / "claude-ext-1" / "projects" / source_transcript.parent.name / f"{CC_ID}.jsonl").exists()
  assert (worker.claude_account.label, worker.account_relays) == ("ext-1", 1)
  logged = _logged_events(tmp_path)
  assert [ev["type"] for ev in logged if ev["type"] == ET.ASSISTANT] == [ET.ASSISTANT, ET.ASSISTANT]
  assert any(ev["type"] == ET.RATE_LIMIT_EVENT for ev in logged)


@pytest.mark.asyncio
async def test_worker_terminates_at_the_safe_point_after_a_far_warning_and_relays(tmp_path: Path, monkeypatch) -> None:
  cfg = _pool_cfg(tmp_path)
  make_transcript(tmp_path / "claude-main", CC_ID)
  first = _ScriptedBackend(
      [rate_limit_event("allowed_warning", 0.92),
       _tool_result(), _assistant("never streamed")], exit_code=0)
  second = _ScriptedBackend([_result()], exit_code=0)
  _install_backends(monkeypatch, [first, second])
  worker = _worker(tmp_path, cfg, "main")

  exit_code = await worker.run()

  assert exit_code == 0
  assert first.terminated
  assert not any("never streamed" in json.dumps(ev) for ev in _logged_events(tmp_path))
  assert any(ev["type"] == ET.USER for ev in _logged_events(tmp_path))
  assert worker.claude_account.label == "ext-1"


@pytest.mark.asyncio
async def test_worker_outside_the_pool_still_raises_on_rejection(tmp_path: Path, monkeypatch) -> None:
  cfg = CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backend_options=[BackendOption(id=POOLED_ID, label="Fable", type="cc-claude", model=FABLE)],
  )
  _install_backends(monkeypatch, [_ScriptedBackend([rate_limit_event("rejected", 1.0)], exit_code=1)])
  worker = _worker(tmp_path, cfg, None)

  with pytest.raises(QuotaExhaustedException, match="Rate limited"):
    await worker.run()


@pytest.mark.asyncio
async def test_worker_raises_pool_exhausted_when_no_account_is_left(tmp_path: Path, monkeypatch) -> None:
  cfg = _pool_cfg(tmp_path, labels=("main", "ext-1"))
  make_transcript(tmp_path / "claude-main", CC_ID)
  _reject("ext-1")
  _install_backends(monkeypatch, [_ScriptedBackend([rate_limit_event("rejected", 1.0)], exit_code=1)])
  worker = _worker(tmp_path, cfg, "main")

  with pytest.raises(claude_relay.PoolExhaustedError, match="earliest reset"):
    await worker.run()


@pytest.mark.asyncio
async def test_worker_stops_after_the_relay_limit(tmp_path: Path, monkeypatch) -> None:
  cfg = _pool_cfg(tmp_path, labels=("main", "a", "b", "c"))
  make_transcript(tmp_path / "claude-main", CC_ID)
  backends = [_ScriptedBackend([rate_limit_event("rejected", 1.0)], exit_code=1) for _ in range(4)]
  builds = _install_backends(monkeypatch, backends)
  worker = _worker(tmp_path, cfg, "main")

  with pytest.raises(RuntimeError, match="relay limit"):
    await worker.run()

  assert len(builds) == 1 + claude_relay.MAX_RELAYS_PER_TURN
  assert worker.account_relays == claude_relay.MAX_RELAYS_PER_TURN


@pytest.mark.asyncio
async def test_worker_login_failure_marks_the_account_and_notifies_the_session(tmp_path: Path, monkeypatch) -> None:
  cfg = _pool_cfg(tmp_path)
  make_transcript(tmp_path / "claude-main", CC_ID)
  first = _ScriptedBackend([_assistant("Failed to authenticate. Please run /login")], exit_code=1)
  second = _ScriptedBackend([_result()], exit_code=0)
  _install_backends(monkeypatch, [first, second])
  worker = _worker(tmp_path, cfg, "main")
  worker.on_session_event = AsyncMock()

  exit_code = await worker.run()

  assert exit_code == 0
  assert not claude_accounts.healthy(claude_accounts.account_by_label(cfg, "main"))
  notice = worker.on_session_event.await_args.args[0]
  assert notice["type"] == ET.CLAUDE_ACCOUNT_LOGIN_REQUIRED
  assert (notice["account"], notice["reason"]) == ("main", "auth_failed")
  assert any(ev["type"] == ET.CLAUDE_ACCOUNT_LOGIN_REQUIRED for ev in _logged_events(tmp_path))
  assert worker.claude_account.label == "ext-1"


@pytest.mark.asyncio
@pytest.mark.parametrize(("prompt_tokens", "compacted"), [(150_000, True), (20_000, False)])
async def test_worker_relay_compacts_a_large_fable_context_on_the_new_account(
    tmp_path: Path, monkeypatch, prompt_tokens: int, compacted: bool) -> None:
  cfg = _pool_cfg(tmp_path)
  make_transcript(tmp_path / "claude-main", CC_ID)
  compact = AsyncMock()
  monkeypatch.setattr(claude_compaction, "compact_with_sonnet", compact)
  first = _ScriptedBackend(
      [_assistant("big", prompt_tokens=prompt_tokens),
       rate_limit_event("rejected", 1.0)], exit_code=1)
  second = _ScriptedBackend([_result()], exit_code=0)
  _install_backends(monkeypatch, [first, second])
  worker = _worker(tmp_path, cfg, "main")

  await worker.run()

  assert compact.await_count == (1 if compacted else 0)
  if compacted:
    kwargs = compact.await_args.kwargs
    assert kwargs["cc_session_id"] == CC_ID
    assert kwargs["config_dir"] == str(tmp_path / "claude-ext-1")
    assert kwargs["pre_tokens"] == prompt_tokens
    assert kwargs["cwd"] == str(tmp_path / "work")
    assert kwargs["log_context"]["trigger"] == "relay"


# ---------------------------------------------------------------------------
# Disposition of an exhausted pool
# ---------------------------------------------------------------------------


class _SessionManager(JudgmentShim):

  def __init__(self) -> None:
    self.events: list[dict] = []

  async def deliver_to_successor(self, session_id: str, event: dict) -> str:
    self.events.append(event)
    return session_id

  async def persist_and_broadcast(self, session_id: str, event: dict) -> None:
    self.events.append(event)

  async def mark_unread(self, session_id: str) -> None:
    del session_id


class _LifecycleThreadManager(_ThreadManager):

  def __init__(self, thread: ThreadMetadata, events_path: Path) -> None:
    super().__init__(events_path)
    self.thread = thread

  async def get_thread(self, session_id: str, thread_id: str) -> ThreadMetadata:
    del session_id, thread_id
    return self.thread

  async def update_status(self, session_id: str, thread_id: str, status: ThreadStatus, **kwargs: Any) -> None:
    del session_id, thread_id, kwargs
    self.thread.status = status


@pytest.mark.asyncio
async def test_stream_worker_events_reports_an_exhausted_pool_as_quota_with_the_reset(
    tmp_path: Path, monkeypatch) -> None:
  cfg = _pool_cfg(tmp_path, labels=("main", "ext-1"))
  make_transcript(tmp_path / "claude-main", CC_ID)
  _reject("ext-1")
  _install_backends(monkeypatch, [_ScriptedBackend([rate_limit_event("rejected", 1.0)], exit_code=1)])
  worker = _worker(tmp_path, cfg, "main")
  thread = _thread()
  session_mgr = _SessionManager()

  outcome = await spawner_finalize._stream_worker_events(
      worker, "s1", thread, _ThreadManager(tmp_path / "e.jsonl"), session_mgr)

  assert outcome.quota_exhausted
  assert "earliest reset" in outcome.error
  assert worker.on_session_event is not None


@pytest.mark.asyncio
async def test_spawn_worker_treats_an_exhausted_pool_at_launch_as_quota_exhaustion(tmp_path: Path, monkeypatch) -> None:
  cfg = _pool_cfg(tmp_path)
  thread = ThreadMetadata(id="t1", session_id="s1", description="task")
  thread_mgr = _LifecycleThreadManager(thread, tmp_path / "events.jsonl")
  session_mgr = _SessionManager()
  finalized: list[Any] = []

  async def exhausted(*args: Any, **kwargs: Any) -> None:
    raise claude_relay.PoolExhaustedError(claude_relay.pool_exhausted_message(cfg))

  async def capture_finalize(ctx: Any, **kwargs: Any) -> None:
    finalized.append(ctx)

  monkeypatch.setattr(spawner_launch, "_create_repoless_process", exhausted)
  monkeypatch.setattr(spawner_finalize, "_finalize_worker_safely", capture_finalize)

  await spawner.spawn_worker(
      "s1",
      "task",
      "t1",
      cfg,
      session_mgr,
      thread_mgr,
      request=SpawnRequest(resolved_backend=POOLED_ID, resolved_model=FABLE))

  assert len(finalized) == 1
  assert finalized[0].outcome.quota_exhausted
  assert "no available account" in finalized[0].outcome.error


@pytest.mark.asyncio
async def test_completion_notice_carries_the_pool_message_after_quota_exhaustion(tmp_path: Path) -> None:
  thread = ThreadMetadata(id="t1", session_id="s1", description="task")
  events_path = tmp_path / "events.jsonl"
  events_path.write_text("", encoding="utf-8")
  thread_mgr = _LifecycleThreadManager(thread, events_path)
  session_mgr = _SessionManager()
  outcome = spawner_finalize._pool_exhausted_outcome(
      claude_relay.PoolExhaustedError("Claude account pool has no available account (earliest reset 13:00 UTC)"))

  _, full_summary = await spawner_finalize._broadcast_completion(
      spawner_finalize._FinalizeCtx(
          session_id="s1",
          description="task",
          thread=thread,
          outcome=outcome,
          thread_mgr=thread_mgr,
          session_mgr=session_mgr,
          cfg=CharlieBotConfig(),
      ),
      verify_report=None,
  )

  assert "API quota exhausted" in full_summary
  assert "earliest reset 13:00 UTC" in full_summary
