"""Master account relay: turn placement, the in-run watch, and _run_cc continuing a turn on another pool account."""

import datetime
import pathlib
import types

import conftest
import pytest

from src.backends.claude_code import claude_accounts, claude_relay, master_cc_relay
from src.infra import event_types as ET
from src.infra import models
from src.runtime import master_cc_run
from src.runtime import master_cc_state
from src.runtime.agent_process import base as backend_base

NOW = datetime.datetime(2026, 9, 6, 20, 0, tzinfo=datetime.UTC)
UUID = "uuid-relay-1"

_fresh_pool_state = conftest.fresh_state_fixture(claude_accounts.reset_for_tests)


def _install_backends(monkeypatch: pytest.MonkeyPatch, backends: list[conftest.ScriptedRelayBackend]) -> list[dict]:
  # The master-cc run path re-imports build_backend through the registry on
  # every call, so the patch lands there; the instructions builder is stubbed
  # with it because _run_cc builds instructions before the first backend build.
  return conftest.install_scripted_backends(monkeypatch, backends, conftest.BUILD_BACKEND_PATCH_TARGET)


def _session_on(label: str | None, cc_session_id: str | None = UUID) -> models.SessionMetadata:
  return models.SessionMetadata(
      profile="manager",
      id="s1",
      name="t",
      backend=conftest.POOLED_FABLE_ID,
      cc_session_id=cc_session_id,
      claude_account=label)


def _events_of(callbacks: models.SessionCallbacks, event_type: str) -> list[dict]:
  return [
      call.args[1] for call in callbacks.persist_and_broadcast.await_args_list if call.args[1].get("type") == event_type
  ]


# ---------------------------------------------------------------------------
# Turn placement
# ---------------------------------------------------------------------------


def test_choose_turn_account_keeps_a_warm_healthy_account_under_the_warning_line(tmp_path: pathlib.Path) -> None:
  cfg = conftest.fable_pool_cfg(tmp_path)
  claude_accounts.observe_rate_limit("main", conftest.rate_limit_event("allowed", 0.50)["rate_limit_info"], now=NOW)
  meta = _session_on("main")

  chosen, cold = master_cc_relay.choose_turn_account(
      cfg, meta, conftest.FABLE_MODEL, NOW - datetime.timedelta(minutes=10), now=NOW)

  assert (chosen.label, cold) == ("main", False)


# ---------------------------------------------------------------------------
# Turn placement inside a named pool
# ---------------------------------------------------------------------------


def test_choose_turn_account_reselects_inside_the_pool_when_the_current_account_is_outside(
    tmp_path: pathlib.Path) -> None:
  """A warm, healthy, under-warning account outside the option's pool does not stick: the turn
  re-selects inside the pool."""
  cfg = conftest.fable_pool_cfg(tmp_path, claude_pools={"alpha": ["main"], "beta": ["ext-1", "ext-2"]})
  claude_accounts.observe_rate_limit("ext-1", conftest.rate_limit_event("allowed", 0.50)["rate_limit_info"], now=NOW)
  meta = _session_on("ext-1")

  chosen, cold = master_cc_relay.choose_turn_account(
      cfg, meta, conftest.FABLE_MODEL, NOW - datetime.timedelta(minutes=10), now=NOW, account_pool="alpha")

  assert (chosen.label, cold) == ("main", False)


def _hold_for_other_session(label: str, index: int) -> None:
  """Park a running work item on *label* for a session other than the test's own."""
  holder = models.SessionMetadata(
      profile="manager", id=f"other-{index}", name="t", backend=conftest.POOLED_FABLE_ID, claude_account=label)
  master_cc_state._current_items[holder.id] = types.SimpleNamespace(session_meta=holder)


