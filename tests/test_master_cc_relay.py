"""Master account relay: turn placement, the in-run watch, and _run_cc continuing a turn on another pool account."""

import dataclasses
import json
from datetime import UTC, datetime, timedelta
from pathlib import Path
from unittest.mock import AsyncMock

import pytest
from conftest import (
    BUILD_BACKEND_PATCH_TARGET,
    FakeBackend,
    fresh_state_fixture,
    make_work_item,
    mock_session_callbacks,
    patch_instructions_content,
)

from src.agents import master_cc_relay, master_cc_run
from src.agents.backends import base as backend_base
from src.api import ext_usage as ext_usage_mod
from src.core import claude_accounts, claude_relay
from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.message_aggregator import MessageAggregator
from src.core.models import BackendOption, ClaudeAccount, SessionMetadata

NOW = datetime(2026, 9, 6, 20, 0, tzinfo=UTC)
FABLE = "claude-fable-5-1"
UUID = "uuid-relay-1"
SLUG = "-home-u--charliebot-sessions-s1"

_fresh_pool_state = fresh_state_fixture(claude_accounts.reset_for_tests)


def _write_credentials(config_dir: Path, access_token: str = "token") -> None:
  config_dir.mkdir(parents=True, exist_ok=True)
  (config_dir / claude_accounts.CREDENTIALS_FILE).write_text(
      json.dumps({"claudeAiOauth": {
          "accessToken": access_token,
          "refreshToken": "r"
      }}), encoding="utf-8")


def _pool_cfg(tmp_path: Path, labels: tuple[str, ...] = ("main", "ext-1", "ext-2")) -> CharlieBotConfig:
  accounts = [ClaudeAccount(label=label, config_dir=str(tmp_path / f"claude-{label}")) for label in labels]
  for account in accounts:
    _write_credentials(Path(account.config_dir))
  return CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      claude_accounts=accounts,
      backend_options=[
          BackendOption(id="claude-fable-5", label="Fable", type="cc-claude", model=FABLE),
          BackendOption(
              id="pinned", label="Pinned", type="cc-claude", model=FABLE, claude_config_dir=str(tmp_path / "pinned")),
      ],
  )


def _write_transcript(config_dir: Path, cc_session_id: str = UUID) -> Path:
  transcript = config_dir / "projects" / SLUG / f"{cc_session_id}.jsonl"
  transcript.parent.mkdir(parents=True, exist_ok=True)
  transcript.write_text('{"type":"user"}\n', encoding="utf-8")
  return transcript


def _rate_limit(status: str, utilization: float, resets_in: timedelta = timedelta(hours=3)) -> dict:
  return {
      "type": ET.RATE_LIMIT_EVENT,
      "rate_limit_info":
          {
              "status": status,
              "rateLimitType": "five_hour",
              "resetsAt": (datetime.now(UTC) + resets_in).timestamp(),
              "unifiedWindows": {
                  "five_hour": {
                      "utilization": utilization
                  },
                  "seven_day": {
                      "utilization": 0.3
                  },
              },
          },
  }


def _tool_result() -> dict:
  return {"type": ET.USER, "message": {"role": "user", "content": [{"type": ET.TOOL_RESULT, "content": "ok"}]}}


def _assistant(text: str) -> dict:
  return {"type": ET.ASSISTANT, "message": {"role": "assistant", "content": [{"type": "text", "text": text}]}}


class _ScriptedBackend(FakeBackend):
  """Yields a script of events; terminate() ends the stream and records the kill."""

  def __init__(self, events: list[dict], exit_code: int, stderr_text: str = "") -> None:
    self._events = events
    self.exit_code = exit_code
    self.stderr_text = stderr_text
    self.terminated = False
    self.prompt: str | None = None
    self.env: dict | None = None

  async def terminate(self) -> None:
    self.terminated = True
    self.exit_code = -15

  async def run(self, prompt: str, cwd: str, env: dict):
    self.prompt = prompt
    self.env = env
    for event in self._events:
      if self.terminated:
        return
      yield event


