"""Tests for the improvement-loop lifecycle module."""

from datetime import datetime, timezone, timedelta
from pathlib import Path
from unittest.mock import AsyncMock, patch

import pytest
import yaml

from src.core.config import ImprovementLoopConfig
from src.core.improvement_loop import determine_action, _next_id


def _make_cfg(**overrides) -> ImprovementLoopConfig:
  defaults = dict(
      backlog='loop/backlog.yaml',
      role='test agent',
      scope_files=['src/'],
      id_prefix='',
      language='en',
      max_pending=10,
      stale_timeout_hours=1.0,
  )
  defaults.update(overrides)
  return ImprovementLoopConfig(**defaults)


def _write_backlog(path: Path, items: list[dict]) -> None:
  path.parent.mkdir(parents=True, exist_ok=True)
  path.write_text(yaml.dump(items, default_flow_style=False, allow_unicode=True, sort_keys=False))


# ---------------------------------------------------------------------------
# test_revision_requested_picked_first
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_revision_requested_picked_first(tmp_path: Path) -> None:
  """Revision feedback takes priority over approved items."""
  backlog = tmp_path / 'backlog.yaml'
  items = [
      {'id': '001', 'status': 'approved', 'title': 'Fix bug', 'priority': 'high'},
      {
          'id': '002',
          'status': 'revision_requested',
          'title': 'Refactor X',
          'revision_feedback': 'Make it simpler',
      },
  ]
  _write_backlog(backlog, items)
  cfg = _make_cfg()

  action, prompt = await determine_action(backlog, cfg, tmp_path)

  assert action == 'revision'
  assert '002' in prompt
  assert 'Make it simpler' in prompt


# ---------------------------------------------------------------------------
# test_stale_in_progress_reset
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_stale_in_progress_reset(tmp_path: Path) -> None:
  """Stale in_progress items get reset to failed, YAML updated."""
  backlog = tmp_path / 'backlog.yaml'
  old_time = (datetime.now(timezone.utc) - timedelta(hours=2)).isoformat()
  items = [
      {'id': '001', 'status': 'in_progress', 'title': 'Slow task', 'created': old_time},
  ]
  _write_backlog(backlog, items)
  cfg = _make_cfg()

  with patch('src.core.improvement_loop.asyncio.to_thread', new_callable=AsyncMock) as mock_thread:
    action, prompt = await determine_action(backlog, cfg, tmp_path)

  assert action == 'stale_reset'
  assert prompt is None
  mock_thread.assert_called_once()

  # Verify YAML was updated
  updated = yaml.safe_load(backlog.read_text())
  assert updated[0]['status'] == 'failed'
  assert 'Timed out' in updated[0]['failed_reason']


# ---------------------------------------------------------------------------
# test_implement_highest_priority
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_implement_highest_priority(tmp_path: Path) -> None:
  """Multiple approved items — picks highest priority."""
  backlog = tmp_path / 'backlog.yaml'
  items = [
      {'id': '001', 'status': 'approved', 'title': 'Low prio', 'priority': 'low', 'description': 'desc1'},
      {'id': '002', 'status': 'approved', 'title': 'High prio', 'priority': 'high', 'description': 'desc2'},
      {'id': '003', 'status': 'approved', 'title': 'Med prio', 'priority': 'medium', 'description': 'desc3'},
  ]
  _write_backlog(backlog, items)
  cfg = _make_cfg()

  action, prompt = await determine_action(backlog, cfg, tmp_path)

  assert action == 'implement'
  assert '002' in prompt
  assert 'High prio' in prompt


# ---------------------------------------------------------------------------
# test_generate_when_no_active
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_generate_when_no_active(tmp_path: Path) -> None:
  """No approved/in_progress items and under cap → generate."""
  backlog = tmp_path / 'backlog.yaml'
  items = [
      {'id': '001', 'status': 'done', 'title': 'Done task'},
      {'id': '002', 'status': 'pending', 'title': 'Pending task'},
  ]
  _write_backlog(backlog, items)
  cfg = _make_cfg(max_pending=10)

  action, prompt = await determine_action(backlog, cfg, tmp_path)

  assert action == 'generate'
  assert '003' in prompt  # next sequential ID


