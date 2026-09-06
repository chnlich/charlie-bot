"""Claude account pool: membership, health, headroom, selection, transcript moves, and the pool's
reach into resume resolution, the backend-switch domain, the usage panel, and metadata persistence."""

import asyncio
import json
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest
from conftest import (
    make_work_item,
    mock_session_callbacks,
    run_session_consumer,
)

from src.agents import master_cc_run
from src.api import ext_usage as ext_usage_mod
from src.api.sessions import _active_backend_payload, _backend_domain
from src.core import claude_accounts, storage_cool, token_tally
from src.core import config as core_config
from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.models import (
    BackendOption,
    ClaudeAccount,
    CreateSessionRequest,
    SessionMetadata,
)
from src.core.sessions import SessionManager

NOW = datetime(2026, 9, 6, 20, 0, tzinfo=UTC)
FABLE = "claude-fable-5-1"
SONNET = "claude-sonnet-5"


@pytest.fixture(autouse=True)
def _fresh_pool_state():
  claude_accounts.reset_for_tests()
  yield
  claude_accounts.reset_for_tests()


def _write_credentials(config_dir: Path, access_token: str = "token") -> None:
  config_dir.mkdir(parents=True, exist_ok=True)
  (config_dir / claude_accounts.CREDENTIALS_FILE).write_text(
      json.dumps({"claudeAiOauth": {
          "accessToken": access_token,
          "refreshToken": "refresh"
      }}), encoding="utf-8")


def _options(pinned_dir: Path) -> list[BackendOption]:
  return [
      BackendOption(id="claude-fable-5", label="Fable", type="cc-claude", model=FABLE, aliases=["claude-fable-5-ext1"]),
      BackendOption(id="claude-sonnet-5", label="Sonnet", type="cc-claude", model=SONNET),
      BackendOption(
          id="pinned-opus", label="Pinned", type="cc-claude", model="claude-opus-5", claude_config_dir=str(pinned_dir)),
      BackendOption(id="codex-o3", label="Codex", type="codex", model="o3"),
  ]


def _pool_cfg(tmp_path: Path, labels: tuple[str, ...] = ("main", "ext-1", "ext-2")) -> CharlieBotConfig:
  accounts = [ClaudeAccount(label=label, config_dir=str(tmp_path / f"claude-{label}")) for label in labels]
  for account in accounts:
    _write_credentials(Path(account.config_dir))
  return CharlieBotConfig(
      charliebot_home=tmp_path / "home", claude_accounts=accounts, backend_options=_options(tmp_path / "pinned"))


def _legacy_cfg(tmp_path: Path) -> CharlieBotConfig:
  return CharlieBotConfig(charliebot_home=tmp_path / "home", backend_options=_options(tmp_path / "pinned"))


def _write_transcript(config_dir: Path, cc_session_id: str, slug: str = "-home-u--charliebot-sessions-s1") -> Path:
  transcript = config_dir / "projects" / slug / f"{cc_session_id}.jsonl"
  transcript.parent.mkdir(parents=True, exist_ok=True)
  transcript.write_text('{"type":"user"}\n', encoding="utf-8")
  return transcript


# ---------------------------------------------------------------------------
# Membership and aliases
# ---------------------------------------------------------------------------


def test_is_pooled_only_for_dirless_cc_claude_entries_when_pool_declared(tmp_path: Path) -> None:
  pooled_cfg = _pool_cfg(tmp_path)
  legacy_cfg = _legacy_cfg(tmp_path)
  fable = pooled_cfg.get_backend_option("claude-fable-5")
  pinned = pooled_cfg.get_backend_option("pinned-opus")
  codex = pooled_cfg.get_backend_option("codex-o3")

  assert claude_accounts.is_pooled(fable, pooled_cfg) is True
  assert claude_accounts.is_pooled(pinned, pooled_cfg) is False
  assert claude_accounts.is_pooled(codex, pooled_cfg) is False
  # No claude_accounts key: the same entry keeps today's per-directory semantics.
  assert claude_accounts.is_pooled(fable, legacy_cfg) is False


