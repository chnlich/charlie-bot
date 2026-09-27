"""Worker account relay: a delegated task keeps running when its pool login runs out of quota.

Covers the pool pin at construction (spawner_launch._construct_worker), the Worker's own
relay loop (src/agents/worker.py run/_relay), the disposition of an exhausted pool through
_stream_worker_events and spawn_worker, and the completion notice carrying the reset time.
"""

import json
from pathlib import Path

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

from src.agents.worker import Worker
from src.core import (
    claude_accounts,
    claude_relay,
)
from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.models import ThreadMetadata

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


def _worker(tmp_path: Path, cfg: CharlieBotConfig, label: str | None) -> Worker:
  option = cfg.get_backend_option(POOLED_FABLE_ID)
  account = claude_accounts.account_by_label(cfg, label) if label else None
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


def test_pin_pool_account_picks_the_most_headroom_and_leaves_the_option_unchanged(tmp_path: Path) -> None:
  cfg = fable_pool_cfg(tmp_path)
  claude_accounts.observe_rate_limit("main", rate_limit_event("allowed_warning", 0.95)["rate_limit_info"])
  claude_accounts.observe_rate_limit("ext-1", rate_limit_event("allowed", 0.20)["rate_limit_info"])
  claude_accounts.observe_rate_limit("ext-2", rate_limit_event("allowed", 0.60)["rate_limit_info"])
  pooled = cfg.get_backend_option(POOLED_FABLE_ID)

  option, account = claude_relay.pin_pool_account(cfg, pooled)

  assert option is pooled
  assert (account.label, account.config_dir) == ("ext-1", str(tmp_path / "claude-ext-1"))


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
  worker = _worker(tmp_path, cfg, "main")

  exit_code = await worker.run()

  assert exit_code == 0
  assert builds[1]["kwargs"]["claude_account"].config_dir == str(tmp_path / "claude-ext-1")
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
async def test_worker_raises_pool_exhausted_when_no_account_is_left(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = fable_pool_cfg(tmp_path, labels=("main", "ext-1"))
  make_transcript(tmp_path / "claude-main", CC_ID)
  _reject("ext-1")
  _install_backends(monkeypatch, [ScriptedRelayBackend([rate_limit_event("rejected", 1.0)], exit_code=1)])
  worker = _worker(tmp_path, cfg, "main")

  with pytest.raises(claude_relay.PoolExhaustedError, match="earliest reset"):
    await worker.run()


# ---------------------------------------------------------------------------
# Disposition of an exhausted pool