def _install_backends(monkeypatch, backends: list[_ScriptedBackend]) -> list[dict]:
  builds: list[dict] = []
  queue = list(backends)

  def fake_build_backend(option, cfg, **kwargs):
    backend = queue.pop(0)
    builds.append({"option": option, "kwargs": kwargs, "backend": backend})
    return backend

  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, fake_build_backend)
  patch_instructions_content(monkeypatch)
  return builds


def _session_on(label: str | None, cc_session_id: str | None = UUID) -> SessionMetadata:
  return SessionMetadata(id="s1", name="t", backend="claude-fable-5", cc_session_id=cc_session_id, claude_account=label)


def _events_of(callbacks, event_type: str) -> list[dict]:
  return [
      call.args[1] for call in callbacks.persist_and_broadcast.await_args_list if call.args[1].get("type") == event_type
  ]


# ---------------------------------------------------------------------------
# Turn placement
# ---------------------------------------------------------------------------


def test_choose_turn_account_keeps_a_warm_healthy_account_under_the_warning_line(tmp_path: Path) -> None:
  cfg = _pool_cfg(tmp_path)
  claude_accounts.observe_rate_limit("main", _rate_limit("allowed", 0.50)["rate_limit_info"], now=NOW)
  meta = _session_on("main")

  chosen, cold = master_cc_relay.choose_turn_account(cfg, meta, FABLE, NOW - timedelta(minutes=10), now=NOW)

  assert (chosen.label, cold) == ("main", False)


def test_choose_turn_account_reselects_on_a_cold_cache_or_at_the_warning_line(tmp_path: Path) -> None:
  cfg = _pool_cfg(tmp_path)
  claude_accounts.observe_rate_limit("main", _rate_limit("allowed", 0.50)["rate_limit_info"], now=NOW)
  claude_accounts.observe_rate_limit("ext-1", _rate_limit("allowed", 0.10)["rate_limit_info"], now=NOW)
  claude_accounts.observe_rate_limit("ext-2", _rate_limit("allowed", 0.20)["rate_limit_info"], now=NOW)

  cold_pick, cold = master_cc_relay.choose_turn_account(
      cfg, _session_on("main"), FABLE, NOW - timedelta(minutes=61), NOW)
  assert (cold_pick.label, cold) == ("ext-1", True)

  claude_accounts.observe_rate_limit("main", _rate_limit("allowed_warning", 0.91)["rate_limit_info"], now=NOW)
  warm_pick, warm = master_cc_relay.choose_turn_account(
      cfg, _session_on("main"), FABLE, NOW - timedelta(minutes=5), NOW)
  assert (warm_pick.label, warm) == ("ext-1", False)

  fresh_pick, _ = master_cc_relay.choose_turn_account(cfg, _session_on(None, None), FABLE, None, NOW)
  assert fresh_pick.label == "ext-1"


def test_choose_turn_account_returns_none_when_the_pool_is_exhausted(tmp_path: Path) -> None:
  cfg = _pool_cfg(tmp_path, labels=("main",))
  claude_accounts.observe_rate_limit("main", _rate_limit("rejected", 1.0)["rate_limit_info"], now=NOW)

  chosen, _cold = master_cc_relay.choose_turn_account(cfg, _session_on("main"), FABLE, NOW, NOW)

  assert chosen is None
  assert "no available account" in claude_relay.pool_exhausted_message(cfg, NOW)
  assert "UTC" in claude_relay.pool_exhausted_message(cfg, NOW)


