"""Claude account pool: membership, health, headroom, selection, transcript moves, and the pool's
reach into resume resolution, the backend-switch domain, the usage panel, and metadata persistence."""

from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest
from conftest import (
    FABLE_MODEL,
    POOLED_FABLE_ID,
    backend_option,
    fresh_state_fixture,
    make_transcript,
    make_work_item,
    mock_session_callbacks,
    pool_cfg,
    run_session_consumer,
    seed_transcript_copy,
    write_pool_credentials,
)

from src.backends.claude_code import claude_accounts, claude_relay
from src.infra import event_types as ET
from src.infra.config import CharlieBotConfig
from src.infra.models import SessionMetadata
from src.runtime import master_cc_state

NOW = datetime(2026, 9, 6, 20, 0, tzinfo=UTC)
SONNET = "claude-sonnet-5"

_fresh_pool_state = fresh_state_fixture(claude_accounts.reset_for_tests)


def _options(pool_name: str | None = None) -> list:
  extra = {} if pool_name is None else {"account_pool": pool_name}
  return [
      backend_option(id=POOLED_FABLE_ID, label="Fable", type="cc-claude", model=FABLE_MODEL, **extra),
      backend_option(id="claude-sonnet-5", label="Sonnet", type="cc-claude", model=SONNET, **extra),
      backend_option(id="codex-o3", label="Codex", type="codex", model="o3"),
  ]


def _pool_cfg(
    tmp_path: Path,
    labels: tuple[str, ...] = ("main", "ext-1", "ext-2"),
    pools: dict[str, list[str]] | None = None,
) -> CharlieBotConfig:
  options = _options(pool_name=next(iter(pools), None) if pools else None)
  return pool_cfg(
      tmp_path,
      options,
      home=tmp_path / "home",
      worktree_dir=tmp_path / "worktrees",
      labels=labels,
      claude_pools=pools,
  )


def _no_pool_cfg(tmp_path: Path) -> CharlieBotConfig:
  return CharlieBotConfig(charliebot_home=tmp_path / "home", backends={"options": _options()})


# ---------------------------------------------------------------------------
# Membership
# ---------------------------------------------------------------------------


def test_every_cc_claude_entry_is_pooled_when_the_pool_is_declared(tmp_path: Path) -> None:
  pooled_cfg = _pool_cfg(tmp_path)
  no_pool_cfg = _no_pool_cfg(tmp_path)
  fable = pooled_cfg.get_backend_option(POOLED_FABLE_ID)
  sonnet = pooled_cfg.get_backend_option("claude-sonnet-5")
  codex = pooled_cfg.get_backend_option("codex-o3")

  assert claude_accounts.is_pooled(fable, pooled_cfg) is True
  assert claude_accounts.is_pooled(sonnet, pooled_cfg) is True
  assert claude_accounts.is_pooled(codex, pooled_cfg) is False
  # No accounts.claude entries: no cc-claude entry is pooled.
  assert claude_accounts.is_pooled(fable, no_pool_cfg) is False


# ---------------------------------------------------------------------------
# Headroom
# ---------------------------------------------------------------------------


def _event(status: str, five_hour: float, seven_day: float, resets_at: float | None = None) -> dict:
  info = {
      "status": status,
      "rateLimitType": "five_hour",
      "unifiedWindows":
          {
              "five_hour": {
                  "utilization": five_hour
              },
              "seven_day": {
                  "utilization": seven_day
              },
              "seven_day_overage_included": {
                  "utilization": 0.99
              },
          },
  }
  if resets_at is not None:
    info["resetsAt"] = resets_at
  return info


# ---------------------------------------------------------------------------
# Selection
# ---------------------------------------------------------------------------


def test_select_skips_excluded_rejected_and_unhealthy_accounts(tmp_path: Path) -> None:
  cfg = _pool_cfg(tmp_path)
  claude_accounts.observe_rate_limit("main", _event("allowed", 0.30, 0.10), now=NOW)
  claude_accounts.observe_rate_limit(
      "ext-1", _event("rejected", 1.0, 0.10, (NOW + timedelta(hours=1)).timestamp()), now=NOW)
  write_pool_credentials(tmp_path / "claude-ext-2", access_token="")  # emptied credential store

  assert claude_accounts.select(cfg, FABLE_MODEL, exclude={"main"}, now=NOW) is None
  assert claude_accounts.select(cfg, FABLE_MODEL, now=NOW).label == "main"
  assert claude_accounts.earliest_reset(cfg, now=NOW) == NOW + timedelta(hours=1)