def test_choose_turn_account_replays_the_2026_10_19_line_bounce(tmp_path: pathlib.Path) -> None:
  """Replay of 2026-10-19 23:23:37 UTC: with every under-line account busy and only the
  at-line ext-2 idle, the tier rule still lands the turn on the healthiest under-line
  account, ext-3, instead of bouncing ext-2 past 100 percent."""
  cfg = conftest.fable_pool_cfg(tmp_path, labels=("main", "ext-1", "ext-2", "ext-3", "ext-4", "invite-1"))
  readings = {"ext-3": 0.07, "invite-1": 0.23, "ext-4": 0.47, "ext-1": 0.73, "main": 0.96, "ext-2": 0.90}
  for label, utilization in readings.items():
    claude_accounts.observe_rate_limit(
        label, conftest.rate_limit_event("allowed", utilization)["rate_limit_info"], now=NOW)
  for index, label in enumerate(["ext-3", "invite-1", "ext-4", "ext-1", "main"]):
    _hold_for_other_session(label, index)
  meta = _session_on("ext-4")

  try:
    chosen, cold = master_cc_relay.choose_turn_account(
        cfg, meta, conftest.FABLE_MODEL, NOW - datetime.timedelta(minutes=10), now=NOW)
  finally:
    master_cc_state._current_items.clear()

  assert (chosen is not None and chosen.label, cold) == ("ext-3", False)


@pytest.mark.asyncio
async def test_place_turn_copies_the_transcript_into_the_option_pool(tmp_path: pathlib.Path) -> None:
  """A mid-session switch to another pool's option re-selects inside that pool and the existing
  move layer carries the transcript to the new account's directory."""
  cfg = conftest.fable_pool_cfg(tmp_path, claude_pools={"alpha": ["main"], "beta": ["ext-1", "ext-2"]})
  option = cfg.get_backend_option(conftest.POOLED_FABLE_ID)
  conftest.make_transcript(tmp_path / "claude-ext-1", UUID)
  meta = _session_on("ext-1")
  item = conftest.make_work_item(cfg, meta, option)

  ctx = master_cc_run._turn_launch_context(item, option, str(tmp_path / "work"), UUID)

  account = await master_cc_relay.place_turn(ctx, UUID, None, NOW - datetime.timedelta(minutes=10), now=NOW)

  assert account.label == "main"
  assert meta.claude_account == "main"
  assert claude_accounts.transcript_path(tmp_path / "claude-main", UUID) is not None


