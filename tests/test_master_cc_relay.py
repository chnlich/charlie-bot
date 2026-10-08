"""Master account relay: turn placement, the in-run watch, and _run_cc continuing a turn on another pool account."""

from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest
from conftest import (
    BUILD_BACKEND_PATCH_TARGET,
    FABLE_MODEL,
    POOLED_FABLE_ID,
    ScriptedRelayBackend,
    assistant_text_event,
    fable_pool_cfg,
    fresh_state_fixture,
    install_scripted_backends,
    make_transcript,
    make_work_item,
    rate_limit_event,
    user_tool_result_event,
    write_pool_credentials,
)

from src.backends.claude_code import claude_accounts, claude_relay, master_cc_relay
from src.infra import event_types as ET
from src.infra.models import SessionCallbacks, SessionMetadata
from src.runtime import master_cc_run
from src.runtime.agent_process import base as backend_base

NOW = datetime(2026, 9, 6, 20, 0, tzinfo=UTC)
UUID = "uuid-relay-1"

_fresh_pool_state = fresh_state_fixture(claude_accounts.reset_for_tests)


def _install_backends(monkeypatch: pytest.MonkeyPatch, backends: list[ScriptedRelayBackend]) -> list[dict]:
  # The master-cc run path re-imports build_backend through the registry on
  # every call, so the patch lands there; the instructions builder is stubbed
  # with it because _run_cc builds instructions before the first backend build.
  builds = install_scripted_backends(monkeypatch, backends, BUILD_BACKEND_PATCH_TARGET)
  return builds


def _session_on(label: str | None, cc_session_id: str | None = UUID) -> SessionMetadata:
  return SessionMetadata(profile="manager", id="s1", name="t", backend=POOLED_FABLE_ID, cc_session_id=cc_session_id, claude_account=label)


def _events_of(callbacks: SessionCallbacks, event_type: str) -> list[dict]:
  return [
      call.args[1] for call in callbacks.persist_and_broadcast.await_args_list if call.args[1].get("type") == event_type
  ]


# ---------------------------------------------------------------------------
# Turn placement
# ---------------------------------------------------------------------------


def test_choose_turn_account_keeps_a_warm_healthy_account_under_the_warning_line(tmp_path: Path) -> None:
  cfg = fable_pool_cfg(tmp_path)
  claude_accounts.observe_rate_limit("main", rate_limit_event("allowed", 0.50)["rate_limit_info"], now=NOW)
  meta = _session_on("main")

  chosen, cold = master_cc_relay.choose_turn_account(cfg, meta, FABLE_MODEL, NOW - timedelta(minutes=10), now=NOW)

  assert (chosen.label, cold) == ("main", False)


# ---------------------------------------------------------------------------
# Turn placement inside a named pool
# ---------------------------------------------------------------------------


def test_choose_turn_account_reselects_inside_the_pool_when_the_current_account_is_outside(tmp_path: Path) -> None:
  """A warm, healthy, under-warning account outside the option's pool does not stick: the turn
  re-selects inside the pool."""
  cfg = fable_pool_cfg(tmp_path, claude_pools={"alpha": ["main"], "beta": ["ext-1", "ext-2"]})
  claude_accounts.observe_rate_limit("ext-1", rate_limit_event("allowed", 0.50)["rate_limit_info"], now=NOW)
  meta = _session_on("ext-1")

  chosen, cold = master_cc_relay.choose_turn_account(
      cfg, meta, FABLE_MODEL, NOW - timedelta(minutes=10), now=NOW, account_pool="alpha")

  assert (chosen.label, cold) == ("main", False)


@pytest.mark.asyncio
async def test_place_turn_copies_the_transcript_into_the_option_pool(tmp_path: Path) -> None:
  """A mid-session switch to another pool's option re-selects inside that pool and the existing
  move layer carries the transcript to the new account's directory."""
  cfg = fable_pool_cfg(tmp_path, claude_pools={"alpha": ["main"], "beta": ["ext-1", "ext-2"]})
  option = cfg.get_backend_option(POOLED_FABLE_ID)
  make_transcript(tmp_path / "claude-ext-1", UUID)
  meta = _session_on("ext-1")
  item = make_work_item(cfg, meta, option)

  ctx = master_cc_run._turn_launch_context(item, option, str(tmp_path / "work"), UUID)

  account = await master_cc_relay.place_turn(ctx, UUID, None, NOW - timedelta(minutes=10), now=NOW)

  assert account.label == "main"
  assert meta.claude_account == "main"
  assert claude_accounts.transcript_path(tmp_path / "claude-main", UUID) is not None


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
  notices = _events_of(item.callbacks, claude_relay.CLAUDE_ACCOUNT_LOGIN_REQUIRED)
  assert sorted(n["account"] for n in notices) == ["ext-1", "ext-2"]
  assert {n["reason"] for n in notices} == {"empty_credentials"}
