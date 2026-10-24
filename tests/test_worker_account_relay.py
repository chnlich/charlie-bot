"""Worker account relay: a delegated task keeps running when its pool login runs out of quota.

Covers the Worker's task run on the shared launch loop (src/runtime/worker.py run, the Claude
lifecycle in src/backends/claude_code/claude_lifecycle.py): placement on the pool account with the
most headroom, rejected-run relay onto another account, the safe-point termination after a far
warning, the relay limit, login-failure marking, and the task relay's no-compaction behavior.
"""

import datetime
import json
import pathlib
from unittest import mock

import conftest
import pytest

from src.backends.claude_code import claude_accounts, claude_compaction, claude_relay
from src.infra import config, models
from src.infra import event_types as ET
from src.runtime import worker
from src.runtime.hooks import backend_lifecycle

CC_ID = "11111111-2222-3333-4444-555555555555"
NOW = datetime.datetime(2026, 9, 6, 20, 0, tzinfo=datetime.UTC)

_fresh_pool_state = conftest.fresh_state_fixture(conftest.reset_claude_account_readings)


def _result() -> dict:
  return {"type": ET.RESULT, "subtype": "success", "is_error": False, "result": "done"}


def _install_backends(monkeypatch: pytest.MonkeyPatch, backends: list[conftest.ScriptedRelayBackend]) -> list[dict]:
  return conftest.install_scripted_backends(monkeypatch, backends, conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)


def _thread() -> models.ThreadMetadata:
  return models.ThreadMetadata(
      id="t1",
      session_id="s1",
      description="task",
      backend=conftest.POOLED_FABLE_ID,
      model=conftest.FABLE_MODEL,
      claude_session_id=CC_ID)


def _worker(tmp_path: pathlib.Path, cfg: config.CharlieBotConfig) -> worker.Worker:
  option = cfg.get_backend_option(conftest.POOLED_FABLE_ID)
  return worker.Worker(
      _thread(),
      tmp_path / "work",
      tmp_path / "data" / "events.jsonl",
      "do the thing",
      cfg,
      backend_option=option,
      session_meta=models.SessionMetadata(profile="manager", id="s1", name="S", backend=conftest.POOLED_FABLE_ID),
      run_kind="work",
  )


def _logged_events(tmp_path: pathlib.Path) -> list[dict]:
  lines = (tmp_path / "data" / "events.jsonl").read_text(encoding="utf-8").splitlines()
  return [json.loads(line) for line in lines if line.strip()]


