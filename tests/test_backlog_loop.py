"""Tests for the improvement-loop lifecycle module."""

import datetime
import pathlib
from unittest import mock

import conftest
import pytest
import yaml

from src.features.backlog import backlog_loop, config


def _make_cfg(**overrides: object) -> config.ImprovementLoopConfig:
  defaults = {
      'backlog': 'backlog/backlog.yaml',
      'role': 'test agent',
      'scope_files': ['src/'],
      'id_prefix': '',
      'language': 'en',
      'max_pending': 10,
      'stale_timeout_hours': 1.0,
  }
  defaults.update(overrides)
  return config.ImprovementLoopConfig(**defaults)


def _write_backlog(path: pathlib.Path, items: list[dict]) -> None:
  path.parent.mkdir(parents=True, exist_ok=True)
  path.write_text(yaml.dump(items, default_flow_style=False, allow_unicode=True, sort_keys=False))


def _assert_concise_description_constraint(prompt: str) -> None:
  assert 'PURPOSE is exactly one short sentence' in prompt
  assert 'HOW is at most a few short implementation bullets or sentences' in prompt
  assert 'long evidence dumps' in prompt
  assert 'grep transcripts' in prompt
  assert 'full correctness proofs' in prompt
  assert 'exhaustive line-by-line implementation plans' in prompt
  assert 'benchmark speculation' in prompt


# ---------------------------------------------------------------------------
# test_revision_requested_picked_first
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_revision_requested_picked_first(tmp_path: pathlib.Path) -> None:
  """Revision feedback takes priority over approved items."""
  backlog = tmp_path / 'backlog.yaml'
  items = [
      {
          'id': '001',
          'status': 'approved',
          'title': 'Fix bug',
          'priority': 'high'
      },
      {
          'id': '002',
          'status': 'revision_requested',
          'title': 'Refactor X',
          'revision_feedback': 'Make it simpler',
      },
  ]
  _write_backlog(backlog, items)
  cfg = _make_cfg()

  action, prompt = await backlog_loop.determine_action(backlog, cfg, tmp_path)

  assert action == 'revision'
  assert '002' in prompt
  assert 'Make it simpler' in prompt


# ---------------------------------------------------------------------------
# test_stale_in_progress_reset
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_stale_in_progress_reset(tmp_path: pathlib.Path) -> None:
  """Stale in_progress items get reset to failed, YAML updated."""
  backlog = tmp_path / 'backlog.yaml'
  old_time = (datetime.datetime.now(datetime.UTC) - datetime.timedelta(hours=2)).isoformat()
  items = [
      {
          'id': '001',
          'status': 'in_progress',
          'title': 'Slow task',
          'created': old_time
      },
  ]
  _write_backlog(backlog, items)
  cfg = _make_cfg()

  with mock.patch(conftest.BACKLOG_LOOP_GIT_ADD_COMMIT_PUSH_PATCH_TARGET, new_callable=mock.AsyncMock) as mock_commit:
    action, prompt = await backlog_loop.determine_action(backlog, cfg, tmp_path)

  assert action == 'stale_reset'
  assert prompt is None
  mock_commit.assert_awaited_once()

  # Verify YAML was updated
  updated = yaml.safe_load(backlog.read_text())
  assert updated[0]['status'] == 'failed'
  assert 'Timed out' in updated[0]['failed_reason']


# ---------------------------------------------------------------------------
# test_implement_picks_highest_priority_approved_item
# ---------------------------------------------------------------------------

_IMPLEMENT_PICK_ROWS = [
    pytest.param(
        [("001", "low", "Low prio"), ("002", "high", "High prio"), ("003", "medium", "Med prio")],
        "002",
        "High prio",
        id="high-beats-medium-and-low"),
    pytest.param(
        [("001", "medium", "Med"), ("002", "low", "Low"), ("003", "high", "High")],
        "003",
        "High",
        id="high-wins-from-any-file-position"),
]