# ---------------------------------------------------------------------------
# _run_cc across accounts
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_run_cc_relays_a_rejected_turn_onto_another_account(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = conftest.fable_pool_cfg(tmp_path)
  conftest.make_transcript(tmp_path / "claude-main", UUID)
  meta = _session_on("main")
  first = conftest.ScriptedRelayBackend(
      [conftest.rate_limit_event("rejected", 1.0),
       backend_base.make_result_event()], exit_code=1)
  second = conftest.ScriptedRelayBackend([backend_base.make_result_event()], exit_code=0)
  builds = _install_backends(monkeypatch, [first, second])
  item = conftest.make_work_item(cfg, meta, cfg.backends.options[0])

  cc_session_id, exit_code, error_msg, extras = await master_cc_run._run_cc(item)

  assert (cc_session_id, exit_code, error_msg) == (UUID, 0, None)
  assert extras["account_relays"] == 1
  assert builds[0]["option"] is cfg.backends.options[0]
  assert builds[0]["kwargs"]["claude_account"].config_dir == str(tmp_path / "claude-main")
  assert builds[0]["kwargs"]["extra_flags"] == ["--resume", UUID, "--exclude-dynamic-system-prompt-sections"]
  assert first.env is not None and "CLAUDE_CONFIG_DIR" not in first.env
  assert builds[1]["kwargs"]["claude_account"].config_dir == str(tmp_path / "claude-ext-1")
  assert builds[1]["kwargs"]["extra_flags"] == ["--resume", UUID, "--exclude-dynamic-system-prompt-sections"]
  assert second.prompt == claude_relay.CONTINUATION_PROMPT
  assert meta.claude_account == "ext-1"
  assert claude_accounts.transcript_path(tmp_path / "claude-ext-1", UUID) is not None
  assert _events_of(item.callbacks, ET.ASSISTANT_ERROR) == []


@pytest.mark.asyncio
async def test_run_cc_terminates_at_the_safe_point_after_a_warning_and_relays(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = conftest.fable_pool_cfg(tmp_path)
  conftest.make_transcript(tmp_path / "claude-main", UUID)
  meta = _session_on("main")
  first = conftest.ScriptedRelayBackend(
      [
          conftest.rate_limit_event("allowed_warning", 0.92),
          conftest.assistant_text_event("step 1"),
          conftest.user_tool_result_event(),
          conftest.assistant_text_event("never streamed")
      ],
      exit_code=0)
  second = conftest.ScriptedRelayBackend([backend_base.make_result_event()], exit_code=0)
  builds = _install_backends(monkeypatch, [first, second])
  item = conftest.make_work_item(cfg, meta, cfg.backends.options[0])

  _cc, exit_code, error_msg, extras = await master_cc_run._run_cc(item)

  assert first.terminated is True
  assert (exit_code, error_msg, extras["account_relays"]) == (0, None, 1)
  assert len(builds) == 2
  assert builds[1]["kwargs"]["claude_account"].config_dir == str(tmp_path / "claude-ext-1")
  assert second.prompt == claude_relay.CONTINUATION_PROMPT


@pytest.mark.asyncio
async def test_run_cc_reports_loudly_when_no_account_is_left(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = conftest.fable_pool_cfg(tmp_path)
  conftest.write_pool_credentials(tmp_path / "claude-ext-1", access_token="")
  conftest.write_pool_credentials(tmp_path / "claude-ext-2", access_token="")
  conftest.make_transcript(tmp_path / "claude-main", UUID)
  meta = _session_on("main")
  first = conftest.ScriptedRelayBackend(
      [conftest.rate_limit_event("rejected", 1.0),
       backend_base.make_result_event()], exit_code=1)
  builds = _install_backends(monkeypatch, [first])
  item = conftest.make_work_item(cfg, meta, cfg.backends.options[0])

  _cc, exit_code, error_msg, _extras = await master_cc_run._run_cc(item)

  assert exit_code == 1
  assert "no available account" in error_msg and "UTC" in error_msg
  assert len(builds) == 1
  errors = _events_of(item.callbacks, ET.ASSISTANT_ERROR)
  assert len(errors) == 1 and "no available account" in errors[0]["content"]
  # Both emptied logins were reported once, without waiting for a run on them.
  notices = _events_of(item.callbacks, claude_relay.event_types.CLAUDE_ACCOUNT_LOGIN_REQUIRED)
  assert sorted(n["account"] for n in notices) == ["ext-1", "ext-2"]
  assert {n["reason"] for n in notices} == {"empty_credentials"}


# ---------------------------------------------------------------------------
# The armed safe point's stay rule
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_run_cc_stays_armed_without_a_below_line_account_and_completes(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A pool with no account strictly below the warning line keeps the run going at the
  safe point: no termination, no relay, and both safe points let the turn complete on
  the same account."""
  cfg = conftest.fable_pool_cfg(tmp_path, labels=("main",))
  conftest.make_transcript(tmp_path / "claude-main", UUID)
  meta = _session_on("main")
  first = conftest.ScriptedRelayBackend(
      [
          conftest.rate_limit_event("allowed_warning", 0.92),
          conftest.assistant_text_event("step 1"),
          conftest.user_tool_result_event(),
          conftest.assistant_text_event("step 2"),
          conftest.user_tool_result_event(),
          conftest.assistant_text_event("done"),
          backend_base.make_result_event(),
      ],
      exit_code=0)
  builds = _install_backends(monkeypatch, [first])
  item = conftest.make_work_item(cfg, meta, cfg.backends.options[0])

  _cc, exit_code, error_msg, extras = await master_cc_run._run_cc(item)

  assert first.terminated is False
  assert (exit_code, error_msg, extras.get("account_relays", 0)) == (0, None, 0)
  assert len(builds) == 1
  assert meta.claude_account == "main"


@pytest.mark.asyncio
async def test_run_cc_warning_relay_lands_strictly_below_the_line_in_the_tie_band(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The gate and the move share the below-line bar: an at-line account that an
  unrestricted select would reach through the tie band -- its score sits within
  ``_SCORE_TIE`` of the best and its reading is the older -- stays out, and the
  landing is strictly below the line."""
  cfg = conftest.fable_pool_cfg(tmp_path)
  conftest.make_transcript(tmp_path / "claude-main", UUID)
  meta = _session_on("main")
  # ext-1 sits below the line (headroom 0.11); ext-2 sits at the line (headroom 0.095)
  # with the older reading: a min_headroom=0 select ties the two (0.015 apart) and would
  # land ext-2, re-arming at the next safe point.
  claude_accounts.observe_rate_limit(
      "ext-1", conftest.rate_limit_event("allowed", 0.89)["rate_limit_info"], now=NOW - datetime.timedelta(minutes=5))
  claude_accounts.observe_rate_limit(
      "ext-2", conftest.rate_limit_event("allowed", 0.905)["rate_limit_info"], now=NOW - datetime.timedelta(minutes=30))
  first = conftest.ScriptedRelayBackend(
      [
          conftest.rate_limit_event("allowed_warning", 0.92),
          conftest.assistant_text_event("step 1"),
          conftest.user_tool_result_event(),
      ],
      exit_code=0)
  second = conftest.ScriptedRelayBackend([backend_base.make_result_event()], exit_code=0)
  builds = _install_backends(monkeypatch, [first, second])
  item = conftest.make_work_item(cfg, meta, cfg.backends.options[0])

  _cc, exit_code, error_msg, extras = await master_cc_run._run_cc(item)

  assert first.terminated is True
  assert (exit_code, error_msg, extras["account_relays"]) == (0, None, 1)
  assert builds[1]["kwargs"]["claude_account"].label == "ext-1"


@pytest.mark.asyncio
async def test_run_cc_falls_back_to_the_regular_select_when_the_below_line_target_vanishes(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The gate and the move read the pool at different moments: a target that rose to the
  line in between lands the regular select -- possibly at or above the line -- and the
  run continues instead of refusing."""
  cfg = conftest.fable_pool_cfg(tmp_path)
  conftest.make_transcript(tmp_path / "claude-main", UUID)
  meta = _session_on("main")
  # ext-1 is the gate's below-line destination; ext-2 already sits at the line.
  claude_accounts.observe_rate_limit("ext-1", conftest.rate_limit_event("allowed", 0.85)["rate_limit_info"], now=NOW)
  claude_accounts.observe_rate_limit("ext-2", conftest.rate_limit_event("allowed", 0.92)["rate_limit_info"], now=NOW)
  conftest.patch_select_with_transient_reading(monkeypatch, NOW)
  first = conftest.ScriptedRelayBackend(
      [
          conftest.rate_limit_event("allowed_warning", 0.92),
          conftest.assistant_text_event("step 1"),
          conftest.user_tool_result_event(),
      ],
      exit_code=0)
  second = conftest.ScriptedRelayBackend([backend_base.make_result_event()], exit_code=0)
  builds = _install_backends(monkeypatch, [first, second])
  item = conftest.make_work_item(cfg, meta, cfg.backends.options[0])

  _cc, exit_code, error_msg, extras = await master_cc_run._run_cc(item)

  assert first.terminated is True
  assert (exit_code, error_msg, extras["account_relays"]) == (0, None, 1)
  # The regular select lands ext-2 -- at the line, headroom 0.08 over ext-1's 0.05.
  assert builds[1]["kwargs"]["claude_account"].label == "ext-2"


@pytest.mark.asyncio
async def test_run_cc_warning_relays_past_the_cap_with_below_line_destinations(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A warning relay with a strictly-below-line destination is exempt from the relay cap:
  the fourth relay proceeds and the turn completes on the fifth account."""
  cfg = conftest.fable_pool_cfg(tmp_path, labels=("main", "a", "b", "c", "d"))
  conftest.make_transcript(tmp_path / "claude-main", UUID)
  meta = _session_on("main")
  warning = conftest.ScriptedRelayBackend(
      [conftest.rate_limit_event("allowed_warning", 0.92),
       conftest.user_tool_result_event()], exit_code=0)
  backends = [warning] + [
      conftest.ScriptedRelayBackend(
          [conftest.rate_limit_event("allowed_warning", 0.92),
           conftest.user_tool_result_event()], exit_code=0) for _ in range(3)
  ]
  last = conftest.ScriptedRelayBackend([backend_base.make_result_event()], exit_code=0)
  builds = _install_backends(monkeypatch, [*backends, last])
  item = conftest.make_work_item(cfg, meta, cfg.backends.options[0])

  _cc, exit_code, error_msg, extras = await master_cc_run._run_cc(item)

  assert (exit_code, error_msg, extras["account_relays"]) == (0, None, claude_relay.MAX_RELAYS_PER_TURN + 1)
  assert [b["kwargs"]["claude_account"].label for b in builds] == ["main", "a", "b", "c", "d"]


@pytest.mark.asyncio
async def test_run_cc_relay_limit_names_each_unavailable_account(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """With the relay count exhausted and no below-line destination -- every login
  authentication-failed, so no rejection reset exists either -- the cap error lists each
  account's cooldown."""
  cfg = conftest.fable_pool_cfg(tmp_path, labels=("main", "a", "b", "c"))
  conftest.make_transcript(tmp_path / "claude-main", UUID)
  meta = _session_on("main")
  failed = [
      conftest.ScriptedRelayBackend(
          [conftest.assistant_text_event("Failed to authenticate. Please run /login")], exit_code=1) for _ in range(4)
  ]
  builds = _install_backends(monkeypatch, failed)
  item = conftest.make_work_item(cfg, meta, cfg.backends.options[0])

  _cc, exit_code, error_msg, _extras = await master_cc_run._run_cc(item)

  assert exit_code == 1
  assert error_msg.startswith("Claude account relay limit reached (3 relays in one run; ")
  assert error_msg.endswith("this run did not complete.")
  for label in ("main", "a", "b", "c"):
    assert f"{label}: auth-failure cooldown until" in error_msg
  assert "UTC" in error_msg
  assert len(builds) == 1 + claude_relay.MAX_RELAYS_PER_TURN


# ---------------------------------------------------------------------------
# The watch's recompute and the cap message's empty pool state
# ---------------------------------------------------------------------------


def test_watch_stays_armed_without_a_below_line_destination_and_fires_when_one_appears(tmp_path: pathlib.Path) -> None:
  """At an armed safe point with no strictly-below-line account the watch keeps the run
  going (armed, reason None); a target that appears by the next safe point fires once."""
  cfg = conftest.fable_pool_cfg(tmp_path, labels=("main", "ext-1"))
  claude_accounts.observe_rate_limit("ext-1", conftest.rate_limit_event("allowed", 0.95)["rate_limit_info"], now=NOW)
  watch = claude_relay.RelayWatch("main", conftest.FABLE_MODEL, cfg=cfg, account_pool=None, now=NOW)

  assert watch.observe(conftest.rate_limit_event("allowed_warning", 0.92)) is False
  assert watch.armed is True
  assert watch.observe(conftest.user_tool_result_event()) is False
  assert watch.reason is None

  claude_accounts.observe_rate_limit("ext-1", conftest.rate_limit_event("allowed", 0.50)["rate_limit_info"], now=NOW)
  assert watch.observe(conftest.user_tool_result_event()) is True
  assert watch.reason == claude_relay.RELAY_WARNING


def test_relay_limit_message_without_unavailability_falls_back_to_a_named_slot(tmp_path: pathlib.Path) -> None:
  """Accounts healthy at or above the line carry no unavailability: the cap message says
  so instead of rendering an empty slot."""
  cfg = conftest.fable_pool_cfg(tmp_path)
  for label in ("main", "ext-1", "ext-2"):
    claude_accounts.observe_rate_limit(
        label, conftest.rate_limit_event("allowed_warning", 0.92)["rate_limit_info"], now=NOW)

  message = claude_relay.relay_limit_message(cfg)

  assert message == (
      "Claude account relay limit reached (3 relays in one run; no unavailability recorded); "
      "this run did not complete.")