@pytest.mark.asyncio
async def test_place_turn_moves_the_transcript_when_the_account_changes(tmp_path: Path) -> None:
  cfg = _pool_cfg(tmp_path)
  _write_transcript(tmp_path / "claude-main")
  claude_accounts.observe_rate_limit("main", _rate_limit("allowed_warning", 0.95)["rate_limit_info"], now=NOW)
  meta = _session_on("main")
  item = make_work_item(cfg, meta, cfg.backend_options[0])

  account, error = await master_cc_relay.place_turn(
      cfg, item, cfg.backend_options[0], UUID, str(tmp_path), None, None, now=NOW)

  assert error is None
  assert account.label in {"ext-1", "ext-2"}
  assert meta.claude_account == account.label
  assert claude_accounts.transcript_path(account.config_dir, UUID) is not None
  assert claude_accounts.transcript_path(tmp_path / "claude-main", UUID) is not None, "source copy stays"


# ---------------------------------------------------------------------------
# The in-run watch
# ---------------------------------------------------------------------------


def test_relay_watch_rejection_relays_without_waiting_for_a_safe_point() -> None:
  watch = claude_relay.RelayWatch("main", FABLE)
  assert watch.observe(_rate_limit("rejected", 1.0)) is False
  assert watch.observe(_tool_result()) is False
  assert watch.decision(1, "") == claude_relay.RELAY_REJECTED
  assert claude_accounts.headroom("main", FABLE) == 0.0


def test_relay_watch_arms_on_a_far_warning_and_fires_at_the_next_tool_result() -> None:
  watch = claude_relay.RelayWatch("main", FABLE)
  assert watch.observe(_rate_limit("allowed_warning", 0.92)) is False
  assert watch.observe(_assistant("working")) is False
  assert watch.observe(_tool_result()) is True
  assert watch.observe(_tool_result()) is False, "fires once"
  assert watch.decision(-15, "") == claude_relay.RELAY_WARNING


def test_relay_watch_ignores_a_warning_whose_reset_is_near_or_under_the_line() -> None:
  near = claude_relay.RelayWatch("main", FABLE)
  near.observe(_rate_limit("allowed_warning", 0.95, resets_in=timedelta(minutes=20)))
  assert near.observe(_tool_result()) is False
  assert near.decision(0, "") is None

  low = claude_relay.RelayWatch("main", FABLE)
  low.observe(_rate_limit("allowed_warning", 0.85))
  assert low.observe(_tool_result()) is False


def test_relay_watch_reports_a_login_failure_from_text_or_stderr() -> None:
  watch = claude_relay.RelayWatch("main", FABLE)
  watch.observe(_assistant("Failed to authenticate: OAuth session expired and could not be refreshed"))
  assert watch.decision(1, "") == claude_relay.LOGIN_FAILED
  assert watch.decision(0, "") is None, "a run that still exited 0 is not a login failure"
  assert claude_relay.RelayWatch("main", FABLE).decision(1, "Failed to authenticate") == claude_relay.LOGIN_FAILED


# ---------------------------------------------------------------------------
# _run_cc across accounts
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_run_cc_relays_a_rejected_turn_onto_another_account(tmp_path: Path, monkeypatch) -> None:
  cfg = _pool_cfg(tmp_path)
  _write_transcript(tmp_path / "claude-main")
  meta = _session_on("main")
  first = _ScriptedBackend([_rate_limit("rejected", 1.0), backend_base.make_result_event()], exit_code=1)
  second = _ScriptedBackend([backend_base.make_result_event()], exit_code=0)
  builds = _install_backends(monkeypatch, [first, second])
  item = make_work_item(cfg, meta, cfg.backend_options[0])

  cc_session_id, exit_code, error_msg, extras = await master_cc_run._run_cc(item)

  assert (cc_session_id, exit_code, error_msg) == (UUID, 0, None)
  assert extras["account_relays"] == 1
  assert builds[0]["option"].claude_config_dir == str(tmp_path / "claude-main")
  assert builds[0]["kwargs"]["extra_flags"] == ["--resume", UUID, "--exclude-dynamic-system-prompt-sections"]
  assert first.env is not None and first.env["CLAUDE_CONFIG_DIR"] is None if "CLAUDE_CONFIG_DIR" in first.env else True
  assert builds[1]["option"].claude_config_dir == str(tmp_path / "claude-ext-1")
  assert builds[1]["kwargs"]["extra_flags"] == ["--resume", UUID, "--exclude-dynamic-system-prompt-sections"]
  assert second.prompt == claude_relay.CONTINUATION_PROMPT
  assert meta.claude_account == "ext-1"
  assert claude_accounts.transcript_path(tmp_path / "claude-ext-1", UUID) is not None
  assert _events_of(item.callbacks, ET.ASSISTANT_ERROR) == []


