"""Claude account pool: membership, health, headroom, selection, transcript moves, and the pool's
reach into resume resolution, the backend-switch domain, the usage panel, and metadata persistence."""

import asyncio
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
from structlog.testing import capture_logs

from src.agents import master_cc_run, master_cc_state
from src.api import ext_usage as ext_usage_mod
from src.api.sessions import _active_backend_payload, _backend_domain
from src.core import claude_accounts, storage_cool, token_tally
from src.core import config as core_config
from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.models import (
    ClaudeAccount,
    CreateSessionRequest,
    SessionMetadata,
)
from src.core.sessions import SessionManager

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


def test_pool_expands_config_dirs(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setenv("HOME", str(tmp_path))
  cfg = CharlieBotConfig(
      charliebot_home=tmp_path / "home", accounts={"claude": [ClaudeAccount(label="main", config_dir="~/.claude")]})

  assert claude_accounts.pool(cfg)[0].config_dir == str(tmp_path / ".claude")
  assert claude_accounts.account_by_label(cfg, "missing") is None


def test_load_config_reads_pool_keys(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  home = tmp_path / "charliebot"
  home.mkdir()
  (home / "config.yaml").write_text(
      """
accounts:
  claude:
    - label: main
      config_dir: ~/.claude
    - label: ext-1
      config_dir: ~/.claude-ext-1
  claude_compaction:
    relay_tokens: 120000
    expired_cache_tokens: 60000
backends:
  options:
    - id: claude-fable-5
      label: Fable
      type: cc-claude
      model: claude-fable-5-1
""",
      encoding="utf-8")
  monkeypatch.setenv(core_config.CHARLIEBOT_HOME_ENV, str(home))

  cfg = core_config.load_config()

  assert [account.label for account in cfg.accounts.claude] == ["main", "ext-1"]
  assert cfg.accounts.claude_compaction.relay_tokens == 120000
  assert cfg.accounts.claude_compaction.expired_cache_tokens == 60000
  assert cfg.backends.options[0].id == "claude-fable-5"


def test_config_defaults_carry_no_pool_and_default_floors(tmp_path: Path) -> None:
  cfg = _no_pool_cfg(tmp_path)

  assert cfg.accounts.claude == []
  assert (cfg.accounts.claude_compaction.relay_tokens,
          cfg.accounts.claude_compaction.expired_cache_tokens) == (100_000, 50_000)


# ---------------------------------------------------------------------------
# Health
# ---------------------------------------------------------------------------


def test_credentials_present_requires_a_non_empty_access_token(tmp_path: Path) -> None:
  account = ClaudeAccount(label="a", config_dir=str(tmp_path / "a"))
  assert claude_accounts.credentials_present(account) is False  # no file

  write_pool_credentials(tmp_path / "a", access_token="")
  assert claude_accounts.credentials_present(account) is False  # emptied by a failed refresh

  (tmp_path / "a" / claude_accounts.CREDENTIALS_FILE).write_text("not json", encoding="utf-8")
  assert claude_accounts.credentials_present(account) is False

  write_pool_credentials(tmp_path / "a")
  assert claude_accounts.credentials_present(account) is True


def test_healthy_excludes_an_account_inside_the_auth_failure_cooldown(tmp_path: Path) -> None:
  cfg = _pool_cfg(tmp_path)
  ext1 = claude_accounts.account_by_label(cfg, "ext-1")

  claude_accounts.record_auth_failure("ext-1", now=NOW)
  assert claude_accounts.healthy(ext1, now=NOW + timedelta(minutes=14)) is False
  assert claude_accounts.healthy(ext1, now=NOW + timedelta(minutes=16)) is True


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


def test_observe_rate_limit_folds_the_binding_windows_only() -> None:
  reading = claude_accounts.observe_rate_limit("ext-1", _event("allowed_warning", 0.92, 0.30), now=NOW)

  assert reading is not None
  assert reading.utilization == pytest.approx(0.92)  # the overage window (0.99) is not a limit
  assert reading.rejected_until is None
  assert claude_accounts.headroom("ext-1", FABLE_MODEL, now=NOW) == pytest.approx(0.08)
  assert claude_accounts.observe_rate_limit("ext-1", {"status": "allowed"}, now=NOW) is None


def test_rejected_reading_zeroes_headroom_until_its_reset() -> None:
  resets_at = (NOW + timedelta(minutes=30)).timestamp()
  claude_accounts.observe_rate_limit("ext-1", _event("rejected", 0.99, 0.40, resets_at), now=NOW)

  assert claude_accounts.headroom("ext-1", FABLE_MODEL, now=NOW) == 0.0
  assert claude_accounts.headroom("ext-1", FABLE_MODEL, now=NOW + timedelta(minutes=31)) == pytest.approx(0.01)


def test_unread_account_has_full_headroom() -> None:
  assert claude_accounts.headroom("never-read", FABLE_MODEL, now=NOW) == 1.0


def test_headroom_takes_the_newer_of_event_and_panel_readings() -> None:
  claude_accounts.observe_rate_limit("ext-1", _event("allowed", 0.20, 0.10), now=NOW)
  claude_accounts.observe_usage_panel(
      "ext-1", {
          "windows": [{
              "window_minutes": 300,
              "utilization": 70.0
          }, {
              "window_minutes": 10080,
              "utilization": 10.0
          }],
          "fetched_at": (NOW + timedelta(minutes=5)).isoformat(),
      })
  assert claude_accounts.headroom("ext-1", FABLE_MODEL, now=NOW + timedelta(minutes=6)) == pytest.approx(0.30)

  claude_accounts.observe_rate_limit("ext-1", _event("allowed", 0.50, 0.10), now=NOW + timedelta(minutes=10))
  assert claude_accounts.headroom("ext-1", FABLE_MODEL, now=NOW + timedelta(minutes=11)) == pytest.approx(0.50)


def test_panel_scoped_window_counts_only_for_its_model_family() -> None:
  claude_accounts.observe_usage_panel(
      "ext-1", {
          "windows":
              [
                  {
                      "window_minutes": 300,
                      "utilization": 10.0
                  },
                  {
                      "window_minutes": 10080,
                      "utilization": 60.0,
                      "scope_label": "Fable 5.1"
                  },
              ],
          "fetched_at": NOW.isoformat(),
      })

  assert claude_accounts.headroom("ext-1", FABLE_MODEL, now=NOW) == pytest.approx(0.40)
  assert claude_accounts.headroom("ext-1", SONNET, now=NOW) == pytest.approx(0.90)


def test_headroom_fuses_a_live_scoped_panel_window_with_a_newer_event_reading() -> None:
  """The Fable weekly bucket survives a newer generic reading: 85 percent weekly keeps
  pressing the headroom even though the 5h event (20 percent) is the newer reading."""
  claude_accounts.observe_usage_panel(
      "ext-1", {
          "windows":
              [
                  {
                      "window_minutes": 300,
                      "utilization": 20.0,
                      "resets_at": (NOW + timedelta(hours=2)).isoformat(),
                  },
                  {
                      "window_minutes": 10080,
                      "utilization": 85.0,
                      "scope_label": "Fable 5.1",
                      "resets_at": (NOW + timedelta(days=4)).isoformat(),
                  },
              ],
          "fetched_at": (NOW - timedelta(minutes=10)).isoformat(),
      })
  claude_accounts.observe_rate_limit("ext-1", _event("allowed", 0.20, 0.10), now=NOW)

  reading = claude_accounts.latest_reading("ext-1", FABLE_MODEL, now=NOW)
  assert reading is not None
  assert reading.utilization == pytest.approx(0.85)
  assert reading.at == NOW, "the newer general reading stamps the fusion"
  assert claude_accounts.headroom("ext-1", FABLE_MODEL, now=NOW) == pytest.approx(0.15)
  # Without the scoped bucket the newer generic reading is the whole story.
  assert claude_accounts.headroom("ext-1", SONNET, now=NOW) == pytest.approx(0.80)


def test_fused_reading_keeps_a_live_rejection_from_the_event_reading() -> None:
  claude_accounts.observe_usage_panel(
      "ext-1", {
          "windows":
              [
                  {
                      "window_minutes": 10080,
                      "utilization": 85.0,
                      "scope_label": "Fable 5.1",
                      "resets_at": (NOW + timedelta(days=4)).isoformat(),
                  }
              ],
          "fetched_at": (NOW - timedelta(minutes=10)).isoformat(),
      })
  resets_at = (NOW + timedelta(minutes=30)).timestamp()
  claude_accounts.observe_rate_limit("ext-1", _event("rejected", 0.99, 0.40, resets_at), now=NOW)

  # The rejection holds the fusion to zero until its reset passes; afterwards the
  # rejected event's own utilization (0.99) is what presses the headroom.
  assert claude_accounts.headroom("ext-1", FABLE_MODEL, now=NOW) == 0.0
  assert claude_accounts.headroom("ext-1", FABLE_MODEL, now=NOW + timedelta(minutes=31)) == pytest.approx(0.01)


def test_fusion_falls_back_past_an_expired_scoped_window() -> None:
  fetched_at = EXPIRY_NOW - timedelta(hours=2)
  claude_accounts.observe_usage_panel(
      "ext-1", {
          "windows":
              [
                  {
                      "window_minutes": 10080,
                      "utilization": 85.0,
                      "scope_label": "Fable 5.1",
                      "resets_at": (EXPIRY_NOW - timedelta(hours=1)).isoformat(),
                  },
                  {
                      "window_minutes": 10080,
                      "utilization": 17.0,
                      "resets_at": (EXPIRY_NOW + timedelta(days=3)).isoformat(),
                  },
              ],
          "fetched_at": fetched_at.isoformat(),
      })
  claude_accounts.observe_rate_limit("ext-1", _event("allowed", 0.50, 0.10), now=EXPIRY_NOW)

  # The scoped weekly window's reset has passed: it stops pressing the headroom
  # and the newer generic event reading is the only one left.
  assert claude_accounts.headroom("ext-1", FABLE_MODEL, now=EXPIRY_NOW) == pytest.approx(0.50)


# ---------------------------------------------------------------------------
# Panel expiry: the shared predicate and the pool fold that drops expired windows
# ---------------------------------------------------------------------------

EXPIRY_NOW = datetime(2026, 9, 10, 16, 0, tzinfo=UTC)


def _dead_5h_window() -> dict:
  """The dead 5h panel window: expired one hour before EXPIRY_NOW at 91 percent."""
  return {
      "window_minutes": 300,
      "utilization": 91.0,
      "resets_at": (EXPIRY_NOW - timedelta(hours=1)).isoformat(),
  }


def test_panel_window_expired_when_the_reset_passed_after_the_sample() -> None:
  window = _dead_5h_window()
  sampled = EXPIRY_NOW - timedelta(hours=2)

  assert claude_accounts.panel_window_expired(window, sampled, EXPIRY_NOW) is True
  # The same sample with the reset still ahead is live.
  assert claude_accounts.panel_window_expired(window, sampled, EXPIRY_NOW - timedelta(hours=3)) is False


def test_panel_window_expired_when_the_sample_is_older_than_the_window() -> None:
  window = {"window_minutes": 300, "utilization": 42.0, "resets_at": ""}

  assert claude_accounts.panel_window_expired(window, EXPIRY_NOW - timedelta(hours=6), EXPIRY_NOW) is True
  assert claude_accounts.panel_window_expired(window, EXPIRY_NOW - timedelta(minutes=5), EXPIRY_NOW) is False


def test_panel_window_expired_ignores_an_illegal_reset_and_falls_to_the_age_rule() -> None:
  window = {"window_minutes": 10080, "utilization": 17.0, "resets_at": "not-a-timestamp"}

  assert claude_accounts.panel_window_expired(window, EXPIRY_NOW - timedelta(minutes=5), EXPIRY_NOW) is False
  assert claude_accounts.panel_window_expired(window, EXPIRY_NOW - timedelta(days=8), EXPIRY_NOW) is True


def test_panel_window_expired_reads_a_missing_sample_as_live() -> None:
  window = _dead_5h_window()

  assert claude_accounts.panel_window_expired(window, None, EXPIRY_NOW) is False


def test_panel_fold_drops_an_expired_window_whatever_its_utilization() -> None:
  """Pool invariance: an expired window's number is free to vary — the headroom is
  the dropped-window value either way (incident shape: dead 5h beside a live 7d)."""
  fetched_at = EXPIRY_NOW - timedelta(hours=2)
  dead = _dead_5h_window()
  live = {
      "window_minutes": 10080,
      "utilization": 17.0,
      "resets_at": (EXPIRY_NOW + timedelta(days=3)).isoformat(),
  }
  claude_accounts.observe_usage_panel("ext-1", {"windows": [dict(live)], "fetched_at": fetched_at.isoformat()})
  dropped = claude_accounts.headroom("ext-1", FABLE_MODEL, now=EXPIRY_NOW)

  for utilization in (0.0, 50.0, 91.0, 100.0):
    claude_accounts.observe_usage_panel(
        "ext-1", {
            "windows": [dict(dead, utilization=utilization), dict(live)],
            "fetched_at": fetched_at.isoformat(),
        })
    assert claude_accounts.headroom("ext-1", FABLE_MODEL, now=EXPIRY_NOW) == pytest.approx(dropped)

  assert dropped == pytest.approx(0.83)


def test_panel_fold_all_windows_expired_falls_back_past_the_panel() -> None:
  fetched_at = (EXPIRY_NOW - timedelta(hours=2)).isoformat()
  dead = _dead_5h_window()
  claude_accounts.observe_usage_panel("ext-1", {"windows": [dead], "fetched_at": fetched_at})

  # No event reading: with no panel reading either, the account scores a full window.
  assert claude_accounts.latest_reading("ext-1", FABLE_MODEL, now=EXPIRY_NOW) is None
  assert claude_accounts.headroom("ext-1", FABLE_MODEL, now=EXPIRY_NOW) == 1.0

  # An event reading survives as the only candidate.
  claude_accounts.observe_rate_limit("ext-1", _event("allowed", 0.50, 0.10), now=EXPIRY_NOW)
  reading = claude_accounts.latest_reading("ext-1", FABLE_MODEL, now=EXPIRY_NOW)
  assert reading is not None
  assert reading.utilization == pytest.approx(0.50)


def test_panel_fold_keeps_a_live_weekly_window_whose_sample_has_aged() -> None:
  """7d semantics: a weekly reading with its reset still ahead stays live even when
  the sample has aged hours — a week bucket does not evaporate by the hour."""
  weekly = {
      "window_minutes": 10080,
      "utilization": 60.0,
      "resets_at": (EXPIRY_NOW + timedelta(days=2)).isoformat(),
  }
  claude_accounts.observe_usage_panel(
      "ext-1", {
          "windows": [weekly],
          "fetched_at": (EXPIRY_NOW - timedelta(hours=6)).isoformat()
      })

  assert claude_accounts.headroom("ext-1", FABLE_MODEL, now=EXPIRY_NOW) == pytest.approx(0.40)


# ---------------------------------------------------------------------------
# Selection
# ---------------------------------------------------------------------------


def test_select_prefers_most_headroom_and_breaks_near_ties_by_lru(tmp_path: Path) -> None:
  cfg = _pool_cfg(tmp_path)
  claude_accounts.observe_rate_limit("main", _event("allowed", 0.50, 0.10), now=NOW)
  claude_accounts.observe_rate_limit("ext-1", _event("allowed", 0.05, 0.05), now=NOW)
  claude_accounts.observe_rate_limit("ext-2", _event("allowed", 0.10, 0.05), now=NOW - timedelta(minutes=10))

  # The clear headroom leader wins whatever the tie-break state.
  assert claude_accounts.select(cfg, FABLE_MODEL, now=NOW).label == "ext-1"

  # A near-tie (within 0.02) breaks by least-recent use: ext-2's event
  # reading is the older one, so it is tried first.
  claude_accounts.observe_rate_limit("ext-1", _event("allowed", 0.115, 0.05), now=NOW)
  assert claude_accounts.select(cfg, FABLE_MODEL, now=NOW).label == "ext-2"


def test_select_tries_a_never_active_account_before_a_near_tied_active_one(tmp_path: Path) -> None:
  cfg = _pool_cfg(tmp_path, labels=("main", "ext-1"))
  claude_accounts.observe_rate_limit("main", _event("allowed", 0.015, 0.01), now=NOW)

  # ext-1 has no reading at all: within the tie band it sorts before main's
  # fresh reading, so the untouched login is tried first.
  assert claude_accounts.select(cfg, FABLE_MODEL, now=NOW).label == "ext-1"


def test_select_keeps_pool_order_when_scores_and_activity_tie(tmp_path: Path) -> None:
  cfg = _pool_cfg(tmp_path)
  # No readings anywhere: every account scores a full window and none has been active.
  assert claude_accounts.select(cfg, FABLE_MODEL, now=NOW).label == "main"


def test_select_skips_busy_accounts_while_any_idle_account_remains(tmp_path: Path) -> None:
  cfg = _pool_cfg(tmp_path, labels=("ext-1", "ext-2"))
  claude_accounts.observe_rate_limit("ext-1", _event("allowed", 0.20, 0.10), now=NOW)
  claude_accounts.observe_rate_limit("ext-2", _event("allowed", 0.30, 0.10), now=NOW)

  # ext-1 has more headroom but another session holds it: the idle account wins.
  assert claude_accounts.select(cfg, FABLE_MODEL, busy_accounts={"ext-1"}, now=NOW).label == "ext-2"
  # Every healthy account busy: the hard slot filter falls back to the full set.
  assert claude_accounts.select(cfg, FABLE_MODEL, busy_accounts={"ext-1", "ext-2"}, now=NOW).label == "ext-1"


def test_select_prefers_an_account_whose_window_resets_sooner(tmp_path: Path) -> None:
  cfg = _pool_cfg(tmp_path, labels=("main", "ext-1"))
  claude_accounts.observe_usage_panel(
      "main", {
          "windows":
              [{
                  "window_minutes": 10080,
                  "utilization": 60.0,
                  "resets_at": (NOW + timedelta(hours=3)).isoformat(),
              }],
          "fetched_at": NOW.isoformat(),
      })
  claude_accounts.observe_usage_panel(
      "ext-1", {
          "windows":
              [{
                  "window_minutes": 10080,
                  "utilization": 55.0,
                  "resets_at": (NOW + timedelta(days=5)).isoformat(),
              }],
          "fetched_at": NOW.isoformat(),
      })

  # main: headroom 0.40 plus 0.0875 of reset bonus (3h to reset); ext-1: 0.45,
  # too far from its reset to earn a bonus -- the bonus flips the ranking.
  assert claude_accounts.select(cfg, FABLE_MODEL, now=NOW).label == "main"

  # Push main's reset beyond the bonus horizon and plain headroom rules again.
  claude_accounts.observe_usage_panel(
      "main", {
          "windows":
              [{
                  "window_minutes": 10080,
                  "utilization": 60.0,
                  "resets_at": (NOW + timedelta(days=5)).isoformat(),
              }],
          "fetched_at": NOW.isoformat(),
      })
  assert claude_accounts.select(cfg, FABLE_MODEL, now=NOW).label == "ext-1"


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


def test_move_transcript_without_a_source_raises(tmp_path: Path) -> None:
  with pytest.raises(claude_accounts.TranscriptMoveError):
    claude_accounts.move_transcript("uuid-x", tmp_path / "a", tmp_path / "b")


def test_find_transcript_account_scans_the_pool(tmp_path: Path) -> None:
  cfg = _pool_cfg(tmp_path)
  make_transcript(tmp_path / "claude-ext-2", "uuid-2")

  assert claude_accounts.find_transcript_account(cfg, "uuid-2").label == "ext-2"
  assert claude_accounts.find_transcript_account(cfg, "uuid-9") is None


# ---------------------------------------------------------------------------
# Resume resolution through the pool
# ---------------------------------------------------------------------------


def test_resolve_resume_id_prefers_own_account_then_searches_pool_and_writes_back(tmp_path: Path) -> None:
  cfg = _pool_cfg(tmp_path)
  fable = cfg.get_backend_option(POOLED_FABLE_ID)
  make_transcript(tmp_path / "claude-ext-2", "uuid-3")

  meta = SessionMetadata(id="s1", name="t", backend=POOLED_FABLE_ID, cc_session_id="uuid-3")
  assert master_cc_run._resolve_resume_id(fable, meta, cfg=cfg) == "uuid-3"
  assert meta.claude_account == "ext-2"

  # A transcript present under the recorded account is taken from there, even when
  # another login also holds a copy (the source copy left behind by a relay).
  make_transcript(tmp_path / "claude-main", "uuid-3")
  meta_main = SessionMetadata(id="s1", name="t", backend=POOLED_FABLE_ID, cc_session_id="uuid-3", claude_account="main")
  assert master_cc_run._resolve_resume_id(fable, meta_main, cfg=cfg) == "uuid-3"
  assert meta_main.claude_account == "main"


def test_resolve_resume_id_pool_miss_returns_none_and_keeps_account(tmp_path: Path) -> None:
  cfg = _pool_cfg(tmp_path)
  fable = cfg.get_backend_option(POOLED_FABLE_ID)
  meta = SessionMetadata(id="s1", name="t", backend=POOLED_FABLE_ID, cc_session_id="uuid-4", claude_account="ext-1")

  assert master_cc_run._resolve_resume_id(fable, meta, cfg=cfg) is None
  assert meta.claude_account == "ext-1"


# ---------------------------------------------------------------------------
# Backend-switch domain
# ---------------------------------------------------------------------------


def test_pooled_entries_share_one_switch_domain(tmp_path: Path) -> None:
  cfg = _pool_cfg(tmp_path)

  assert _backend_domain(cfg.get_backend_option(POOLED_FABLE_ID), cfg) == claude_accounts.POOL_DOMAIN
  assert _backend_domain(cfg.get_backend_option("claude-sonnet-5"), cfg) == claude_accounts.POOL_DOMAIN
  assert _backend_domain(cfg.get_backend_option("codex-o3"), cfg) is None

  pooled = _active_backend_payload(SessionMetadata(id="a", name="t", backend=POOLED_FABLE_ID), cfg)
  assert pooled["switchable_backends"] == ["claude-fable-5", "claude-sonnet-5"]


# ---------------------------------------------------------------------------
# Directory derivations: usage panel, token tally, cold storage
# ---------------------------------------------------------------------------


def test_usage_panel_accounts_are_the_default_dir_plus_the_pool_labels(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _pool_cfg(tmp_path)
  monkeypatch.setattr(ext_usage_mod, "get_config", lambda: cfg)

  accounts = ext_usage_mod._derive_accounts()["claude"]

  assert accounts[0] == ("main", ext_usage_mod.CLAUDE_DEFAULT_DIR)
  assert [label for label, _ in accounts] == ["main", "ext-1", "ext-2"]
  assert dict(accounts)["ext-1"] == str(tmp_path / "claude-ext-1")


def test_token_tally_and_cold_storage_include_pool_directories(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _pool_cfg(tmp_path)
  for label in ("main", "ext-1", "ext-2"):
    (tmp_path / f"claude-{label}" / "projects").mkdir(parents=True, exist_ok=True)
  monkeypatch.setattr(token_tally, "get_config", lambda: cfg)

  claude_map, _codex_map = token_tally.discover_homes(tmp_path / ".claude", tmp_path / ".codex")
  assert set(claude_map.values()) >= {tmp_path / "claude-main", tmp_path / "claude-ext-1", tmp_path / "claude-ext-2"}

  roots = storage_cool.claude_projects_roots(cfg)
  assert tmp_path / "claude-ext-2" / "projects" in roots


# ---------------------------------------------------------------------------
# Metadata persistence
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_persist_claude_account_round_trips_without_touching_other_fields(tmp_path: Path) -> None:
  cfg = _no_pool_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = await session_mgr.create_session(CreateSessionRequest(name="t"), backend=POOLED_FABLE_ID)
  await session_mgr.persist_cc_session_id(meta.id, "uuid-7")

  assert await session_mgr.persist_claude_account(meta.id, "ext-1") == "ext-1"
  fresh = await session_mgr.read_metadata_fresh(meta.id)
  assert fresh.claude_account == "ext-1"
  assert fresh.cc_session_id == "uuid-7"
  assert await session_mgr.persist_claude_account("missing-session", "ext-1") is None


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


@pytest.mark.asyncio
async def test_consumer_skips_account_persistence_when_no_account_was_assigned(tmp_path: Path) -> None:
  cfg = _no_pool_cfg(tmp_path)
  session_meta = SessionMetadata(id="consumer-none", name="t", backend=POOLED_FABLE_ID)
  callbacks = mock_session_callbacks()
  item = make_work_item(cfg, session_meta, cfg.backends.options[0], callbacks=callbacks)

  async def fake_run_cc(work_item: master_cc_state._WorkItem) -> tuple[str | None, int, str | None, dict]:
    return "uuid-9", 0, None, {}

  await run_session_consumer(session_meta.id, [item], fake_run_cc)

  callbacks.persist_claude_account.assert_not_awaited()
  assert asyncio.iscoroutinefunction(SessionManager.persist_claude_account)


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


def test_move_transcript_refuses_when_mtimes_match_and_destination_is_larger(tmp_path: Path) -> None:
  src_dir, dst_dir = tmp_path / "claude-main", tmp_path / "claude-ext-1"
  seed_transcript_copy(src_dir, "uuid-1", "short\n", mtime_ns=5_000)
  seed_transcript_copy(dst_dir, "uuid-1", "short\nand the destination grew under the same stamp\n", mtime_ns=5_000)

  with pytest.raises(claude_accounts.TranscriptMoveError) as excinfo:
    claude_accounts.move_transcript("uuid-1", src_dir, dst_dir)

  assert claude_accounts.GUARD_REFUSAL_MARKER in str(excinfo.value)


def test_move_transcript_passes_an_identical_copy_back_through(tmp_path: Path) -> None:
  """Equal mtime and size is the same copy re-moved: the move proceeds, not a refusal."""
  src_dir, dst_dir = tmp_path / "claude-main", tmp_path / "claude-ext-1"
  body = '{"same": true}\n'
  src = seed_transcript_copy(src_dir, "uuid-1", body, mtime_ns=5_000)
  dst = seed_transcript_copy(dst_dir, "uuid-1", body, mtime_ns=5_000)

  moved = claude_accounts.move_transcript("uuid-1", src_dir, dst_dir)

  assert moved == dst
  assert dst.read_text(encoding="utf-8") == body
  assert src.exists(), "the source copy stays for fallback"


def test_move_transcript_leaves_no_staging_file_behind(tmp_path: Path) -> None:
  src_dir, dst_dir = tmp_path / "claude-main", tmp_path / "claude-ext-1"
  transcript = make_transcript(src_dir, "uuid-1")
  sidecar = transcript.with_suffix("") / "tool-results"
  sidecar.mkdir(parents=True)
  (sidecar / "r.txt").write_text("result", encoding="utf-8")

  claude_accounts.move_transcript("uuid-1", src_dir, dst_dir)

  slug_dir = dst_dir / "projects" / transcript.parent.name
  assert sorted(path.name for path in slug_dir.iterdir()) == [
      "uuid-1", "uuid-1.jsonl"
  ], ("the staged copies are gone: only the final jsonl and its sidecar remain")


def test_move_transcript_sidecar_mismatch_error_carries_the_file_set_diff(tmp_path: Path) -> None:
  """A sidecar stage that cannot land reports which files are missing, extra, or differ."""
  src_dir, dst_dir = tmp_path / "claude-main", tmp_path / "claude-ext-1"
  transcript = make_transcript(src_dir, "uuid-1")
  sidecar = transcript.with_suffix("")
  sidecar.mkdir()
  (sidecar / "a.txt").write_text("alpha", encoding="utf-8")
  (sidecar / "sub").mkdir()
  (sidecar / "sub" / "b.txt").write_text("beta", encoding="utf-8")
  # The destination slug already holds a directory where a.txt must land: the
  # staged copy can never be replaced into place.
  dst_sidecar = dst_dir / "projects" / transcript.parent.name / "uuid-1"
  (dst_sidecar / "a.txt").mkdir(parents=True)
  (dst_sidecar / "a.txt" / "keep.txt").write_text("born here", encoding="utf-8")

  with pytest.raises(claude_accounts.TranscriptMoveError) as excinfo:
    claude_accounts.move_transcript("uuid-1", src_dir, dst_dir)

  message = str(excinfo.value)
  assert "file-set mismatch" in message
  assert "missing at dst" in message and "a.txt" in message and "sub/b.txt" in message
  assert "only at dst" in message and "a.txt/keep.txt" in message
  # No staged half-products remain at the destination.
  assert [path.name for path in dst_sidecar.rglob("*") if path.is_file()] == ["keep.txt"]


def test_move_transcript_retains_and_counts_destination_born_sidecar_files(tmp_path: Path) -> None:
  src_dir, dst_dir = tmp_path / "claude-main", tmp_path / "claude-ext-1"
  transcript = make_transcript(src_dir, "uuid-1")
  src_sidecar = transcript.with_suffix("")
  src_sidecar.mkdir()
  (src_sidecar / "src.txt").write_text("from source", encoding="utf-8")
  dst_sidecar = dst_dir / "projects" / transcript.parent.name / "uuid-1"
  (dst_sidecar / "born-here").mkdir(parents=True)
  (dst_sidecar / "born-here" / "dst.txt").write_text("from destination", encoding="utf-8")

  with capture_logs() as logs:
    claude_accounts.move_transcript("uuid-1", src_dir, dst_dir)

  assert (dst_sidecar / "born-here" / "dst.txt").read_text(encoding="utf-8") == "from destination"
  moved = next(entry for entry in logs if entry["event"] == "claude_account_transcript_moved")
  assert moved["sidecar_src_files"] == 1
  assert moved["sidecar_dst_only_files"] == 1
  assert moved["sidecar_bytes"] == len("from source")


# ---------------------------------------------------------------------------
# Copy retirement after a sound round
# ---------------------------------------------------------------------------


def test_retire_transcript_copies_keeps_the_newest_two(tmp_path: Path) -> None:
  cfg = _pool_cfg(tmp_path)
  oldest = seed_transcript_copy(tmp_path / "claude-ext-2", "uuid-1", "oldest\n", mtime_ns=1_000)
  middle = seed_transcript_copy(tmp_path / "claude-ext-1", "uuid-1", "middle\n", mtime_ns=2_000)
  newest = seed_transcript_copy(tmp_path / "claude-main", "uuid-1", "newest\n", mtime_ns=3_000)
  for copy in (oldest, middle, newest):
    (copy.with_suffix("") / "tool-results").mkdir(parents=True)
    (copy.with_suffix("") / "tool-results" / "r.txt").write_text("x", encoding="utf-8")

  claude_accounts.retire_transcript_copies(cfg, "uuid-1")

  assert not oldest.exists() and not oldest.with_suffix("").exists()
  assert middle.exists() and newest.exists()
  assert (newest.with_suffix("") / "tool-results" / "r.txt").exists()
  assert claude_accounts.transcript_matches(tmp_path / "claude-ext-2", "uuid-1") == []


def test_retire_transcript_copies_keeps_a_custom_count(tmp_path: Path) -> None:
  cfg = _pool_cfg(tmp_path)
  seed_transcript_copy(tmp_path / "claude-ext-2", "uuid-1", "oldest\n", mtime_ns=1_000)
  middle = seed_transcript_copy(tmp_path / "claude-ext-1", "uuid-1", "middle\n", mtime_ns=2_000)
  newest = seed_transcript_copy(tmp_path / "claude-main", "uuid-1", "newest\n", mtime_ns=3_000)

  claude_accounts.retire_transcript_copies(cfg, "uuid-1", keep=1)

  assert not middle.exists()
  assert newest.exists()


def test_retire_transcript_copies_is_a_noop_below_the_keep_count(tmp_path: Path) -> None:
  cfg = _pool_cfg(tmp_path)
  only = seed_transcript_copy(tmp_path / "claude-main", "uuid-1", "only\n", mtime_ns=1_000)

  claude_accounts.retire_transcript_copies(cfg, "uuid-1")

  assert only.exists()


# ---------------------------------------------------------------------------
# The placement probe's lineage helpers
# ---------------------------------------------------------------------------


def test_newest_transcript_copy_picks_the_mtime_newest_pool_holder(tmp_path: Path) -> None:
  cfg = _pool_cfg(tmp_path)
  seed_transcript_copy(tmp_path / "claude-ext-2", "uuid-1", "old\n", mtime_ns=1_000)
  newest = seed_transcript_copy(tmp_path / "claude-ext-1", "uuid-1", "new\n", mtime_ns=2_000)

  holder = claude_accounts.newest_transcript_copy(cfg, "uuid-1")

  assert holder is not None
  assert holder[0].label == "ext-1"
  assert holder[1] == newest
  assert claude_accounts.newest_transcript_copy(cfg, "uuid-9") is None


def test_transcript_lineage_split_tells_succession_from_fork(tmp_path: Path) -> None:
  label_copy = tmp_path / "label.jsonl"
  # A successor: the newest copy grew past the label's tail, out of the probe window.
  seed_line = '{"type": "user", "content": "seed"}\n'
  grown = seed_line + '{"type": "assistant", "content": "' + "x" * (claude_accounts.PROBE_TAIL_BYTES * 2) + '"}\n'
  newest_grown = tmp_path / "newest-grown.jsonl"
  newest_grown.write_text(grown, encoding="utf-8")
  label_copy.write_text(seed_line, encoding="utf-8")

  assert claude_accounts.transcript_lineage_split(
      label_copy, newest_grown) is True, ("the label's tail line fell out of the newest copy's tail window: split")

  # Same lineage: the newest copy still ends where the label ends.
  newest_equal = tmp_path / "newest-equal.jsonl"
  newest_equal.write_text(seed_line, encoding="utf-8")
  assert claude_accounts.transcript_lineage_split(label_copy, newest_equal) is False

  # A blank label window has no tail line to compare and never reads as split.
  blank = tmp_path / "blank.jsonl"
  blank.write_text("", encoding="utf-8")
  assert claude_accounts.transcript_lineage_split(blank, newest_grown) is False