@pytest.mark.asyncio
@pytest.mark.parametrize(("items", "expected_id", "expected_title"), _IMPLEMENT_PICK_ROWS)
async def test_implement_picks_highest_priority_approved_item(
    tmp_path: pathlib.Path, items: list[tuple[str, str, str]], expected_id: str, expected_title: str) -> None:
  """Multiple approved items — picks highest priority."""
  backlog = tmp_path / 'backlog.yaml'
  _write_backlog(
      backlog, [
          {
              'id': item_id,
              'status': 'approved',
              'title': title,
              'priority': priority,
              'description': f'desc-{item_id}'
          } for item_id, priority, title in items
      ])
  cfg = _make_cfg()

  action, prompt = await backlog_loop.determine_action(backlog, cfg, tmp_path)

  assert action == 'implement'
  assert expected_id in prompt
  assert expected_title in prompt


# ---------------------------------------------------------------------------
# test_generate_when_no_active
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_generate_when_no_active(tmp_path: pathlib.Path) -> None:
  """No approved/in_progress items and under cap → generate."""
  backlog = tmp_path / 'backlog.yaml'
  items = [
      {
          'id': '001',
          'status': 'done',
          'title': 'Done task'
      },
      {
          'id': '002',
          'status': 'pending',
          'title': 'Pending task'
      },
  ]
  _write_backlog(backlog, items)
  cfg = _make_cfg(max_pending=10)

  action, prompt = await backlog_loop.determine_action(backlog, cfg, tmp_path)

  assert action == 'generate'
  assert '003' in prompt  # next sequential ID
  _assert_concise_description_constraint(prompt)


# ---------------------------------------------------------------------------
# test_noop_when_at_cap
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_noop_when_at_cap(tmp_path: pathlib.Path) -> None:
  """At max_pending, no active items → skip generate, go to scan."""
  backlog = tmp_path / 'backlog.yaml'
  items = [{'id': f'{i:03d}', 'status': 'pending', 'title': f'Item {i}'} for i in range(1, 11)]
  _write_backlog(backlog, items)
  cfg = _make_cfg(max_pending=10)

  action, prompt = await backlog_loop.determine_action(backlog, cfg, tmp_path)

  # At cap: skip generate, fall through to scan
  assert action == 'scan'
  assert prompt is not None


# ---------------------------------------------------------------------------
# test_scan_fallback
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_scan_fallback(tmp_path: pathlib.Path) -> None:
  """Empty backlog → scan fallback."""
  backlog = tmp_path / 'backlog.yaml'
  _write_backlog(backlog, [])
  cfg = _make_cfg(max_pending=0)  # at cap, forces skip of generate

  action, prompt = await backlog_loop.determine_action(backlog, cfg, tmp_path)

  assert action == 'scan'
  assert 'test agent' in prompt
  _assert_concise_description_constraint(prompt)


# ---------------------------------------------------------------------------
# test_missing_backlog_generates
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_missing_backlog_generates(tmp_path: pathlib.Path) -> None:
  """Missing backlog file → generate (empty backlog, under cap)."""
  backlog = tmp_path / 'nonexistent' / 'backlog.yaml'
  cfg = _make_cfg(max_pending=10)

  action, _prompt = await backlog_loop.determine_action(backlog, cfg, tmp_path)

  assert action == 'generate'


@pytest.mark.asyncio
async def test_malformed_backlog_fails_loud(tmp_path: pathlib.Path) -> None:
  """A non-list backlog file errors naming the file instead of silently reading as empty."""
  backlog = tmp_path / 'backlog.yaml'
  backlog.write_text('items:\n- id: 001\n', encoding='utf-8')
  cfg = _make_cfg()

  with pytest.raises(ValueError, match=r'backlog\.yaml: expected a YAML list of backlog items, got dict'):
    await backlog_loop.determine_action(backlog, cfg, tmp_path)
