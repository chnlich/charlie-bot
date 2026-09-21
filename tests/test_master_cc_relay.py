"""Master account relay: turn placement, the in-run watch, and _run_cc continuing a turn on another pool account."""

import dataclasses
from collections.abc import AsyncIterator
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any
from unittest.mock import AsyncMock

import pytest
from conftest import (
    BUILD_BACKEND_PATCH_TARGET,
    FABLE_MODEL,
    LITELLM_503_ERROR_MESSAGE,
    LITELLM_FEEDBACK_BANNER_STDERR,
    POOLED_FABLE_ID,
    ScriptedRelayBackend,
    assistant_text_event,
    backend_option,
    fable_pool_cfg,
    fresh_state_fixture,
    install_scripted_backends,
    make_transcript,
    make_work_item,
    manager_backed_callbacks,
    mock_session_callbacks,
    patch_instructions_content,
    rate_limit_event,
    seed_transcript_copy,
    user_tool_result_event,
    write_pool_credentials,
)
from structlog.testing import capture_logs

from src.agents import master_cc_relay, master_cc_run, master_cc_state
from src.agents.backends import base as backend_base
from src.api import ext_usage as ext_usage_mod
from src.core import claude_accounts, claude_relay
from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.message_aggregator import MessageAggregator
from src.core.models import BackendOption, CreateSessionRequest, SessionCallbacks, SessionMetadata
from src.core.sessions import SessionManager

NOW = datetime(2026, 9, 6, 20, 0, tzinfo=UTC)
UUID = "uuid-relay-1"

_fresh_pool_state = fresh_state_fixture(claude_accounts.reset_for_tests)


def _install_backends(monkeypatch: pytest.MonkeyPatch, backends: list[ScriptedRelayBackend]) -> list[dict]:
  # The master-cc run path re-imports build_backend through the registry on
  # every call, so the patch lands there; the instructions builder is stubbed
  # with it because _run_cc builds instructions before the first backend build.
  builds = install_scripted_backends(monkeypatch, backends, BUILD_BACKEND_PATCH_TARGET)
  patch_instructions_content(monkeypatch)
  return builds


def _session_on(label: str | None, cc_session_id: str | None = UUID) -> SessionMetadata:
  return SessionMetadata(id="s1", name="t", backend=POOLED_FABLE_ID, cc_session_id=cc_session_id, claude_account=label)


def _events_of(callbacks: SessionCallbacks, event_type: str) -> list[dict]:
  return [
      call.args[1] for call in callbacks.persist_and_broadcast.await_args_list if call.args[1].get("type") == event_type
  ]


async def _seed_relay_session(
    cfg: CharlieBotConfig,
    name: str,
    persist_pair: bool = True,
) -> tuple[SessionManager, SessionMetadata, SessionMetadata, master_cc_state._WorkItem]:
  """One seeded session for the placement and _run_cc tests: create it, persist the
  relay pair (cc_session_id + claude_account="main") unless *persist_pair* is False,
  read the metadata back, and wrap it in the manager-backed work item.

  Returns (mgr, session, meta, item); callers bind what they use.
  Only the no-resume-id probe test passes persist_pair=False.
  """
  mgr = SessionManager(cfg)
  session = await mgr.create_session(CreateSessionRequest(name=name))
  if persist_pair:
    await mgr.persist_cc_session_id(session.id, UUID)
    await mgr.persist_claude_account(session.id, "main")
  meta = await mgr.get_session(session.id)
  item = make_work_item(cfg, meta, cfg.backends.options[0], callbacks=manager_backed_callbacks(mgr))
  return mgr, session, meta, item


# ---------------------------------------------------------------------------
# Turn placement
# ---------------------------------------------------------------------------


def test_choose_turn_account_keeps_a_warm_healthy_account_under_the_warning_line(tmp_path: Path) -> None:
  cfg = fable_pool_cfg(tmp_path)
  claude_accounts.observe_rate_limit("main", rate_limit_event("allowed", 0.50)["rate_limit_info"], now=NOW)
  meta = _session_on("main")

  chosen, cold = master_cc_relay.choose_turn_account(cfg, meta, FABLE_MODEL, NOW - timedelta(minutes=10), now=NOW)

  assert (chosen.label, cold) == ("main", False)


