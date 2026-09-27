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

from src.agents import master_cc_state
from src.core import claude_accounts
from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.models import (
    SessionMetadata,
)

NOW = datetime(2026, 9, 6, 20, 0, tzinfo=UTC)
SONNET = "claude-sonnet-5"

_fresh_pool_state = fresh_state_fixture(claude_accounts.reset_for_tests)


def _options() -> list:
  return [
      backend_option(id=POOLED_FABLE_ID, label="Fable", type="cc-claude", model=FABLE_MODEL),
      backend_option(id="claude-sonnet-5", label="Sonnet", type="cc-claude", model=SONNET),
      backend_option(id="codex-o3", label="Codex", type="codex", model="o3"),
  ]


def _pool_cfg(tmp_path: Path, labels: tuple[str, ...] = ("main", "ext-1", "ext-2")) -> CharlieBotConfig:
  return pool_cfg(
      tmp_path,
      _options(),
      home=tmp_path / "home",
      worktree_dir=tmp_path / "worktrees",
      labels=labels,
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
# Health


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
# Panel expiry: the shared predicate and the pool fold that drops expired windows


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
# Resume resolution through the pool


# ---------------------------------------------------------------------------
# Backend-switch domain


# ---------------------------------------------------------------------------
# Directory derivations: usage panel, token tally, cold storage


# ---------------------------------------------------------------------------
# Metadata persistence
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_consumer_persists_the_account_the_run_settled_on(tmp_path: Path) -> None:
  cfg = _no_pool_cfg(tmp_path)
  session_meta = SessionMetadata(id="consumer-account", name="t", backend=POOLED_FABLE_ID)
  callbacks = mock_session_callbacks()
  callbacks.persist_claude_account.side_effect = lambda sid, label: "other"
  item = make_work_item(cfg, session_meta, cfg.backends.options[0], callbacks=callbacks)

  async def fake_run_cc(work_item: master_cc_state._WorkItem) -> tuple[str | None, int, str | None, dict]:
    work_item.session_meta.claude_account = "ext-1"
    return "uuid-8", 0, None, {}

  await run_session_consumer(session_meta.id, [item], fake_run_cc)

  callbacks.persist_claude_account.assert_awaited_once_with(session_meta.id, "ext-1")
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


# ---------------------------------------------------------------------------
# Copy retirement after a sound round


# ---------------------------------------------------------------------------
# The placement probe's lineage helpers