@pytest.mark.asyncio
async def test_run_cc_terminates_at_the_safe_point_after_a_warning_and_relays(tmp_path: Path, monkeypatch) -> None:
  cfg = _pool_cfg(tmp_path)
  _write_transcript(tmp_path / "claude-main")
  meta = _session_on("main")
  first = _ScriptedBackend(
      [_rate_limit("allowed_warning", 0.92),
       _assistant("step 1"),
       _tool_result(),
       _assistant("never streamed")],
      exit_code=0)
  second = _ScriptedBackend([backend_base.make_result_event()], exit_code=0)
  builds = _install_backends(monkeypatch, [first, second])
  item = make_work_item(cfg, meta, cfg.backend_options[0])

  _cc, exit_code, error_msg, extras = await master_cc_run._run_cc(item)

  assert first.terminated is True
  assert (exit_code, error_msg, extras["account_relays"]) == (0, None, 1)
  assert len(builds) == 2 and builds[1]["option"].claude_config_dir == str(tmp_path / "claude-ext-1")
  assert second.prompt == claude_relay.CONTINUATION_PROMPT


@pytest.mark.asyncio
async def test_run_cc_reports_loudly_when_no_account_is_left(tmp_path: Path, monkeypatch) -> None:
  cfg = _pool_cfg(tmp_path)
  _write_credentials(tmp_path / "claude-ext-1", access_token="")
  _write_credentials(tmp_path / "claude-ext-2", access_token="")
  _write_transcript(tmp_path / "claude-main")
  meta = _session_on("main")
  first = _ScriptedBackend([_rate_limit("rejected", 1.0), backend_base.make_result_event()], exit_code=1)
  builds = _install_backends(monkeypatch, [first])
  item = make_work_item(cfg, meta, cfg.backend_options[0])

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
async def test_run_cc_stops_after_the_relay_limit(tmp_path: Path, monkeypatch) -> None:
  cfg = _pool_cfg(tmp_path, labels=("main", "a", "b", "c", "d"))
  _write_transcript(tmp_path / "claude-main")
  meta = _session_on("main")
  backends = [
      _ScriptedBackend([_rate_limit("rejected", 1.0), backend_base.make_result_event()], exit_code=1) for _ in range(5)
  ]
  builds = _install_backends(monkeypatch, backends)
  item = make_work_item(cfg, meta, cfg.backend_options[0])

  _cc, exit_code, error_msg, extras = await master_cc_run._run_cc(item)

  assert exit_code == 1
  assert "relay limit" in error_msg
  assert len(builds) == 1 + claude_relay.MAX_RELAYS_PER_TURN
  assert extras["account_relays"] == claude_relay.MAX_RELAYS_PER_TURN