# ---------------------------------------------------------------------------
# Named pools
# ---------------------------------------------------------------------------


def test_select_considers_only_the_named_pools_accounts(tmp_path: Path) -> None:
  """The pool bounds the candidates: an outside account with more headroom is not chosen,
  and with no pool named every account still contends."""
  cfg = _pool_cfg(tmp_path, pools={"alpha": ["main"], "beta": ["ext-1", "ext-2"]})
  # ext-1 sits on a full window, main on half: across all accounts ext-1 wins.
  claude_accounts.observe_rate_limit("main", _event("allowed", 0.50, 0.10), now=NOW)

  assert claude_accounts.select(cfg, FABLE_MODEL, now=NOW).label == "ext-1"
  assert claude_accounts.select(cfg, FABLE_MODEL, now=NOW, account_pool="alpha").label == "main"


def test_a_label_in_two_pools_loads_and_selects_from_either(tmp_path: Path) -> None:
  """One label may sit in several pools; each pool selects among its own members alone."""
  cfg = _pool_cfg(tmp_path, pools={"alpha": ["main", "ext-1"], "beta": ["ext-1", "ext-2"]})
  # ext-1 is the shared label; both its pool-mates sit on a full window, so each
  # pool's only live account is the one the other pool does not hold.
  claude_accounts.observe_rate_limit(
      "main", _event("rejected", 1.0, 0.10, (NOW + timedelta(hours=1)).timestamp()), now=NOW)
  claude_accounts.observe_rate_limit(
      "ext-2", _event("rejected", 1.0, 0.10, (NOW + timedelta(hours=2)).timestamp()), now=NOW)

  assert claude_accounts.select(cfg, FABLE_MODEL, now=NOW, account_pool="alpha").label == "ext-1"
  assert claude_accounts.select(cfg, FABLE_MODEL, now=NOW, account_pool="beta").label == "ext-1"


def test_relay_stays_inside_the_pool_and_names_it_when_exhausted(tmp_path: Path) -> None:
  """relay_move picks the next account inside the pool only; a healthy account in
  another pool stays unused, and exhaustion names the pool and its earliest reset."""
  cfg = _pool_cfg(tmp_path, pools={"alpha": ["main", "ext-1"], "beta": ["ext-2"]})
  make_transcript(tmp_path / "claude-main", "uuid-1")
  current = claude_accounts.account_by_label(cfg, "main")
  claude_accounts.observe_rate_limit("main", _event("allowed", 0.80, 0.10), now=NOW)

  move = claude_relay.relay_move(cfg, FABLE_MODEL, current, "uuid-1", now=NOW, account_pool="alpha")

  assert (move.account.label, move.error, move.refused_holder) == ("ext-1", None, None)

  claude_accounts.observe_rate_limit(
      "ext-1", _event("rejected", 1.0, 0.10, (NOW + timedelta(hours=1)).timestamp()), now=NOW)

  move = claude_relay.relay_move(cfg, FABLE_MODEL, current, "uuid-1", now=NOW, account_pool="alpha")

  assert (move.account, move.refused_holder) == (None, None)
  assert move.error == (
      "Claude account pool has no available account (pool 'alpha'; earliest reset 21:00 UTC); "
      "this run did not complete.")


def test_relay_exhaustion_without_pools_keeps_the_unnamed_message(tmp_path: Path) -> None:
  cfg = _pool_cfg(tmp_path)
  make_transcript(tmp_path / "claude-main", "uuid-1")
  current = claude_accounts.account_by_label(cfg, "main")
  for label in ("ext-1", "ext-2"):
    claude_accounts.observe_rate_limit(
        label, _event("rejected", 1.0, 0.10, (NOW + timedelta(hours=1)).timestamp()), now=NOW)

  move = claude_relay.relay_move(cfg, FABLE_MODEL, current, "uuid-1", now=NOW)

  assert move.account is None
  assert move.error == claude_relay.pool_exhausted_message(cfg, NOW)
  assert move.error == (
      "Claude account pool has no available account (earliest reset 21:00 UTC); "
      "this run did not complete.")