def test_choose_turn_account_reselects_on_a_cold_cache_or_at_the_warning_line(tmp_path: Path) -> None:
  cfg = fable_pool_cfg(tmp_path)
  claude_accounts.observe_rate_limit("main", rate_limit_event("allowed", 0.50)["rate_limit_info"], now=NOW)
  claude_accounts.observe_rate_limit("ext-1", rate_limit_event("allowed", 0.10)["rate_limit_info"], now=NOW)
  claude_accounts.observe_rate_limit("ext-2", rate_limit_event("allowed", 0.20)["rate_limit_info"], now=NOW)

  cold_pick, cold = master_cc_relay.choose_turn_account(
      cfg, _session_on("main"), FABLE_MODEL, NOW - timedelta(minutes=61), NOW)
  assert (cold_pick.label, cold) == ("ext-1", True)

  claude_accounts.observe_rate_limit("main", rate_limit_event("allowed_warning", 0.91)["rate_limit_info"], now=NOW)
  warm_pick, warm = master_cc_relay.choose_turn_account(
      cfg, _session_on("main"), FABLE_MODEL, NOW - timedelta(minutes=5), NOW)
  assert (warm_pick.label, warm) == ("ext-1", False)

  fresh_pick, _ = master_cc_relay.choose_turn_account(cfg, _session_on(None, None), FABLE_MODEL, None, NOW)
  assert fresh_pick.label == "ext-1"


@pytest.mark.asyncio
async def test_choose_turn_account_breaks_warm_stickiness_when_another_session_runs_the_account(tmp_path: Path) -> None:
  cfg = fable_pool_cfg(tmp_path)
  claude_accounts.observe_rate_limit("main", rate_limit_event("allowed", 0.50)["rate_limit_info"], now=NOW)
  other = SessionMetadata(id="s2", name="other", backend=POOLED_FABLE_ID, claude_account="main")
  master_cc_state._current_items["s2"] = make_work_item(cfg, other, cfg.backends.options[0])
  try:
    chosen, cold = master_cc_relay.choose_turn_account(
        cfg, _session_on("main"), FABLE_MODEL, NOW - timedelta(minutes=10), now=NOW)
  finally:
    master_cc_state._current_items.pop("s2", None)

  # The cache is warm and main is healthy under the warning line, but another
  # running session holds main: the stickiness breaks and an idle account wins.
  assert (chosen.label, cold) == ("ext-1", False)


@pytest.mark.asyncio
async def test_choose_turn_account_ignores_the_session_itself_in_the_busy_set(tmp_path: Path) -> None:
  cfg = fable_pool_cfg(tmp_path)
  claude_accounts.observe_rate_limit("main", rate_limit_event("allowed", 0.50)["rate_limit_info"], now=NOW)
  meta = _session_on("main")
  master_cc_state._current_items[meta.id] = make_work_item(cfg, meta, cfg.backends.options[0])
  try:
    chosen, cold = master_cc_relay.choose_turn_account(cfg, meta, FABLE_MODEL, NOW - timedelta(minutes=10), now=NOW)
  finally:
    master_cc_state._current_items.pop(meta.id, None)

  # The consumer parks the turn's own work item under the session id while it
  # runs; only another session's item may break the warm stickiness.
  assert (chosen.label, cold) == ("main", False)


def test_choose_turn_account_returns_none_when_the_pool_is_exhausted(tmp_path: Path) -> None:
  cfg = fable_pool_cfg(tmp_path, labels=("main",))
  claude_accounts.observe_rate_limit("main", rate_limit_event("rejected", 1.0)["rate_limit_info"], now=NOW)

  chosen, _cold = master_cc_relay.choose_turn_account(cfg, _session_on("main"), FABLE_MODEL, NOW, NOW)

  assert chosen is None
  assert "no available account" in claude_relay.pool_exhausted_message(cfg, NOW)
  assert "UTC" in claude_relay.pool_exhausted_message(cfg, NOW)


@pytest.mark.asyncio
async def test_place_turn_moves_the_transcript_when_the_account_changes(tmp_path: Path) -> None:
  cfg = fable_pool_cfg(tmp_path)
  make_transcript(tmp_path / "claude-main", UUID)
  claude_accounts.observe_rate_limit("main", rate_limit_event("allowed_warning", 0.95)["rate_limit_info"], now=NOW)
  meta = _session_on("main")
  item = make_work_item(cfg, meta, cfg.backends.options[0])

  account, error = await master_cc_relay.place_turn(
      cfg, item, cfg.backends.options[0], UUID, str(tmp_path), None, None, now=NOW)

  assert error is None
  assert account.label in {"ext-1", "ext-2"}
  assert meta.claude_account == account.label
  assert claude_accounts.transcript_path(account.config_dir, UUID) is not None
  assert claude_accounts.transcript_path(tmp_path / "claude-main", UUID) is not None, "source copy stays"


# ---------------------------------------------------------------------------
# The in-run watch
# ---------------------------------------------------------------------------


def test_relay_watch_rejection_relays_without_waiting_for_a_safe_point() -> None:
  watch = claude_relay.RelayWatch("main", FABLE_MODEL)
  assert watch.observe(rate_limit_event("rejected", 1.0)) is False
  assert watch.observe(user_tool_result_event()) is False
  assert watch.decision(1, "") == claude_relay.RELAY_REJECTED
  assert claude_accounts.headroom("main", FABLE_MODEL) == 0.0