@pytest.mark.asyncio
async def test_run_cc_marks_a_login_failure_and_relays(tmp_path: Path, monkeypatch) -> None:
  cfg = _pool_cfg(tmp_path)
  _write_transcript(tmp_path / "claude-main")
  meta = _session_on("main")
  first = _ScriptedBackend(
      [_assistant("Failed to authenticate: OAuth session expired and could not be refreshed")], exit_code=1)
  second = _ScriptedBackend([backend_base.make_result_event()], exit_code=0)
  builds = _install_backends(monkeypatch, [first, second])
  item = make_work_item(cfg, meta, cfg.backend_options[0])

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
    tmp_path: Path, monkeypatch, context_tokens: int, minutes_since: int, compacted: bool) -> None:
  cfg = _pool_cfg(tmp_path)
  _write_transcript(tmp_path / "claude-main")
  meta = _session_on("main")
  order: list[str] = []

  async def fake_compact(**kwargs):
    order.append(f"compact:{Path(kwargs['config_dir']).name}:{kwargs['pre_tokens']}")
    return True

  monkeypatch.setattr(master_cc_relay.claude_compaction, "compact_with_sonnet", fake_compact)
  backend = _ScriptedBackend([backend_base.make_result_event()], exit_code=0)
  builds: list[dict] = []

  def fake_build_backend(option, cfg_, **kwargs):
    order.append("spawn")
    builds.append({"option": option})
    return backend

  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, fake_build_backend)
  patch_instructions_content(monkeypatch)
  callbacks = dataclasses.replace(
      mock_session_callbacks(),
      claude_context_state=AsyncMock(
          return_value=(context_tokens, datetime.now(UTC) - timedelta(minutes=minutes_since))))
  item = make_work_item(cfg, meta, cfg.backend_options[0], callbacks=callbacks)

  _cc, exit_code, _err, _extras = await master_cc_run._run_cc(item)

  assert exit_code == 0
  if compacted:
    assert order == [f"compact:{Path(builds[0]['option'].claude_config_dir).name}:{context_tokens}", "spawn"]
  else:
    assert order == ["spawn"]


@pytest.mark.asyncio
async def test_run_cc_relay_compacts_a_large_fable_context_on_the_new_account(tmp_path: Path, monkeypatch) -> None:
  cfg = _pool_cfg(tmp_path)
  _write_transcript(tmp_path / "claude-main")
  meta = _session_on("main")
  compactions: list[tuple[str, int | None]] = []

  async def fake_compact(**kwargs):
    compactions.append((Path(kwargs["config_dir"]).name, kwargs["pre_tokens"]))
    return True

  monkeypatch.setattr(master_cc_relay.claude_compaction, "compact_with_sonnet", fake_compact)
  first = _ScriptedBackend([_rate_limit("rejected", 1.0), backend_base.make_result_event()], exit_code=1)
  second = _ScriptedBackend([backend_base.make_result_event()], exit_code=0)
  _install_backends(monkeypatch, [first, second])
  callbacks = dataclasses.replace(
      mock_session_callbacks(),
      claude_context_state=AsyncMock(return_value=(150_000, datetime.now(UTC) - timedelta(minutes=1))))
  item = make_work_item(cfg, meta, cfg.backend_options[0], callbacks=callbacks)

  _cc, exit_code, _err, _extras = await master_cc_run._run_cc(item)

  assert exit_code == 0
  assert compactions == [("claude-ext-1", 150_000)], "relay compaction runs in the new login, not at turn start"


@pytest.mark.asyncio
async def test_run_cc_without_a_pool_spawns_the_option_unchanged(tmp_path: Path, monkeypatch) -> None:
  cfg = CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backend_options=[
          BackendOption(
              id="pinned", label="Pinned", type="cc-claude", model=FABLE, claude_config_dir=str(tmp_path / "pinned"))
      ],
  )
  _write_transcript(tmp_path / "pinned")
  meta = SessionMetadata(id="s1", name="t", backend="pinned", cc_session_id=UUID)
  backend = _ScriptedBackend([_rate_limit("rejected", 1.0), backend_base.make_result_event()], exit_code=1)
  builds = _install_backends(monkeypatch, [backend])
  item = make_work_item(cfg, meta, cfg.backend_options[0])

  _cc, exit_code, _err, extras = await master_cc_run._run_cc(item)

  assert builds[0]["option"] is cfg.backend_options[0]
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


def test_usage_panel_entry_carries_the_login_directory_while_unhealthy(tmp_path: Path, monkeypatch) -> None:
  cfg = _pool_cfg(tmp_path)
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