def test_pool_exhaustion_names_the_pool_even_without_a_reset_time(tmp_path: Path) -> None:
  """A pool exhausted with no rejection reading -- every login emptied, say -- still names
  the pool; the reset slot reads 'no reset time known'."""
  cfg = _pool_cfg(tmp_path, pools={"alpha": ["main", "ext-1"]})
  write_pool_credentials(tmp_path / "claude-main", access_token="")
  write_pool_credentials(tmp_path / "claude-ext-1", access_token="")
  current = claude_accounts.account_by_label(cfg, "main")

  move = claude_relay.relay_move(cfg, FABLE_MODEL, current, "uuid-1", now=NOW, account_pool="alpha")

  assert (move.account, move.refused_holder) == (None, None)
  assert move.error == (
      "Claude account pool has no available account (pool 'alpha'; no reset time known); "
      "this run did not complete.")


# ---------------------------------------------------------------------------
# Transcripts
# ---------------------------------------------------------------------------


def test_move_transcript_copies_conversation_and_sidecar_into_the_same_slug(tmp_path: Path) -> None:
  src_dir = tmp_path / "claude-ext-2"
  dst_dir = tmp_path / "claude-main"
  transcript = make_transcript(src_dir, "uuid-1")
  sidecar = transcript.with_suffix("") / "tool-results"
  sidecar.mkdir(parents=True)
  (sidecar / "r.txt").write_text("result", encoding="utf-8")

  moved = claude_accounts.move_transcript("uuid-1", src_dir, dst_dir)

  assert moved == dst_dir / "projects" / transcript.parent.name / "uuid-1.jsonl"
  assert moved.read_text(encoding="utf-8") == transcript.read_text(encoding="utf-8")
  assert (moved.with_suffix("") / "tool-results" / "r.txt").read_text(encoding="utf-8") == "result"
  assert transcript.exists(), "the source copy stays for fallback"
  assert claude_accounts.transcript_path(dst_dir, "uuid-1") == moved


# ---------------------------------------------------------------------------
# Metadata persistence
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_consumer_persists_the_account_the_run_settled_on(tmp_path: Path) -> None:
  cfg = _no_pool_cfg(tmp_path)
  session_meta = SessionMetadata(profile="manager", id="consumer-account", name="t", backend=POOLED_FABLE_ID)
  callbacks = mock_session_callbacks()
  callbacks.persist_account_label.side_effect = lambda sid, label: "other"
  item = make_work_item(cfg, session_meta, cfg.backends.options[0], callbacks=callbacks)

  async def fake_run_cc(work_item: master_cc_state._WorkItem) -> tuple[str | None, int, str | None, dict]:
    work_item.session_meta.claude_account = "ext-1"
    return "uuid-8", 0, None, {}

  await run_session_consumer(session_meta.id, [item], fake_run_cc)

  callbacks.persist_account_label.assert_awaited_once_with(session_meta.id, "ext-1")
  errors = [
      call.args[1]
      for call in callbacks.persist_and_broadcast.await_args_list
      if call.args[1].get("type") == ET.ERROR and call.args[1].get("source") == "claude_account"
  ]
  assert len(errors) == 1, "a read-back that disagrees with the write is reported"


# ---------------------------------------------------------------------------
# The move guard (refuse to overwrite a newer copy)
# ---------------------------------------------------------------------------


def test_move_transcript_refuses_to_overwrite_a_strictly_newer_destination(tmp_path: Path) -> None:
  src_dir, dst_dir = tmp_path / "claude-main", tmp_path / "claude-ext-1"
  src = seed_transcript_copy(src_dir, "uuid-1", '{"stale": true}\n', mtime_ns=1_000)
  dst = seed_transcript_copy(dst_dir, "uuid-1", '{"stale": true}\n{"live": true}\n', mtime_ns=2_000)

  with pytest.raises(claude_accounts.TranscriptMoveError) as excinfo:
    claude_accounts.move_transcript("uuid-1", src_dir, dst_dir)

  message = str(excinfo.value)
  assert claude_accounts.GUARD_REFUSAL_MARKER in message
  assert str(dst) in message and str(src) in message
  dst_body, src_body = dst.read_text(encoding="utf-8"), src.read_text(encoding="utf-8")
  assert f"size {len(dst_body)}" in message.split("vs src")[0]
  assert f"size {len(src_body)}" in message.split("vs src")[1]
  # The refusal moves nothing: both copies stand exactly as they were.
  assert src.read_text(encoding="utf-8") == '{"stale": true}\n'
  assert dst.read_text(encoding="utf-8") == '{"stale": true}\n{"live": true}\n'