def test_relay_watch_arms_on_a_far_warning_and_fires_at_the_next_tool_result() -> None:
  watch = claude_relay.RelayWatch("main", FABLE_MODEL)
  assert watch.observe(rate_limit_event("allowed_warning", 0.92)) is False
  assert watch.observe(assistant_text_event("working")) is False
  assert watch.observe(user_tool_result_event()) is True
  assert watch.observe(user_tool_result_event()) is False, "fires once"
  assert watch.decision(-15, "") == claude_relay.RELAY_WARNING


def test_relay_watch_ignores_a_warning_whose_reset_is_near_or_under_the_line() -> None:
  near = claude_relay.RelayWatch("main", FABLE_MODEL)
  near.observe(rate_limit_event("allowed_warning", 0.95, resets_in=timedelta(minutes=20)))
  assert near.observe(user_tool_result_event()) is False
  assert near.decision(0, "") is None

  low = claude_relay.RelayWatch("main", FABLE_MODEL)
  low.observe(rate_limit_event("allowed_warning", 0.85))
  assert low.observe(user_tool_result_event()) is False


def test_relay_watch_reports_a_login_failure_from_text_or_stderr() -> None:
  watch = claude_relay.RelayWatch("main", FABLE_MODEL)
  watch.observe(assistant_text_event("Failed to authenticate: OAuth session expired and could not be refreshed"))
  assert watch.decision(1, "") == claude_relay.LOGIN_FAILED
  assert watch.decision(0, "") is None, "a run that still exited 0 is not a login failure"
  assert claude_relay.RelayWatch("main", FABLE_MODEL).decision(1, "Failed to authenticate") == claude_relay.LOGIN_FAILED


