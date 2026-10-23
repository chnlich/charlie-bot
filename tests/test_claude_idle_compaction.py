"""Idle compaction of a cc-claude master session: the trigger decision, the command line parity with the
master turn, the timer against the turn path, and the run the trigger path starts."""

import asyncio
import dataclasses
import datetime
import json
import pathlib
from collections.abc import Iterator
from typing import Any

import conftest
import pytest

from src.backends.claude_code import (
    claude_compaction,
    claude_config,
    claude_idle_compaction,
    claude_lifecycle,
    claude_metadata,
    master_cc_relay,
)
from src.infra import config, models
from src.runtime import master_cc_run, master_cc_state
from src.runtime.hooks import backend_lifecycle, backend_types

SESSION_ID = "idle-session"
CC_ID = "11111111-2222-3333-4444-555555555555"
ACCOUNT_LABEL = "acct-a"
IDLE_TOKENS = 100_000
NOW = datetime.datetime(2026, 9, 6, 20, 0, tzinfo=datetime.UTC)
LAST_REQUEST = NOW - datetime.timedelta(minutes=55)
BASE_FACTS = claude_idle_compaction.IdleFacts(
    archived=False,
    backend_unchanged=True,
    turn_busy=False,
    armed_at=LAST_REQUEST,
    last_request_at=LAST_REQUEST,
    compacted_after_request=False,
    context_tokens=IDLE_TOKENS,
    resume_id=CC_ID,
    transcript_present=True,
)


def _option() -> models.BackendOption:
  return conftest.backend_option(id="cc-fable", label="Fable", type="cc-claude", model="claude-fable-5-1")


def _cfg(tmp_path: pathlib.Path, option: models.BackendOption) -> config.CharlieBotConfig:
  account = claude_config.ClaudeAccount(label=ACCOUNT_LABEL, config_dir=str(tmp_path / "acct-a"))
  return config.CharlieBotConfig(
      charliebot_home=tmp_path / "home",
      backends={"options": [option]},
      accounts={"claude": [account]},
  )


def _round_ctx(
    tmp_path: pathlib.Path,
    option: models.BackendOption,
    *,
    last_request_at: datetime.datetime | None,
    context_tokens: int | None = IDLE_TOKENS,
    held_native_id: str | None = CC_ID,
) -> backend_lifecycle.LaunchContext:
  """The context a master round of a session on *option* carries: its reading and its last request."""
  meta = models.SessionMetadata(
      profile="manager", id=SESSION_ID, name="Idle", backend=option.id, cc_session_id=held_native_id)
  claude_metadata.set_account(meta, ACCOUNT_LABEL)

  async def context_state() -> tuple[int | None, datetime.datetime | None]:
    return context_tokens, last_request_at

  async def emit(event: dict) -> None:
    raise AssertionError(f"no chat event expected in this test: {event}")

  async def record_account(label: str) -> None:
    raise AssertionError(f"no account write expected in this test: {label}")

  return backend_lifecycle.LaunchContext(
      cfg=_cfg(tmp_path, option),
      option=option,
      session_meta=meta,
      kind="turn",
      run_kind="manager_turn",
      cwd=str(tmp_path / "session"),
      held_native_id=held_native_id,
      preassigned_native_id=None,
      emit=emit,
      record_account=record_account,
      context_state=context_state,
      run_id="11111111-2222-3333-4444-555555555555",
  )


class _FakeStore:
  """The metadata store the trigger path reads: one session, read fresh."""

  def __init__(self, meta: models.SessionMetadata) -> None:
    self._meta = meta

  async def read_metadata_fresh(self, session_id: str) -> models.SessionMetadata | None:
    return self._meta


class _FakeEvents:
  """The chat events the trigger path reads: none, so no compaction follows any request."""

  def load_chat_events_sync(self, session_id: str) -> list[dict]:
    return []