# ---------------------------------------------------------------------------
# The Worker's relay loop
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_worker_relays_a_rejected_run_onto_another_account(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = conftest.fable_pool_cfg(tmp_path)
  source_transcript = conftest.make_transcript(tmp_path / "claude-main", CC_ID)
  first = conftest.ScriptedRelayBackend(
      [conftest.assistant_text_event("working"),
       conftest.rate_limit_event("rejected", 1.0)], exit_code=1)
  second = conftest.ScriptedRelayBackend([conftest.assistant_text_event("done"), _result()], exit_code=0)
  builds = _install_backends(monkeypatch, [first, second])
  task_worker = _worker(tmp_path, cfg)

  exit_code = await task_worker.run()

  assert exit_code == 0
  assert builds[1]["kwargs"]["claude_account"].config_dir == str(tmp_path / "claude-ext-1")
  assert builds[1]["kwargs"]["extra_flags"] == ["--resume", CC_ID]
  assert "claude_session_id" not in builds[1]["kwargs"]
  assert builds[0]["kwargs"]["claude_session_id"] == CC_ID
  assert second.prompt == claude_relay.CONTINUATION_PROMPT
  assert (tmp_path / "claude-ext-1" / "projects" / source_transcript.parent.name / f"{CC_ID}.jsonl").exists()
  assert (builds[1]["kwargs"]["claude_account"].label, task_worker.account_relays) == ("ext-1", 1)
  logged = _logged_events(tmp_path)
  assert [ev["type"] for ev in logged if ev["type"] == ET.ASSISTANT] == [ET.ASSISTANT, ET.ASSISTANT]
  assert any(ev["type"] == ET.RATE_LIMIT_EVENT for ev in logged)


@pytest.mark.asyncio
async def test_worker_raises_pool_exhausted_when_no_account_is_left(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = conftest.fable_pool_cfg(tmp_path, labels=("main", "ext-1"))
  conftest.make_transcript(tmp_path / "claude-main", CC_ID)
  conftest.reject_claude_login("ext-1")
  _install_backends(
      monkeypatch, [conftest.ScriptedRelayBackend([conftest.rate_limit_event("rejected", 1.0)], exit_code=1)])
  task_worker = _worker(tmp_path, cfg)

  with pytest.raises(backend_lifecycle.LaunchRefused, match="earliest reset") as excinfo:
    await task_worker.run()

  assert excinfo.value.quota_exhausted is True


@pytest.mark.asyncio
async def test_worker_relay_stays_inside_the_option_pool(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The relay picks the next account inside the option's pool: a healthy account in another
  pool, with more headroom than every pool member, stays unused."""
  cfg = conftest.fable_pool_cfg(
      tmp_path, labels=("main", "ext-1", "ext-2"), claude_pools={
          "alpha": ["main", "ext-1"],
          "beta": ["ext-2"]
      })
  source_transcript = conftest.make_transcript(tmp_path / "claude-main", CC_ID)
  # ext-1 sits at half a window; untouched ext-2 (beta) has the most headroom of all.
  claude_accounts.observe_rate_limit("ext-1", conftest.rate_limit_event("allowed", 0.50)["rate_limit_info"])
  first = conftest.ScriptedRelayBackend([conftest.rate_limit_event("rejected", 1.0)], exit_code=1)
  second = conftest.ScriptedRelayBackend([conftest.assistant_text_event("done"), _result()], exit_code=0)
  builds = _install_backends(monkeypatch, [first, second])
  task_worker = _worker(tmp_path, cfg)

  exit_code = await task_worker.run()

  assert exit_code == 0
  assert builds[1]["kwargs"]["claude_account"].config_dir == str(tmp_path / "claude-ext-1")
  assert (tmp_path / "claude-ext-1" / "projects" / source_transcript.parent.name / f"{CC_ID}.jsonl").exists()
  assert claude_accounts.transcript_path(tmp_path / "claude-ext-2", CC_ID) is None


@pytest.mark.asyncio
async def test_worker_pool_exhaustion_names_the_option_pool(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """With every account of the option's pool rejected, the worker ends loudly naming the pool
  and its earliest reset; the other pool's healthy accounts stay unused."""
  cfg = conftest.fable_pool_cfg(
      tmp_path, labels=("main", "ext-1", "ext-2"), claude_pools={
          "alpha": ["main"],
          "beta": ["ext-1", "ext-2"]
      })
  conftest.make_transcript(tmp_path / "claude-main", CC_ID)
  _install_backends(
      monkeypatch, [conftest.ScriptedRelayBackend([conftest.rate_limit_event("rejected", 1.0)], exit_code=1)])
  task_worker = _worker(tmp_path, cfg)

  with pytest.raises(backend_lifecycle.LaunchRefused) as excinfo:
    await task_worker.run()

  message = str(excinfo.value)
  assert claude_relay.POOL_EXHAUSTED_PHRASE in message
  assert "'alpha'" in message and "earliest reset" in message and "UTC" in message
  assert claude_accounts.transcript_path(tmp_path / "claude-ext-1", CC_ID) is None
  assert claude_accounts.transcript_path(tmp_path / "claude-ext-2", CC_ID) is None


@pytest.mark.asyncio
async def test_worker_stops_after_the_relay_limit(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = conftest.fable_pool_cfg(tmp_path, labels=("main", "a", "b", "c"))
  conftest.make_transcript(tmp_path / "claude-main", CC_ID)
  backends = [
      conftest.ScriptedRelayBackend([conftest.rate_limit_event("rejected", 1.0)], exit_code=1) for _ in range(4)
  ]
  builds = _install_backends(monkeypatch, backends)
  task_worker = _worker(tmp_path, cfg)

  with pytest.raises(backend_lifecycle.LaunchRefused, match="relay limit") as excinfo:
    await task_worker.run()

  assert excinfo.value.quota_exhausted is False
  assert len(builds) == 1 + claude_relay.MAX_RELAYS_PER_TURN
  assert task_worker.account_relays == claude_relay.MAX_RELAYS_PER_TURN


# ---------------------------------------------------------------------------
# The armed safe point's stay rule
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_worker_stays_armed_without_a_below_line_account_and_completes(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A pool with no account strictly below the warning line keeps the task going at the
  safe point: no termination, no relay, and both safe points let the task complete on
  the same account."""
  cfg = conftest.fable_pool_cfg(tmp_path, labels=("main",))
  conftest.make_transcript(tmp_path / "claude-main", CC_ID)
  first = conftest.ScriptedRelayBackend(
      [
          conftest.assistant_text_event("working"),
          conftest.rate_limit_event("allowed_warning", 0.92),
          conftest.user_tool_result_event(),
          conftest.assistant_text_event("still working"),
          conftest.user_tool_result_event(),
          conftest.assistant_text_event("done"),
          _result(),
      ],
      exit_code=0)
  builds = _install_backends(monkeypatch, [first])
  task_worker = _worker(tmp_path, cfg)

  exit_code = await task_worker.run()

  assert exit_code == 0
  assert first.terminated is False
  assert len(builds) == 1
  assert task_worker.account_relays == 0


@pytest.mark.asyncio
async def test_worker_warning_relay_lands_strictly_below_the_line_in_the_tie_band(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The gate and the move share the below-line bar: an at-line account that an
  unrestricted select would reach through the tie band -- its score sits within
  ``_SCORE_TIE`` of the best and its reading is the older -- stays out, and the
  landing is strictly below the line."""
  cfg = conftest.fable_pool_cfg(tmp_path)
  conftest.make_transcript(tmp_path / "claude-main", CC_ID)
  # ext-1 sits below the line (headroom 0.11); ext-2 sits at the line (headroom 0.095)
  # with the older reading: a min_headroom=0 select ties the two (0.015 apart) and would
  # land ext-2, re-arming at the next safe point.
  claude_accounts.observe_rate_limit(
      "ext-1", conftest.rate_limit_event("allowed", 0.89)["rate_limit_info"], now=NOW - datetime.timedelta(minutes=5))
  claude_accounts.observe_rate_limit(
      "ext-2", conftest.rate_limit_event("allowed", 0.905)["rate_limit_info"], now=NOW - datetime.timedelta(minutes=30))
  first = conftest.ScriptedRelayBackend(
      [conftest.rate_limit_event("allowed_warning", 0.92),
       conftest.user_tool_result_event()], exit_code=0)
  second = conftest.ScriptedRelayBackend([_result()], exit_code=0)
  builds = _install_backends(monkeypatch, [first, second])
  task_worker = _worker(tmp_path, cfg)

  exit_code = await task_worker.run()

  assert exit_code == 0
  assert first.terminated is True
  assert (builds[1]["kwargs"]["claude_account"].label, task_worker.account_relays) == ("ext-1", 1)


@pytest.mark.asyncio
async def test_worker_falls_back_to_the_regular_select_when_the_below_line_target_vanishes(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The gate and the move read the pool at different moments: a target that rose to the
  line in between lands the regular select -- possibly at or above the line -- and the
  run continues instead of refusing."""
  cfg = conftest.fable_pool_cfg(tmp_path)
  conftest.make_transcript(tmp_path / "claude-main", CC_ID)
  # ext-1 is the gate's below-line destination; ext-2 already sits at the line.
  claude_accounts.observe_rate_limit("ext-1", conftest.rate_limit_event("allowed", 0.85)["rate_limit_info"], now=NOW)
  claude_accounts.observe_rate_limit("ext-2", conftest.rate_limit_event("allowed", 0.92)["rate_limit_info"], now=NOW)
  conftest.patch_select_with_transient_reading(monkeypatch, NOW)
  first = conftest.ScriptedRelayBackend(
      [conftest.rate_limit_event("allowed_warning", 0.92),
       conftest.user_tool_result_event()], exit_code=0)
  second = conftest.ScriptedRelayBackend([_result()], exit_code=0)
  builds = _install_backends(monkeypatch, [first, second])
  task_worker = _worker(tmp_path, cfg)

  exit_code = await task_worker.run()

  assert exit_code == 0
  assert first.terminated is True
  # The regular select lands ext-2 -- at the line, headroom 0.08 over ext-1's 0.05.
  assert (builds[1]["kwargs"]["claude_account"].label, task_worker.account_relays) == ("ext-2", 1)


@pytest.mark.asyncio
async def test_worker_warning_relays_past_the_cap_with_below_line_destinations(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A warning relay with a strictly-below-line destination is exempt from the relay cap:
  the fourth relay proceeds and the task completes on the fifth account."""
  cfg = conftest.fable_pool_cfg(tmp_path, labels=("main", "a", "b", "c", "d"))
  conftest.make_transcript(tmp_path / "claude-main", CC_ID)
  backends = [
      conftest.ScriptedRelayBackend(
          [conftest.rate_limit_event("allowed_warning", 0.92),
           conftest.user_tool_result_event()], exit_code=0) for _ in range(4)
  ]
  last = conftest.ScriptedRelayBackend([_result()], exit_code=0)
  builds = _install_backends(monkeypatch, [*backends, last])
  task_worker = _worker(tmp_path, cfg)

  exit_code = await task_worker.run()

  assert exit_code == 0
  assert [b["kwargs"]["claude_account"].label for b in builds] == ["main", "a", "b", "c", "d"]
  assert task_worker.account_relays == claude_relay.MAX_RELAYS_PER_TURN + 1


@pytest.mark.asyncio
async def test_worker_relay_limit_names_each_unavailable_account(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """With the relay count exhausted and no below-line destination -- every login
  authentication-failed, so no rejection reset exists either -- the cap error lists each
  account's cooldown."""
  cfg = conftest.fable_pool_cfg(tmp_path, labels=("main", "a", "b", "c"))
  conftest.make_transcript(tmp_path / "claude-main", CC_ID)
  failed = [
      conftest.ScriptedRelayBackend(
          [conftest.assistant_text_event("Failed to authenticate. Please run /login")], exit_code=1) for _ in range(4)
  ]
  builds = _install_backends(monkeypatch, failed)
  task_worker = _worker(tmp_path, cfg)
  task_worker.on_session_event = mock.AsyncMock()

  with pytest.raises(backend_lifecycle.LaunchRefused, match="relay limit") as excinfo:
    await task_worker.run()

  message = str(excinfo.value)
  assert excinfo.value.quota_exhausted is False
  assert message.startswith("Claude account relay limit reached (3 relays in one run; ")
  assert message.endswith("this run did not complete.")
  for label in ("main", "a", "b", "c"):
    assert f"{label}: auth-failure cooldown until" in message
  assert "UTC" in message
  assert len(builds) == 1 + claude_relay.MAX_RELAYS_PER_TURN


@pytest.mark.asyncio
async def test_worker_login_failure_marks_the_account_and_notifies_the_session(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = conftest.fable_pool_cfg(tmp_path)
  conftest.make_transcript(tmp_path / "claude-main", CC_ID)
  first = conftest.ScriptedRelayBackend(
      [conftest.assistant_text_event("Failed to authenticate. Please run /login")], exit_code=1)
  second = conftest.ScriptedRelayBackend([_result()], exit_code=0)
  builds = _install_backends(monkeypatch, [first, second])
  task_worker = _worker(tmp_path, cfg)
  task_worker.on_session_event = mock.AsyncMock()

  exit_code = await task_worker.run()

  assert exit_code == 0
  assert not claude_accounts.healthy(claude_accounts.account_by_label(cfg, "main"))
  notice = task_worker.on_session_event.await_args.args[0]
  assert notice["type"] == claude_relay.event_types.CLAUDE_ACCOUNT_LOGIN_REQUIRED
  assert (notice["account"], notice["reason"]) == ("main", "auth_failed")
  assert any(ev["type"] == claude_relay.event_types.CLAUDE_ACCOUNT_LOGIN_REQUIRED for ev in _logged_events(tmp_path))
  assert builds[1]["kwargs"]["claude_account"].label == "ext-1"


@pytest.mark.asyncio
@pytest.mark.parametrize("prompt_tokens", [150_000, 20_000])
async def test_worker_relay_does_not_compact_the_task_context(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, prompt_tokens: int) -> None:
  cfg = conftest.fable_pool_cfg(tmp_path)
  conftest.make_transcript(tmp_path / "claude-main", CC_ID)
  compact = mock.AsyncMock()
  monkeypatch.setattr(claude_compaction, "compact_with_sonnet", compact)
  big = conftest.assistant_text_event("big")
  big["message"]["usage"] = {"input_tokens": 10, "cache_read_input_tokens": prompt_tokens - 10}
  first = conftest.ScriptedRelayBackend([big, conftest.rate_limit_event("rejected", 1.0)], exit_code=1)
  second = conftest.ScriptedRelayBackend([_result()], exit_code=0)
  _install_backends(monkeypatch, [first, second])
  task_worker = _worker(tmp_path, cfg)

  await task_worker.run()

  compact.assert_not_awaited()