# ---------------------------------------------------------------------------
# test_noop_when_at_cap
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_noop_when_at_cap(tmp_path: Path) -> None:
  """At max_pending, no active items → skip generate, go to scan."""
  backlog = tmp_path / 'backlog.yaml'
  items = [{'id': f'{i:03d}', 'status': 'pending', 'title': f'Item {i}'} for i in range(1, 11)]
  _write_backlog(backlog, items)
  cfg = _make_cfg(max_pending=10)

  action, prompt = await determine_action(backlog, cfg, tmp_path)

  # At cap: skip generate, fall through to scan
  assert action == 'scan'
  assert prompt is not None


# ---------------------------------------------------------------------------
# test_scan_fallback
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_scan_fallback(tmp_path: Path) -> None:
  """Empty backlog → scan fallback."""
  backlog = tmp_path / 'backlog.yaml'
  _write_backlog(backlog, [])
  cfg = _make_cfg(max_pending=0)  # at cap, forces skip of generate

  action, prompt = await determine_action(backlog, cfg, tmp_path)

  assert action == 'scan'
  assert 'test agent' in prompt


# ---------------------------------------------------------------------------
# test_priority_ordering
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_priority_ordering(tmp_path: Path) -> None:
  """Priority: high > medium > low."""
  backlog = tmp_path / 'backlog.yaml'
  items = [
      {'id': '001', 'status': 'approved', 'title': 'Med', 'priority': 'medium', 'description': 'x'},
      {'id': '002', 'status': 'approved', 'title': 'Low', 'priority': 'low', 'description': 'y'},
      {'id': '003', 'status': 'approved', 'title': 'High', 'priority': 'high', 'description': 'z'},
  ]
  _write_backlog(backlog, items)
  cfg = _make_cfg()

  action, prompt = await determine_action(backlog, cfg, tmp_path)

  assert action == 'implement'
  assert '003' in prompt
  assert 'High' in prompt


# ---------------------------------------------------------------------------
# test_next_id_with_prefix
# ---------------------------------------------------------------------------

def test_next_id_with_prefix() -> None:
  items = [{'id': 'D-001'}, {'id': 'D-005'}, {'id': '007'}]
  assert _next_id(items, 'D') == 'D-006'


def test_next_id_without_prefix() -> None:
  items = [{'id': '003'}, {'id': '010'}, {'id': 'D-001'}]
  assert _next_id(items, '') == '011'


def test_next_id_empty_backlog() -> None:
  assert _next_id([], 'F') == 'F-001'
  assert _next_id([], '') == '001'


# ---------------------------------------------------------------------------
# test_missing_backlog_file
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_missing_backlog_generates(tmp_path: Path) -> None:
  """Missing backlog file → generate (empty backlog, under cap)."""
  backlog = tmp_path / 'nonexistent' / 'backlog.yaml'
  cfg = _make_cfg(max_pending=10)

  action, prompt = await determine_action(backlog, cfg, tmp_path)

  assert action == 'generate'


# ---------------------------------------------------------------------------
# test_language_rule_in_prompt
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_language_rule_zh_cn(tmp_path: Path) -> None:
  """zh-CN language rule appears in generate prompt."""
  backlog = tmp_path / 'backlog.yaml'
  _write_backlog(backlog, [])
  cfg = _make_cfg(language='zh-CN', max_pending=10)

  action, prompt = await determine_action(backlog, cfg, tmp_path)

  assert action == 'generate'
  assert 'simplified Chinese' in prompt


@pytest.mark.asyncio
async def test_state_files_in_implement_prompt(tmp_path: Path) -> None:
  """State files instructions appear in implement prompt."""
  backlog = tmp_path / 'backlog.yaml'
  items = [{'id': '001', 'status': 'approved', 'title': 'Fix', 'priority': 'high', 'description': 'desc'}]
  _write_backlog(backlog, items)
  cfg = _make_cfg(state_files=['loop/history.yaml', 'loop/e2e_report.json'])

  action, prompt = await determine_action(backlog, cfg, tmp_path)

  assert action == 'implement'
  assert 'Read loop/history.yaml before acting' in prompt
  assert 'Read loop/e2e_report.json (read-only, do not modify)' in prompt