def _patch_session_world(monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path, option: models.BackendOption) -> None:
  """A pooled session whose transcript sits in its account directory, as the trigger path finds it."""
  cfg = _cfg(tmp_path, option)
  meta = models.SessionMetadata(profile="manager", id=SESSION_ID, name="Idle", backend=option.id, cc_session_id=CC_ID)
  claude_metadata.set_account(meta, ACCOUNT_LABEL)
  conftest.make_transcript(pathlib.Path(cfg.accounts.claude[0].config_dir), CC_ID)
  monkeypatch.setattr(conftest.CONFIG_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  monkeypatch.setattr(claude_idle_compaction.session_store, "store", lambda: _FakeStore(meta))
  monkeypatch.setattr(claude_idle_compaction.session_events, "events", _FakeEvents)


@pytest.fixture(autouse=True)
def _fresh_idle_state() -> Iterator[None]:
  """Cancel the timers a test armed and clear the tables, so no timer outlives its test."""

  def _reset() -> None:
    for timer in claude_idle_compaction._timers.values():
      timer.task.cancel()
    claude_idle_compaction._timers.clear()
    claude_idle_compaction._running.clear()
    master_cc_state._session_queues.clear()
    master_cc_state._current_items.clear()

  _reset()
  yield
  _reset()


# ---------------------------------------------------------------------------
# The trigger decision (pure)
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "override",
    [
        pytest.param({"archived": True}, id="archived"),
        pytest.param({"backend_unchanged": False}, id="backend_switched"),
        pytest.param({"turn_busy": True}, id="turn_running_or_queued"),
        pytest.param({"armed_at": LAST_REQUEST - datetime.timedelta(minutes=1)}, id="armed_for_another_request"),
        pytest.param({
            "last_request_at": None,
            "armed_at": None
        }, id="no_request"),
        pytest.param(
            {
                "last_request_at": NOW - datetime.timedelta(minutes=60),
                "armed_at": NOW - datetime.timedelta(minutes=60)
            },
            id="cache_expired",
        ),
        pytest.param({"compacted_after_request": True}, id="compacted_after_request"),
        pytest.param({"context_tokens": IDLE_TOKENS - 1}, id="context_below_floor"),
        pytest.param({"context_tokens": None}, id="no_context_reading"),
        pytest.param({"resume_id": None}, id="no_resume_id"),
        pytest.param({"transcript_present": False}, id="transcript_missing"),
    ],
)
def test_each_condition_alone_false_gives_no_compaction(override: dict[str, Any]) -> None:
  facts = dataclasses.replace(BASE_FACTS, **override)

  assert claude_idle_compaction.should_compact(facts, idle_tokens=IDLE_TOKENS, now=NOW) is False


def test_every_condition_true_gives_a_compaction() -> None:
  assert claude_idle_compaction.should_compact(BASE_FACTS, idle_tokens=IDLE_TOKENS, now=NOW) is True


# ---------------------------------------------------------------------------
# Command line parity with the master turn
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_idle_argv_equals_the_master_turn_argv_for_the_same_option_and_account(tmp_path: pathlib.Path) -> None:
  option = _option()
  cfg = _cfg(tmp_path, option)
  account = cfg.accounts.claude[0]
  meta = models.SessionMetadata(profile="manager", id=SESSION_ID, name="Idle", backend=option.id, cc_session_id=CC_ID)
  item = conftest.make_work_item(cfg, meta, option)
  ctx = master_cc_run._turn_launch_context(item, option, str(tmp_path / "session"), CC_ID)

  # The master turn path, as _run_cc builds one process of a turn.
  launch = claude_lifecycle.ClaudeCodeLifecycle()._pooled_launch(ctx, account, resume_id=CC_ID, prompt=None, relays=0)
  spawn_flags, spawn_resume_id = master_cc_run._build_extra_flags(option, launch, item)
  launch_kwargs = {key: value for key, value in launch.backend_kwargs.items() if key != "extra_flags"}
  turn_backend = backend_types.build_backend(
      option,
      cfg,
      **launch_kwargs,
      extra_flags=spawn_flags or None,
      buffer_limit=cfg.subprocess_buffer_limit,
      instructions_content=item.task_instructions,
      resume_session_id=spawn_resume_id,
      log_dir=tmp_path / "run",
      cgroup_session_id=SESSION_ID,
  )
  expected = turn_backend._build_command(item.user_content)

  argv, _env = claude_idle_compaction._compaction_process(cfg, option, account, SESSION_ID, CC_ID, ctx.run_kind)

  format_index = expected.index("--input-format")
  assert expected[format_index:format_index + 3] == ["--input-format", "stream-json", "--replay-user-messages"]
  assert "--input-format" not in argv
  assert argv == expected[:format_index] + expected[format_index + 3:]
  settings = json.loads(argv[argv.index("--settings") + 1])
  [session_start] = settings["hooks"]["SessionStart"]
  assert session_start["matcher"] == "compact"
  assert [hook["command"] for hook in session_start["hooks"]
         ] == [f"charliebot compaction-context --session {SESSION_ID} --slot {slot}" for slot in range(1, 7)]