def test_pool_expands_config_dirs(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setenv("HOME", str(tmp_path))
  cfg = CharlieBotConfig(
      charliebot_home=tmp_path / "home", claude_accounts=[ClaudeAccount(label="main", config_dir="~/.claude")])

  assert claude_accounts.pool(cfg)[0].config_dir == str(tmp_path / ".claude")
  assert claude_accounts.account_for_dir(cfg, "~/.claude").label == "main"
  assert claude_accounts.account_by_label(cfg, "missing") is None


def test_get_backend_option_resolves_aliases_after_exact_ids(tmp_path: Path) -> None:
  cfg = _pool_cfg(tmp_path)

  assert cfg.get_backend_option("claude-fable-5-ext1").id == "claude-fable-5"
  assert cfg.get_backend_option("claude-fable-5").id == "claude-fable-5"
  assert cfg.get_backend_option("claude-fable-sub") is None


def test_load_config_reads_pool_keys(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  home = tmp_path / "charliebot"
  home.mkdir()
  (home / "config.yaml").write_text(
      """
claude_accounts:
  - label: main
    config_dir: ~/.claude
  - label: ext-1
    config_dir: ~/.claude-ext-1
claude_compaction:
  relay_tokens: 120000
  expired_cache_tokens: 60000
backend_options:
  - id: claude-fable-5
    label: Fable
    type: cc-claude
    model: claude-fable-5-1
    aliases: [claude-fable-5-ext1]
""",
      encoding="utf-8")
  monkeypatch.setenv(core_config.CHARLIEBOT_HOME_ENV, str(home))

  cfg = core_config.load_config()

  assert [account.label for account in cfg.claude_accounts] == ["main", "ext-1"]
  assert cfg.claude_compaction.relay_tokens == 120000
  assert cfg.claude_compaction.expired_cache_tokens == 60000
  assert cfg.backend_options[0].aliases == ["claude-fable-5-ext1"]


def test_config_defaults_carry_no_pool_and_default_floors(tmp_path: Path) -> None:
  cfg = _legacy_cfg(tmp_path)

  assert cfg.claude_accounts == []
  assert (cfg.claude_compaction.relay_tokens, cfg.claude_compaction.expired_cache_tokens) == (100_000, 50_000)


# ---------------------------------------------------------------------------
# Health
# ---------------------------------------------------------------------------


def test_credentials_present_requires_a_non_empty_access_token(tmp_path: Path) -> None:
  account = ClaudeAccount(label="a", config_dir=str(tmp_path / "a"))
  assert claude_accounts.credentials_present(account) is False  # no file

  _write_credentials(tmp_path / "a", access_token="")
  assert claude_accounts.credentials_present(account) is False  # emptied by a failed refresh

  (tmp_path / "a" / claude_accounts.CREDENTIALS_FILE).write_text("not json", encoding="utf-8")
  assert claude_accounts.credentials_present(account) is False

  _write_credentials(tmp_path / "a")
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
  assert claude_accounts.headroom("ext-1", FABLE, now=NOW) == pytest.approx(0.08)
  assert claude_accounts.observe_rate_limit("ext-1", {"status": "allowed"}, now=NOW) is None


def test_rejected_reading_zeroes_headroom_until_its_reset() -> None:
  resets_at = (NOW + timedelta(minutes=30)).timestamp()
  claude_accounts.observe_rate_limit("ext-1", _event("rejected", 0.99, 0.40, resets_at), now=NOW)

  assert claude_accounts.headroom("ext-1", FABLE, now=NOW) == 0.0
  assert claude_accounts.headroom("ext-1", FABLE, now=NOW + timedelta(minutes=31)) == pytest.approx(0.01)


def test_unread_account_has_full_headroom() -> None:
  assert claude_accounts.headroom("never-read", FABLE, now=NOW) == 1.0


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
  assert claude_accounts.headroom("ext-1", FABLE, now=NOW + timedelta(minutes=6)) == pytest.approx(0.30)

  claude_accounts.observe_rate_limit("ext-1", _event("allowed", 0.50, 0.10), now=NOW + timedelta(minutes=10))
  assert claude_accounts.headroom("ext-1", FABLE, now=NOW + timedelta(minutes=11)) == pytest.approx(0.50)


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

  assert claude_accounts.headroom("ext-1", FABLE, now=NOW) == pytest.approx(0.40)
  assert claude_accounts.headroom("ext-1", SONNET, now=NOW) == pytest.approx(0.90)


# ---------------------------------------------------------------------------
# Selection
# ---------------------------------------------------------------------------


def test_select_prefers_most_headroom_and_keeps_current_on_a_tie(tmp_path: Path) -> None:
  cfg = _pool_cfg(tmp_path)
  claude_accounts.observe_rate_limit("main", _event("allowed", 0.50, 0.10), now=NOW)
  claude_accounts.observe_rate_limit("ext-1", _event("allowed", 0.10, 0.05), now=NOW)
  claude_accounts.observe_rate_limit("ext-2", _event("allowed", 0.10, 0.05), now=NOW)

  assert claude_accounts.select(cfg, FABLE, current="ext-2", now=NOW).label == "ext-2"
  assert claude_accounts.select(cfg, FABLE, current="ext-1", now=NOW).label == "ext-1"
  assert claude_accounts.select(cfg, FABLE, current="main", now=NOW).label in {"ext-1", "ext-2"}


def test_select_skips_excluded_rejected_and_unhealthy_accounts(tmp_path: Path) -> None:
  cfg = _pool_cfg(tmp_path)
  claude_accounts.observe_rate_limit("main", _event("allowed", 0.30, 0.10), now=NOW)
  claude_accounts.observe_rate_limit(
      "ext-1", _event("rejected", 1.0, 0.10, (NOW + timedelta(hours=1)).timestamp()), now=NOW)
  _write_credentials(tmp_path / "claude-ext-2", access_token="")  # emptied credential store

  assert claude_accounts.select(cfg, FABLE, current="main", exclude={"main"}, now=NOW) is None
  assert claude_accounts.select(cfg, FABLE, current="ext-1", now=NOW).label == "main"
  assert claude_accounts.earliest_reset(cfg, now=NOW) == NOW + timedelta(hours=1)


# ---------------------------------------------------------------------------
# Transcripts
# ---------------------------------------------------------------------------


def test_move_transcript_copies_conversation_and_sidecar_into_the_same_slug(tmp_path: Path) -> None:
  src_dir = tmp_path / "claude-ext-2"
  dst_dir = tmp_path / "claude-main"
  transcript = _write_transcript(src_dir, "uuid-1")
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
  _write_transcript(tmp_path / "claude-ext-2", "uuid-2")

  assert claude_accounts.find_transcript_account(cfg, "uuid-2").label == "ext-2"
  assert claude_accounts.find_transcript_account(cfg, "uuid-9") is None


# ---------------------------------------------------------------------------
# Resume resolution through the pool
# ---------------------------------------------------------------------------


def test_resolve_resume_id_prefers_own_account_then_searches_pool_and_writes_back(tmp_path: Path) -> None:
  cfg = _pool_cfg(tmp_path)
  fable = cfg.get_backend_option("claude-fable-5")
  _write_transcript(tmp_path / "claude-ext-2", "uuid-3")

  meta = SessionMetadata(id="s1", name="t", backend="claude-fable-5", cc_session_id="uuid-3")
  assert master_cc_run._resolve_resume_id(fable, meta, cfg=cfg) == "uuid-3"
  assert meta.claude_account == "ext-2"

  # A transcript present under the recorded account is taken from there, even when
  # another login also holds a copy (the source copy left behind by a relay).
  _write_transcript(tmp_path / "claude-main", "uuid-3")
  meta_main = SessionMetadata(
      id="s1", name="t", backend="claude-fable-5", cc_session_id="uuid-3", claude_account="main")
  assert master_cc_run._resolve_resume_id(fable, meta_main, cfg=cfg) == "uuid-3"
  assert meta_main.claude_account == "main"


def test_resolve_resume_id_pool_miss_returns_none_and_keeps_account(tmp_path: Path) -> None:
  cfg = _pool_cfg(tmp_path)
  fable = cfg.get_backend_option("claude-fable-5")
  meta = SessionMetadata(id="s1", name="t", backend="claude-fable-5", cc_session_id="uuid-4", claude_account="ext-1")

  assert master_cc_run._resolve_resume_id(fable, meta, cfg=cfg) is None
  assert meta.claude_account == "ext-1"


def test_resolve_resume_id_without_pool_keeps_the_pinned_directory_rule(tmp_path: Path) -> None:
  cfg = _legacy_cfg(tmp_path)
  pinned = cfg.get_backend_option("pinned-opus")
  _write_transcript(tmp_path / "pinned", "uuid-5")
  _write_transcript(tmp_path / "elsewhere", "uuid-6")

  found = SessionMetadata(id="s1", name="t", backend="pinned-opus", cc_session_id="uuid-5")
  missing = SessionMetadata(id="s1", name="t", backend="pinned-opus", cc_session_id="uuid-6")
  assert master_cc_run._resolve_resume_id(pinned, found, cfg=cfg) == "uuid-5"
  assert master_cc_run._resolve_resume_id(pinned, missing, cfg=cfg) is None
  assert found.claude_account is None


# ---------------------------------------------------------------------------
# Backend-switch domain
# ---------------------------------------------------------------------------


def test_pooled_entries_share_one_switch_domain_and_pinned_entries_keep_theirs(tmp_path: Path) -> None:
  cfg = _pool_cfg(tmp_path)

  assert _backend_domain(cfg.get_backend_option("claude-fable-5"), cfg) == claude_accounts.POOL_DOMAIN
  assert _backend_domain(cfg.get_backend_option("claude-sonnet-5"), cfg) == claude_accounts.POOL_DOMAIN
  assert _backend_domain(cfg.get_backend_option("pinned-opus"), cfg) == str(tmp_path / "pinned")
  assert _backend_domain(cfg.get_backend_option("codex-o3"), cfg) is None

  pooled = _active_backend_payload(SessionMetadata(id="a", name="t", backend="claude-fable-5"), cfg)
  assert pooled["switchable_backends"] == ["claude-fable-5", "claude-sonnet-5"]
  pinned = _active_backend_payload(SessionMetadata(id="b", name="t", backend="pinned-opus"), cfg)
  assert pinned["switchable_backends"] == ["pinned-opus"]


# ---------------------------------------------------------------------------
# Directory derivations: usage panel, token tally, cold storage
# ---------------------------------------------------------------------------


def test_usage_panel_accounts_come_from_the_pool_under_their_own_labels(tmp_path: Path, monkeypatch) -> None:
  cfg = _pool_cfg(tmp_path)
  monkeypatch.setattr(ext_usage_mod, "get_config", lambda: cfg)

  accounts = ext_usage_mod._derive_accounts()["claude"]

  assert accounts[0] == ("main", ext_usage_mod.CLAUDE_DEFAULT_DIR)
  assert [label for label, _ in accounts] == ["main", "ext-1", "ext-2", "pinned"]
  assert dict(accounts)["ext-1"] == str(tmp_path / "claude-ext-1")


def test_token_tally_and_cold_storage_include_pool_directories(tmp_path: Path, monkeypatch) -> None:
  cfg = _pool_cfg(tmp_path)
  for label in ("main", "ext-1", "ext-2"):
    (tmp_path / f"claude-{label}" / "projects").mkdir(parents=True, exist_ok=True)
  monkeypatch.setattr(token_tally, "get_config", lambda: cfg)

  claude_map, _codex_map = token_tally.discover_homes(tmp_path / ".claude", tmp_path / ".codex")
  assert set(claude_map.values()) >= {tmp_path / "claude-main", tmp_path / "claude-ext-1", tmp_path / "claude-ext-2"}

  roots = storage_cool.claude_projects_roots(cfg)
  assert tmp_path / "claude-ext-2" / "projects" in roots
  assert tmp_path / "pinned" / "projects" in roots


# ---------------------------------------------------------------------------
# Metadata persistence
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_persist_claude_account_round_trips_without_touching_other_fields(tmp_path: Path) -> None:
  cfg = _legacy_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = await session_mgr.create_session(CreateSessionRequest(name="t"), backend="claude-fable-5")
  await session_mgr.persist_cc_session_id(meta.id, "uuid-7")

  assert await session_mgr.persist_claude_account(meta.id, "ext-1") == "ext-1"
  fresh = await session_mgr.read_metadata_fresh(meta.id)
  assert fresh.claude_account == "ext-1"
  assert fresh.cc_session_id == "uuid-7"
  assert await session_mgr.persist_claude_account("missing-session", "ext-1") is None


@pytest.mark.asyncio
async def test_consumer_persists_the_account_the_run_settled_on(tmp_path: Path) -> None:
  cfg = _legacy_cfg(tmp_path)
  session_meta = SessionMetadata(id="consumer-account", name="t", backend="claude-fable-5")
  callbacks = mock_session_callbacks()
  callbacks.persist_claude_account.side_effect = lambda sid, label: "other"
  item = make_work_item(cfg, session_meta, cfg.backend_options[0], callbacks=callbacks)

  async def fake_run_cc(work_item):
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
  cfg = _legacy_cfg(tmp_path)
  session_meta = SessionMetadata(id="consumer-none", name="t", backend="claude-fable-5")
  callbacks = mock_session_callbacks()
  item = make_work_item(cfg, session_meta, cfg.backend_options[0], callbacks=callbacks)

  async def fake_run_cc(work_item):
    return "uuid-9", 0, None, {}

  await run_session_consumer(session_meta.id, [item], fake_run_cc)

  callbacks.persist_claude_account.assert_not_awaited()
  assert asyncio.iscoroutinefunction(SessionManager.persist_claude_account)
