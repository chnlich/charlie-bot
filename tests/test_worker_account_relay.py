"""Worker account relay: a delegated task keeps running when its pool login runs out of quota.

Covers the Worker's task run on the shared launch loop (src/runtime/worker.py run, the Claude
lifecycle in src/backends/claude_code/claude_lifecycle.py): placement on the pool account with the
most headroom, rejected-run relay onto another account, the safe-point termination after a far
warning, the relay limit, login-failure marking, and the task relay's no-compaction behavior.
"""

import json
from pathlib import Path
from unittest.mock import AsyncMock

import pytest
from conftest import (
    FABLE_MODEL,
    POOLED_FABLE_ID,
    WORKER_BUILD_BACKEND_PATCH_TARGET,
    ScriptedRelayBackend,
    assistant_text_event,
    fable_pool_cfg,
    fresh_state_fixture,
    install_scripted_backends,
    make_transcript,
    rate_limit_event,
)

from src.backends.claude_code import claude_accounts, claude_compaction, claude_relay
from src.infra import event_types as ET
from src.infra.config import CharlieBotConfig
from src.infra.models import SessionMetadata, ThreadMetadata
from src.runtime.hooks import backend_lifecycle
from src.runtime.worker import Worker

CC_ID = "11111111-2222-3333-4444-555555555555"

_fresh_pool_state = fresh_state_fixture(claude_accounts.reset_for_tests)


def _reject(label: str) -> None:
  claude_accounts.observe_rate_limit(label, rate_limit_event("rejected", 1.0)["rate_limit_info"])


def _result() -> dict:
  return {"type": ET.RESULT, "subtype": "success", "is_error": False, "result": "done"}


def _install_backends(monkeypatch: pytest.MonkeyPatch, backends: list[ScriptedRelayBackend]) -> list[dict]:
  return install_scripted_backends(monkeypatch, backends, WORKER_BUILD_BACKEND_PATCH_TARGET)


def _thread() -> ThreadMetadata:
  return ThreadMetadata(
      id="t1", session_id="s1", description="task", backend=POOLED_FABLE_ID, model=FABLE_MODEL, claude_session_id=CC_ID)


def _worker(tmp_path: Path, cfg: CharlieBotConfig) -> Worker:
  option = cfg.get_backend_option(POOLED_FABLE_ID)
  return Worker(
      _thread(),
      tmp_path / "work",
      tmp_path / "data" / "events.jsonl",
      "do the thing",
      cfg,
      backend_option=option,
      session_meta=SessionMetadata(profile="manager", id="s1", name="S", backend=POOLED_FABLE_ID),
  )


def _logged_events(tmp_path: Path) -> list[dict]:
  lines = (tmp_path / "data" / "events.jsonl").read_text(encoding="utf-8").splitlines()
  return [json.loads(line) for line in lines if line.strip()]