# ---------------------------------------------------------------------------
# _run_cc across accounts
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_run_cc_relays_a_rejected_turn_onto_another_account(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = fable_pool_cfg(tmp_path)
  make_transcript(tmp_path / "claude-main", UUID)
  meta = _session_on("main")
  first = ScriptedRelayBackend([rate_limit_event("rejected", 1.0), backend_base.make_result_event()], exit_code=1)
  second = ScriptedRelayBackend([backend_base.make_result_event()], exit_code=0)
  builds = _install_backends(monkeypatch, [first, second])
  item = make_work_item(cfg, meta, cfg.backends.options[0])

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
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = fable_pool_cfg(tmp_path)
  make_transcript(tmp_path / "claude-main", UUID)
  meta = _session_on("main")
  first = ScriptedRelayBackend(
      [
          rate_limit_event("allowed_warning", 0.92),
          assistant_text_event("step 1"),
          user_tool_result_event(),
          assistant_text_event("never streamed")
      ],
      exit_code=0)
  second = ScriptedRelayBackend([backend_base.make_result_event()], exit_code=0)
  builds = _install_backends(monkeypatch, [first, second])
  item = make_work_item(cfg, meta, cfg.backends.options[0])

  _cc, exit_code, error_msg, extras = await master_cc_run._run_cc(item)

  assert first.terminated is True
  assert (exit_code, error_msg, extras["account_relays"]) == (0, None, 1)
  assert len(builds) == 2
  assert builds[1]["kwargs"]["claude_account"].config_dir == str(tmp_path / "claude-ext-1")
  assert second.prompt == claude_relay.CONTINUATION_PROMPT


@pytest.mark.asyncio
async def test_run_cc_reports_loudly_when_no_account_is_left(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = fable_pool_cfg(tmp_path)
  write_pool_credentials(tmp_path / "claude-ext-1", access_token="")
  write_pool_credentials(tmp_path / "claude-ext-2", access_token="")
  make_transcript(tmp_path / "claude-main", UUID)
  meta = _session_on("main")
  first = ScriptedRelayBackend([rate_limit_event("rejected", 1.0), backend_base.make_result_event()], exit_code=1)
  builds = _install_backends(monkeypatch, [first])
  item = make_work_item(cfg, meta, cfg.backends.options[0])

  _cc, exit_code, error_msg, _extras = await master_cc_run._run_cc(item)

  assert exit_code == 1
  assert "no available account" in error_msg and "UTC" in error_msg
  assert len(builds) == 1
  errors = _events_of(item.callbacks, ET.ASSISTANT_ERROR)
  assert len(errors) == 1 and "no available account" in errors[0]["content"]
  # Both emptied logins were reported once, without waiting for a run on them.
  notices = _events_of(item.callbacks, ET.CLAUDE_ACCOUNT_LOGIN_REQUIRED)
  assert sorted(n["account"] for n in notices) == ["ext-1", "ext-2"]
  assert {n["reason"] for n in notices} == {"empty_credentials"}


@pytest.mark.asyncio
async def test_run_cc_stops_after_the_relay_limit(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = fable_pool_cfg(tmp_path, labels=("main", "a", "b", "c", "d"))
  make_transcript(tmp_path / "claude-main", UUID)
  meta = _session_on("main")
  backends = [
      ScriptedRelayBackend(
          [rate_limit_event("rejected", 1.0), backend_base.make_result_event()], exit_code=1) for _ in range(5)
  ]
  builds = _install_backends(monkeypatch, backends)
  item = make_work_item(cfg, meta, cfg.backends.options[0])

  _cc, exit_code, error_msg, extras = await master_cc_run._run_cc(item)

  assert exit_code == 1
  assert "relay limit" in error_msg
  assert len(builds) == 1 + claude_relay.MAX_RELAYS_PER_TURN
  assert extras["account_relays"] == claude_relay.MAX_RELAYS_PER_TURN


@pytest.mark.asyncio
async def test_run_cc_marks_a_login_failure_and_relays(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = fable_pool_cfg(tmp_path)
  make_transcript(tmp_path / "claude-main", UUID)
  meta = _session_on("main")
  first = ScriptedRelayBackend(
      [assistant_text_event("Failed to authenticate: OAuth session expired and could not be refreshed")], exit_code=1)
  second = ScriptedRelayBackend([backend_base.make_result_event()], exit_code=0)
  builds = _install_backends(monkeypatch, [first, second])
  item = make_work_item(cfg, meta, cfg.backends.options[0])

  _cc, exit_code, error_msg, _extras = await master_cc_run._run_cc(item)

  assert (exit_code, error_msg) == (0, None)
  assert len(builds) == 2 and meta.claude_account == "ext-1"
  notices = _events_of(item.callbacks, ET.CLAUDE_ACCOUNT_LOGIN_REQUIRED)
  assert notices == [
      {
          "type": ET.CLAUDE_ACCOUNT_LOGIN_REQUIRED,
          "account": "main",
          "config_dir": str(tmp_path / "claude-main"),
          "reason": "auth_failed",
      }
  ]
  assert claude_accounts.healthy(claude_accounts.account_by_label(cfg, "main")) is False


@pytest.mark.parametrize(
    ("context_tokens", "minutes_since", "compacted"),
    [
        pytest.param(120_000, 61, True, id="cold_and_large"),
        pytest.param(40_000, 61, False, id="cold_but_small"),
        pytest.param(350_000, 30, False, id="warm"),
    ],
)
@pytest.mark.asyncio
async def test_run_cc_compacts_with_sonnet_before_spawning_on_an_expired_cache(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, context_tokens: int, minutes_since: int, compacted: bool) -> None:
  cfg = fable_pool_cfg(tmp_path)
  make_transcript(tmp_path / "claude-main", UUID)
  meta = _session_on("main")
  order: list[str] = []

  async def fake_compact(**kwargs: Any) -> bool:
    order.append(f"compact:{Path(kwargs['config_dir']).name}:{kwargs['pre_tokens']}")
    return True

  monkeypatch.setattr(master_cc_relay.claude_compaction, "compact_with_sonnet", fake_compact)
  backend = ScriptedRelayBackend([backend_base.make_result_event()], exit_code=0)
  builds: list[dict] = []

  def fake_build_backend(option: BackendOption, cfg_: CharlieBotConfig, **kwargs: Any) -> ScriptedRelayBackend:
    order.append("spawn")
    builds.append({"claude_account": kwargs["claude_account"]})
    return backend

  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, fake_build_backend)
  patch_instructions_content(monkeypatch)
  callbacks = dataclasses.replace(
      mock_session_callbacks(),
      claude_context_state=AsyncMock(
          return_value=(context_tokens, datetime.now(UTC) - timedelta(minutes=minutes_since))))
  item = make_work_item(cfg, meta, cfg.backends.options[0], callbacks=callbacks)

  _cc, exit_code, _err, _extras = await master_cc_run._run_cc(item)

  assert exit_code == 0
  if compacted:
    assert order == [f"compact:{Path(builds[0]['claude_account'].config_dir).name}:{context_tokens}", "spawn"]
  else:
    assert order == ["spawn"]


@pytest.mark.asyncio
async def test_run_cc_relay_compacts_a_large_fable_context_on_the_new_account(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = fable_pool_cfg(tmp_path)
  make_transcript(tmp_path / "claude-main", UUID)
  meta = _session_on("main")
  compactions: list[tuple[str, int | None]] = []

  async def fake_compact(**kwargs: Any) -> bool:
    compactions.append((Path(kwargs["config_dir"]).name, kwargs["pre_tokens"]))
    return True

  monkeypatch.setattr(master_cc_relay.claude_compaction, "compact_with_sonnet", fake_compact)
  first = ScriptedRelayBackend([rate_limit_event("rejected", 1.0), backend_base.make_result_event()], exit_code=1)
  second = ScriptedRelayBackend([backend_base.make_result_event()], exit_code=0)
  _install_backends(monkeypatch, [first, second])
  callbacks = dataclasses.replace(
      mock_session_callbacks(),
      claude_context_state=AsyncMock(return_value=(150_000, datetime.now(UTC) - timedelta(minutes=1))))
  item = make_work_item(cfg, meta, cfg.backends.options[0], callbacks=callbacks)

  _cc, exit_code, _err, _extras = await master_cc_run._run_cc(item)

  assert exit_code == 0
  assert compactions == [("claude-ext-1", 150_000)], "relay compaction runs in the new login, not at turn start"


@pytest.mark.asyncio
async def test_run_cc_without_a_pool_spawns_the_option_unchanged(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(tmp_path / "login"))
  cfg = CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backends={"options": [backend_option(id="solo", label="Solo", type="cc-claude", model=FABLE_MODEL)]},
  )
  make_transcript(tmp_path / "login", UUID)
  meta = SessionMetadata(id="s1", name="t", backend="solo", cc_session_id=UUID)
  backend = ScriptedRelayBackend([rate_limit_event("rejected", 1.0), backend_base.make_result_event()], exit_code=1)
  builds = _install_backends(monkeypatch, [backend])
  item = make_work_item(cfg, meta, cfg.backends.options[0])

  _cc, exit_code, _err, extras = await master_cc_run._run_cc(item)

  assert builds[0]["option"] is cfg.backends.options[0]
  assert builds[0]["kwargs"]["claude_account"] is None
  assert builds[0]["kwargs"]["extra_flags"] == ["--resume", UUID, "--exclude-dynamic-system-prompt-sections"]
  assert len(builds) == 1 and exit_code == 1
  assert "account_relays" not in extras
  assert meta.claude_account is None
  item.callbacks.claude_context_state.assert_not_awaited()


# ---------------------------------------------------------------------------
# Operator surfaces
# ---------------------------------------------------------------------------


def test_login_required_renders_account_free_in_chat() -> None:
  agg = MessageAggregator()
  deltas = list(
      agg.feed(
          {
              "type": ET.CLAUDE_ACCOUNT_LOGIN_REQUIRED,
              "account": "ext-2",
              "config_dir": "/x/.claude-ext-2",
              "reason": "auth_failed",
              "timestamp": "t",
          }))
  message = deltas[0]["message"]
  assert message["kind"] == "claude_account_login_required"
  assert "ext-2" not in message["content"] and "/x/" not in message["content"]
  assert "usage panel" in message["content"]


def test_usage_panel_entry_carries_the_login_directory_while_unhealthy(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = fable_pool_cfg(tmp_path)
  monkeypatch.setattr(ext_usage_mod, "get_config", lambda: cfg)
  monkeypatch.setattr(ext_usage_mod, "_cached_usage", {"claude:ext-1": {"provider": "claude", "account": "ext-1"}})

  account = claude_accounts.account_by_label(cfg, "ext-1")
  assert ext_usage_mod._derive_accounts()["pool"]["ext-1"] == account.config_dir

  claude_accounts.record_auth_failure("ext-1")
  ext_usage_mod._annotate_login_state("claude:ext-1", account)
  assert ext_usage_mod._cached_usage["claude:ext-1"]["login_required"] == str(tmp_path / "claude-ext-1")

  claude_accounts.reset_for_tests()
  ext_usage_mod._annotate_login_state("claude:ext-1", account)
  assert "login_required" not in ext_usage_mod._cached_usage["claude:ext-1"]


# ---------------------------------------------------------------------------
# Label persistence at placement, the lineage probe, and refusal self-heal
# ---------------------------------------------------------------------------


def _reconciled(logs: list[dict]) -> dict | None:
  return next((entry for entry in logs if entry["event"] == "master_cc_account_label_reconciled"), None)


@pytest.mark.asyncio
async def test_place_turn_persists_the_label_when_the_move_lands(tmp_path: Path) -> None:
  """A cold-cache switch moves the transcript and the label lands on disk immediately:
  a fresh SessionManager reads the new account before the round has done anything."""
  cfg = fable_pool_cfg(tmp_path)
  make_transcript(tmp_path / "claude-main", UUID)
  claude_accounts.observe_rate_limit("main", rate_limit_event("allowed_warning", 0.95)["rate_limit_info"], now=NOW)
  mgr, session, _meta, item = await _seed_relay_session(cfg, "disk-true")

  account, error = await master_cc_relay.place_turn(
      cfg, item, cfg.backends.options[0], UUID, str(tmp_path), None, None, now=NOW)

  assert error is None
  disk = await mgr.read_metadata_fresh(session.id)
  assert disk.claude_account == account.label
  cold_reader = SessionManager(cfg)
  cold_disk = await cold_reader.read_metadata_fresh(session.id)
  assert cold_disk.claude_account == account.label


@pytest.mark.asyncio
async def test_place_turn_probe_adopts_the_newest_holder_after_a_kill_between_move_and_persist(tmp_path: Path) -> None:
  """Kill-shaped interleave: the relay's move landed, its label persist did not. The
  next placement's probe adopts the newest holder before selection and continues
  from it without moving or touching a byte of it."""
  cfg = fable_pool_cfg(tmp_path)
  seed_line = '{"type": "user", "content": "seed"}\n'
  grown_tail = '{"type": "assistant", "content": "' + "x" * (claude_accounts.PROBE_TAIL_BYTES * 2) + '"}\n'
  stale = seed_transcript_copy(tmp_path / "claude-main", UUID, seed_line, mtime_ns=1_000)
  live = seed_transcript_copy(tmp_path / "claude-ext-1", UUID, seed_line + grown_tail, mtime_ns=2_000)
  live_before = live.stat()
  mgr, session, meta, item = await _seed_relay_session(cfg, "kill-window")

  with capture_logs() as logs:
    account, error = await master_cc_relay.place_turn(
        cfg, item, cfg.backends.options[0], UUID, str(tmp_path), NOW - timedelta(minutes=10), None, now=NOW)

  assert error is None
  assert account.label == "ext-1", "the probe adopted the holder of the grown copy"
  assert meta.claude_account == "ext-1"
  disk = await mgr.read_metadata_fresh(session.id)
  assert disk.claude_account == "ext-1"
  reconciled = _reconciled(logs)
  assert reconciled is not None
  assert reconciled["adopted"] == "ext-1" and reconciled["previous"] == "main"
  assert reconciled["reason"] == "forked_or_stale_lineage"
  live_after = live.stat()
  assert (live_after.st_mtime_ns,
          live_after.st_size) == (live_before.st_mtime_ns,
                                  live_before.st_size), ("the adopted copy's bytes and stamp are untouched")
  assert stale.read_text(encoding="utf-8") == seed_line


@pytest.mark.asyncio
async def test_place_turn_probe_skips_with_at_most_the_labels_own_copy(tmp_path: Path) -> None:
  """One pool copy (the label's own) is nothing to reconcile: no adoption, no warn."""
  cfg = fable_pool_cfg(tmp_path)
  make_transcript(tmp_path / "claude-main", UUID)
  mgr, session, _meta, item = await _seed_relay_session(cfg, "single-copy")

  with capture_logs() as logs:
    account, error = await master_cc_relay.place_turn(
        cfg, item, cfg.backends.options[0], UUID, str(tmp_path), NOW - timedelta(minutes=10), None, now=NOW)

  assert error is None
  assert _reconciled(logs) is None
  disk = await mgr.read_metadata_fresh(session.id)
  assert disk.claude_account == account.label
  assert account.label == "main", "the label was never second-guessed"


@pytest.mark.asyncio
async def test_place_turn_probe_skips_without_a_resume_id(tmp_path: Path) -> None:
  cfg = fable_pool_cfg(tmp_path)
  _mgr, _session, _meta, item = await _seed_relay_session(cfg, "fresh", persist_pair=False)

  with capture_logs() as logs:
    account, error = await master_cc_relay.place_turn(
        cfg, item, cfg.backends.options[0], None, str(tmp_path), None, None, now=NOW)

  assert error is None
  assert _reconciled(logs) is None
  assert account is not None


@pytest.mark.asyncio
async def test_place_turn_probe_skips_for_a_declared_fresh_start(tmp_path: Path) -> None:
  """expect_fresh_session is the weekly recycle's no-transcript scenario: the probe
  stays out and the existing label path runs."""
  cfg = fable_pool_cfg(tmp_path)
  seed_line = '{"type": "user", "content": "seed"}\n'
  grown_tail = '{"type": "assistant", "content": "' + "x" * (claude_accounts.PROBE_TAIL_BYTES * 2) + '"}\n'
  seed_transcript_copy(tmp_path / "claude-main", UUID, seed_line, mtime_ns=1_000)
  seed_transcript_copy(tmp_path / "claude-ext-1", UUID, seed_line + grown_tail, mtime_ns=2_000)
  _mgr, _session, meta, item = await _seed_relay_session(cfg, "recycled")
  item.expect_fresh_session = True

  with capture_logs() as logs:
    account, error = await master_cc_relay.place_turn(
        cfg, item, cfg.backends.options[0], UUID, str(tmp_path), NOW - timedelta(minutes=10), None, now=NOW)

  assert error is None
  assert _reconciled(logs) is None
  assert meta.claude_account == account.label


@pytest.mark.asyncio
async def test_place_turn_adopts_the_refused_destination_and_continues_from_it(tmp_path: Path) -> None:
  """The placement move refused by the guard (the destination holds a strictly newer
  copy) does not fail the turn: the destination is adopted, persisted, and the turn
  continues from it with no copy."""
  cfg = fable_pool_cfg(tmp_path)
  seed_transcript_copy(tmp_path / "claude-main", UUID, '{"stale": true}\n', mtime_ns=1_000)
  live = seed_transcript_copy(tmp_path / "claude-ext-1", UUID, '{"stale": true}\n{"live": true}\n', mtime_ns=2_000)
  claude_accounts.observe_rate_limit("main", rate_limit_event("allowed_warning", 0.95)["rate_limit_info"], now=NOW)
  live_before = live.stat()
  mgr, session, meta, item = await _seed_relay_session(cfg, "refused")

  with capture_logs() as logs:
    account, error = await master_cc_relay.place_turn(
        cfg, item, cfg.backends.options[0], UUID, str(tmp_path), NOW - timedelta(hours=2), None, now=NOW)

  assert error is None, "a refusal the newer copy already answers is not a failed placement"
  assert account.label == "ext-1"
  assert meta.claude_account == "ext-1"
  disk = await mgr.read_metadata_fresh(session.id)
  assert disk.claude_account == "ext-1"
  reconciled = _reconciled(logs)
  assert reconciled is not None
  assert reconciled["reason"] == "guard_refused_newer_transcript"
  live_after = live.stat()
  assert (live_after.st_mtime_ns, live_after.st_size) == (live_before.st_mtime_ns, live_before.st_size)


@pytest.mark.asyncio
async def test_run_cc_persists_the_label_when_the_mid_turn_relay_lands(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = fable_pool_cfg(tmp_path)
  make_transcript(tmp_path / "claude-main", UUID)
  mgr, session, _meta, item = await _seed_relay_session(cfg, "mid-relay")
  first = ScriptedRelayBackend([rate_limit_event("rejected", 1.0), backend_base.make_result_event()], exit_code=1)
  second = ScriptedRelayBackend([backend_base.make_result_event()], exit_code=0)
  _install_backends(monkeypatch, [first, second])

  cc_session_id, exit_code, error_msg, _extras = await master_cc_run._run_cc(item)

  assert (cc_session_id, exit_code, error_msg) == (UUID, 0, None)
  disk = await mgr.read_metadata_fresh(session.id)
  assert disk.claude_account == "ext-1", "the label is disk-true from the relay, not at round end"


@pytest.mark.asyncio
async def test_run_cc_mid_turn_refusal_adopts_the_destination_and_continues(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The mid-turn relay's move refused by the guard (the destination holds a newer
  copy) adopts the destination and continues the turn from it -- no failed turn, no
  overwrite."""
  cfg = fable_pool_cfg(tmp_path)
  # The label copy is old; ext-2 holds its successor -- same lineage, grown
  # inside the probe's tail window -- under a newer stamp from an earlier state
  # of this session. The placement probe therefore stays out of the way, and the
  # mid-turn relay onto ext-2 refuses and is adopted rather than overwriting.
  seed_line = '{"type": "user", "content": "seed"}\n'
  seed_transcript_copy(tmp_path / "claude-main", UUID, seed_line, mtime_ns=1_000)
  live = seed_transcript_copy(
      tmp_path / "claude-ext-2",
      UUID,
      seed_line + '{"type": "assistant", "content": "grew a little"}\n',
      mtime_ns=9_000)
  live_before = live.stat()
  mgr, session, _meta, item = await _seed_relay_session(cfg, "mid-refusal")
  first = ScriptedRelayBackend([rate_limit_event("rejected", 1.0), backend_base.make_result_event()], exit_code=1)
  second = ScriptedRelayBackend([rate_limit_event("rejected", 1.0), backend_base.make_result_event()], exit_code=1)
  third = ScriptedRelayBackend([backend_base.make_result_event()], exit_code=0)
  builds = _install_backends(monkeypatch, [first, second, third])

  with capture_logs() as logs:
    cc_session_id, exit_code, error_msg, extras = await master_cc_run._run_cc(item)

  assert (cc_session_id, exit_code, error_msg) == (UUID, 0, None)
  assert extras["account_relays"] == 2, "the adoption counts toward the relay cap like any account change"
  assert builds[2]["kwargs"]["claude_account"].config_dir == str(tmp_path / "claude-ext-2")
  assert third.prompt == claude_relay.CONTINUATION_PROMPT
  reconciled = _reconciled(logs)
  assert reconciled is not None
  assert reconciled["adopted"] == "ext-2" and reconciled["previous"] == "ext-1"
  assert reconciled["reason"] == "guard_refused_newer_transcript"
  disk = await mgr.read_metadata_fresh(session.id)
  assert disk.claude_account == "ext-2"
  live_after = live.stat()
  assert (live_after.st_mtime_ns,
          live_after.st_size) == (live_before.st_mtime_ns,
                                  live_before.st_size), ("the adopted copy was not overwritten")


# ---------------------------------------------------------------------------
# End-of-run error hint on the live exit path
# ---------------------------------------------------------------------------


def _solo_cfg(tmp_path: Path) -> CharlieBotConfig:
  """A non-pooled cc-claude config: no relay watch, the plain exit path."""
  return CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backends={"options": [backend_option(id="solo", label="Solo", type="cc-claude", model=FABLE_MODEL)]},
  )


class _StoppedMidStreamBackend:
  """A backend the user stopped mid-stream: the error event had already
  streamed, then the stop landed and the transport ended terminated."""

  exit_code = -15
  stderr_text = LITELLM_FEEDBACK_BANNER_STDERR
  terminated = True
  hang_diagnostics = None

  def cgroup_exit_report(self) -> str | None:
    return None

  async def terminate(self) -> None:
    self.terminated = True

  async def run(self,
                prompt: str,
                cwd: str,
                env: dict,
                uploaded_files: list[dict] | None = None) -> AsyncIterator[dict]:
    yield backend_base.make_error_event(LITELLM_503_ERROR_MESSAGE)


class _OomReportBackend(ScriptedRelayBackend):
  """Scripted backend whose cgroup attribution fires (session memory cap)."""

  def __init__(self, events: list[dict], exit_code: int, stderr_text: str, report: str) -> None:
    super().__init__(events, exit_code, stderr_text=stderr_text)
    self._report = report

  def cgroup_exit_report(self) -> str | None:
    return self._report


@pytest.mark.asyncio
@pytest.mark.parametrize(
    ("scripted_exit", "expected_exit", "expected_msg", "expected_errors"),
    [
        pytest.param(
            1,
            1,
            LITELLM_503_ERROR_MESSAGE,
            [f"Agent error: {LITELLM_503_ERROR_MESSAGE}"],
            id="nonzero-exit-publishes-the-error-event",
        ),
        pytest.param(0, 0, None, [], id="zero-exit-stays-hint-free"),
    ],
)
async def test_run_cc_error_hint_follows_the_invocation_exit(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    scripted_exit: int,
    expected_exit: int,
    expected_msg: str | None,
    expected_errors: list[str],
) -> None:
  """The invocation's structured error event is the end-of-run hint, never the
  stderr help banner: a nonzero exit publishes it, and an exit-0 recovery stays
  hint-free even though the stream carried the same error event."""
  monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(tmp_path / "login"))
  cfg = _solo_cfg(tmp_path)
  meta = SessionMetadata(id="s1", name="t", backend="solo")
  backend = ScriptedRelayBackend(
      [backend_base.make_error_event(LITELLM_503_ERROR_MESSAGE),
       backend_base.make_result_event()],
      exit_code=scripted_exit,
      stderr_text=LITELLM_FEEDBACK_BANNER_STDERR)
  _install_backends(monkeypatch, [backend])
  item = make_work_item(cfg, meta, cfg.backends.options[0])

  _cc, exit_code, error_msg, _extras = await master_cc_run._run_cc(item)

  assert exit_code == expected_exit
  assert error_msg == expected_msg
  assert [e["content"] for e in _events_of(item.callbacks, ET.ASSISTANT_ERROR)] == expected_errors


@pytest.mark.asyncio
async def test_run_cc_terminated_stays_hint_free(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """An explicit user stop keeps today's no-hint behavior: the stop's own kill
  is not a failure the invocation's error events or stderr should explain."""
  monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(tmp_path / "login"))
  cfg = _solo_cfg(tmp_path)
  meta = SessionMetadata(id="s1", name="t", backend="solo")
  _install_backends(monkeypatch, [_StoppedMidStreamBackend()])
  item = make_work_item(cfg, meta, cfg.backends.options[0])

  _cc, exit_code, error_msg, _extras = await master_cc_run._run_cc(item)

  assert exit_code == -15
  assert error_msg is None
  assert _events_of(item.callbacks, ET.ASSISTANT_ERROR) == []


@pytest.mark.asyncio
async def test_run_cc_cgroup_report_wins_over_error_event_and_stderr(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The session memory-cap / host-OOM attribution still outranks both the
  invocation's error events and the stderr tail."""
  monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(tmp_path / "login"))
  cfg = _solo_cfg(tmp_path)
  meta = SessionMetadata(id="s1", name="t", backend="solo")
  report = "Session memory cap hit: the run was killed by its cgroup (oom_kill 1)."
  backend = _OomReportBackend(
      [backend_base.make_error_event(LITELLM_503_ERROR_MESSAGE)], 137, LITELLM_FEEDBACK_BANNER_STDERR, report)
  _install_backends(monkeypatch, [backend])
  item = make_work_item(cfg, meta, cfg.backends.options[0])

  _cc, exit_code, error_msg, _extras = await master_cc_run._run_cc(item)

  assert exit_code == 137
  assert error_msg == report
  errors = _events_of(item.callbacks, ET.ASSISTANT_ERROR)
  assert len(errors) == 1 and errors[0]["content"] == f"Agent error: {report}"