# ---------------------------------------------------------------------------
# Timing against the master turn path
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_a_second_arm_leaves_one_timer(tmp_path: pathlib.Path) -> None:
  option = _option()
  last = models.utc_now()
  await claude_idle_compaction.arm(_round_ctx(tmp_path, option, last_request_at=last))
  first_task = claude_idle_compaction._timers[SESSION_ID].task

  await claude_idle_compaction.arm(_round_ctx(tmp_path, option, last_request_at=last))

  assert list(claude_idle_compaction._timers) == [SESSION_ID]
  assert claude_idle_compaction._timers[SESSION_ID].task is not first_task
  await asyncio.sleep(0)
  assert first_task.cancelled()


@pytest.mark.asyncio
async def test_a_turn_start_cancels_the_pending_timer(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  option = _option()
  ctx = _round_ctx(tmp_path, option, last_request_at=models.utc_now(), held_native_id=None)
  await claude_idle_compaction.arm(ctx)
  timer_task = claude_idle_compaction._timers[SESSION_ID].task
  account = ctx.cfg.accounts.claude[0]

  async def place_turn(*args: Any, **kwargs: Any) -> claude_config.ClaudeAccount:
    return account

  monkeypatch.setattr(master_cc_relay, "place_turn", place_turn)
  await claude_lifecycle.ClaudeCodeLifecycle()._place_turn(ctx)

  assert SESSION_ID not in claude_idle_compaction._timers
  await asyncio.sleep(0)
  assert timer_task.cancelled()


@pytest.mark.asyncio
async def test_place_turn_waits_for_a_running_idle_compaction(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  option = _option()
  ctx = _round_ctx(tmp_path, option, last_request_at=models.utc_now(), held_native_id=None)
  account = ctx.cfg.accounts.claude[0]
  release = asyncio.Event()
  claude_idle_compaction._running[SESSION_ID] = asyncio.create_task(release.wait())

  async def place_turn(*args: Any, **kwargs: Any) -> claude_config.ClaudeAccount:
    return account

  monkeypatch.setattr(master_cc_relay, "place_turn", place_turn)
  placed = asyncio.create_task(claude_lifecycle.ClaudeCodeLifecycle()._place_turn(ctx))
  for _ in range(3):
    await asyncio.sleep(0)
  assert not placed.done()

  release.set()
  await placed

  assert claude_idle_compaction._running[SESSION_ID].done()


@pytest.mark.asyncio
async def test_a_turn_queued_before_the_trigger_registers_makes_it_skip(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  option = _option()
  last = models.utc_now()
  _patch_session_world(monkeypatch, tmp_path, option)
  await claude_idle_compaction.arm(_round_ctx(tmp_path, option, last_request_at=last, context_tokens=120_000))
  master_cc_state._session_queues[SESSION_ID] = asyncio.Queue()
  master_cc_state._session_queues[SESSION_ID].put_nowait(object())
  runs: list[dict[str, Any]] = []

  async def run_compaction(**kwargs: Any) -> claude_compaction.CompactionOutcome:
    runs.append(kwargs)
    return claude_compaction.CompactionOutcome(ok=True)

  monkeypatch.setattr(claude_compaction, "run_compaction", run_compaction)

  started = await claude_idle_compaction.fire(SESSION_ID, now=last + datetime.timedelta(minutes=55))

  assert started is None
  assert runs == []
  assert not claude_idle_compaction._running


# ---------------------------------------------------------------------------
# The run the trigger path starts
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_the_trigger_path_starts_one_idle_run_on_the_sessions_own_command_line(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  option = _option()
  last = models.utc_now()
  _patch_session_world(monkeypatch, tmp_path, option)
  await claude_idle_compaction.arm(_round_ctx(tmp_path, option, last_request_at=last, context_tokens=120_000))
  runs: list[dict[str, Any]] = []

  async def run_compaction(**kwargs: Any) -> claude_compaction.CompactionOutcome:
    runs.append(kwargs)
    return claude_compaction.CompactionOutcome(ok=True, post_tokens=9_111, cache_read_tokens=118_000)

  monkeypatch.setattr(claude_compaction, "run_compaction", run_compaction)

  started = await claude_idle_compaction.fire(SESSION_ID, now=last + datetime.timedelta(minutes=55))
  assert started is not None
  await started

  [run] = runs
  cfg = _cfg(tmp_path, option)
  argv, _env = claude_idle_compaction._compaction_process(
      cfg, option, cfg.accounts.claude[0], SESSION_ID, CC_ID, "manager_turn")
  assert run["argv"] == argv
  assert run["cwd"] == str(tmp_path / "session")
  assert run["config_dir"] == cfg.accounts.claude[0].config_dir
  assert run["pre_tokens"] == 120_000
  assert (run["trigger"], run["failure_trigger"]) == ("idle", "idle")
  assert (run["model"], run["family"]) == ("claude-fable-5-1", "fable")
  assert claude_idle_compaction._running == {}


@pytest.mark.asyncio
async def test_an_unpooled_session_uses_its_default_login_directory(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  option = _option()
  cfg = config.CharlieBotConfig(
      charliebot_home=tmp_path / "home",
      backends={"options": [option]},
  )
  default_dir = tmp_path / "default-claude"
  conftest.make_transcript(default_dir, CC_ID)
  monkeypatch.setattr(claude_idle_compaction.login_dirs, "claude_config_dir", lambda: default_dir)
  monkeypatch.setattr(conftest.CONFIG_GET_CONFIG_PATCH_TARGET, lambda: cfg)

  ctx = _round_ctx(tmp_path, option, last_request_at=LAST_REQUEST, context_tokens=120_000)
  claude_metadata.set_account(ctx.session_meta, None)
  meta = ctx.session_meta
  monkeypatch.setattr(claude_idle_compaction.session_store, "store", lambda: _FakeStore(meta))
  monkeypatch.setattr(claude_idle_compaction.session_events, "events", _FakeEvents)
  runs: list[dict[str, Any]] = []

  async def run_compaction(**kwargs: Any) -> claude_compaction.CompactionOutcome:
    runs.append(kwargs)
    return claude_compaction.CompactionOutcome(ok=True, post_tokens=9_111, cache_read_tokens=118_000)

  monkeypatch.setattr(claude_compaction, "run_compaction", run_compaction)
  await claude_idle_compaction.arm(ctx)

  started = await claude_idle_compaction.fire(SESSION_ID, now=LAST_REQUEST + datetime.timedelta(minutes=55))
  assert started is not None
  await started

  [run] = runs
  assert run["config_dir"] == str(default_dir)
  assert run["env"][config.CLAUDE_CONFIG_DIR_ENV_VAR] == str(default_dir)
  assert run["log_context"]["account"] == "default"