# ---------------------------------------------------------------------------
# The Worker's relay loop
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_worker_relays_a_rejected_run_onto_another_account(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = fable_pool_cfg(tmp_path)
  source_transcript = make_transcript(tmp_path / "claude-main", CC_ID)
  first = ScriptedRelayBackend([assistant_text_event("working"), rate_limit_event("rejected", 1.0)], exit_code=1)
  second = ScriptedRelayBackend([assistant_text_event("done"), _result()], exit_code=0)
  builds = _install_backends(monkeypatch, [first, second])
  worker = _worker(tmp_path, cfg)

  exit_code = await worker.run()

  assert exit_code == 0
  assert builds[1]["kwargs"]["claude_account"].config_dir == str(tmp_path / "claude-ext-1")
  assert builds[1]["kwargs"]["extra_flags"] == ["--resume", CC_ID]
  assert "claude_session_id" not in builds[1]["kwargs"]
  assert builds[0]["kwargs"]["claude_session_id"] == CC_ID
  assert second.prompt == claude_relay.CONTINUATION_PROMPT
  assert (tmp_path / "claude-ext-1" / "projects" / source_transcript.parent.name / f"{CC_ID}.jsonl").exists()
  assert (builds[1]["kwargs"]["claude_account"].label, worker.account_relays) == ("ext-1", 1)
  logged = _logged_events(tmp_path)
  assert [ev["type"] for ev in logged if ev["type"] == ET.ASSISTANT] == [ET.ASSISTANT, ET.ASSISTANT]
  assert any(ev["type"] == ET.RATE_LIMIT_EVENT for ev in logged)


@pytest.mark.asyncio
async def test_worker_raises_pool_exhausted_when_no_account_is_left(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = fable_pool_cfg(tmp_path, labels=("main", "ext-1"))
  make_transcript(tmp_path / "claude-main", CC_ID)
  _reject("ext-1")
  _install_backends(monkeypatch, [ScriptedRelayBackend([rate_limit_event("rejected", 1.0)], exit_code=1)])
  worker = _worker(tmp_path, cfg)

  with pytest.raises(backend_lifecycle.LaunchRefused, match="earliest reset") as excinfo:
    await worker.run()

  assert excinfo.value.quota_exhausted is True


@pytest.mark.asyncio
async def test_worker_relay_stays_inside_the_option_pool(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The relay picks the next account inside the option's pool: a healthy account in another
  pool, with more headroom than every pool member, stays unused."""
  cfg = fable_pool_cfg(
      tmp_path, labels=("main", "ext-1", "ext-2"), claude_pools={
          "alpha": ["main", "ext-1"],
          "beta": ["ext-2"]
      })
  source_transcript = make_transcript(tmp_path / "claude-main", CC_ID)
  # ext-1 sits at half a window; untouched ext-2 (beta) has the most headroom of all.
  claude_accounts.observe_rate_limit("ext-1", rate_limit_event("allowed", 0.50)["rate_limit_info"])
  first = ScriptedRelayBackend([rate_limit_event("rejected", 1.0)], exit_code=1)
  second = ScriptedRelayBackend([assistant_text_event("done"), _result()], exit_code=0)
  builds = _install_backends(monkeypatch, [first, second])
  worker = _worker(tmp_path, cfg)

  exit_code = await worker.run()

  assert exit_code == 0
  assert builds[1]["kwargs"]["claude_account"].config_dir == str(tmp_path / "claude-ext-1")
  assert (tmp_path / "claude-ext-1" / "projects" / source_transcript.parent.name / f"{CC_ID}.jsonl").exists()
  assert claude_accounts.transcript_path(tmp_path / "claude-ext-2", CC_ID) is None


@pytest.mark.asyncio
async def test_worker_pool_exhaustion_names_the_option_pool(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """With every account of the option's pool rejected, the worker ends loudly naming the pool
  and its earliest reset; the other pool's healthy accounts stay unused."""
  cfg = fable_pool_cfg(
      tmp_path, labels=("main", "ext-1", "ext-2"), claude_pools={
          "alpha": ["main"],
          "beta": ["ext-1", "ext-2"]
      })
  make_transcript(tmp_path / "claude-main", CC_ID)
  _install_backends(monkeypatch, [ScriptedRelayBackend([rate_limit_event("rejected", 1.0)], exit_code=1)])
  worker = _worker(tmp_path, cfg)

  with pytest.raises(backend_lifecycle.LaunchRefused) as excinfo:
    await worker.run()

  message = str(excinfo.value)
  assert claude_relay.POOL_EXHAUSTED_PHRASE in message
  assert "'alpha'" in message and "earliest reset" in message and "UTC" in message
  assert claude_accounts.transcript_path(tmp_path / "claude-ext-1", CC_ID) is None
  assert claude_accounts.transcript_path(tmp_path / "claude-ext-2", CC_ID) is None


@pytest.mark.asyncio
async def test_worker_stops_after_the_relay_limit(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = fable_pool_cfg(tmp_path, labels=("main", "a", "b", "c"))
  make_transcript(tmp_path / "claude-main", CC_ID)
  backends = [ScriptedRelayBackend([rate_limit_event("rejected", 1.0)], exit_code=1) for _ in range(4)]
  builds = _install_backends(monkeypatch, backends)
  worker = _worker(tmp_path, cfg)

  with pytest.raises(backend_lifecycle.LaunchRefused, match="relay limit") as excinfo:
    await worker.run()

  assert excinfo.value.quota_exhausted is False
  assert len(builds) == 1 + claude_relay.MAX_RELAYS_PER_TURN
  assert worker.account_relays == claude_relay.MAX_RELAYS_PER_TURN


@pytest.mark.asyncio
async def test_worker_login_failure_marks_the_account_and_notifies_the_session(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = fable_pool_cfg(tmp_path)
  make_transcript(tmp_path / "claude-main", CC_ID)
  first = ScriptedRelayBackend([assistant_text_event("Failed to authenticate. Please run /login")], exit_code=1)
  second = ScriptedRelayBackend([_result()], exit_code=0)
  builds = _install_backends(monkeypatch, [first, second])
  worker = _worker(tmp_path, cfg)
  worker.on_session_event = AsyncMock()

  exit_code = await worker.run()

  assert exit_code == 0
  assert not claude_accounts.healthy(claude_accounts.account_by_label(cfg, "main"))
  notice = worker.on_session_event.await_args.args[0]
  assert notice["type"] == claude_relay.CLAUDE_ACCOUNT_LOGIN_REQUIRED
  assert (notice["account"], notice["reason"]) == ("main", "auth_failed")
  assert any(ev["type"] == claude_relay.CLAUDE_ACCOUNT_LOGIN_REQUIRED for ev in _logged_events(tmp_path))
  assert builds[1]["kwargs"]["claude_account"].label == "ext-1"


@pytest.mark.asyncio
@pytest.mark.parametrize("prompt_tokens", [150_000, 20_000])
async def test_worker_relay_does_not_compact_the_task_context(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, prompt_tokens: int) -> None:
  cfg = fable_pool_cfg(tmp_path)
  make_transcript(tmp_path / "claude-main", CC_ID)
  compact = AsyncMock()
  monkeypatch.setattr(claude_compaction, "compact_with_sonnet", compact)
  big = assistant_text_event("big")
  big["message"]["usage"] = {"input_tokens": 10, "cache_read_input_tokens": prompt_tokens - 10}
  first = ScriptedRelayBackend([big, rate_limit_event("rejected", 1.0)], exit_code=1)
  second = ScriptedRelayBackend([_result()], exit_code=0)
  _install_backends(monkeypatch, [first, second])
  worker = _worker(tmp_path, cfg)

  await worker.run()

  compact.assert_not_awaited()
