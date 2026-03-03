"""Improvement-loop lifecycle — determines the next action from a backlog YAML."""

import asyncio
import re
import subprocess
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

import yaml
import structlog

from src.core.config import ImprovementLoopConfig

log = structlog.get_logger()

_PRIORITY_ORDER = {'high': 0, 'medium': 1, 'low': 2}


def _load_backlog(backlog_path: Path) -> list[dict]:
  """Load backlog items from YAML. Returns empty list if file missing."""
  if not backlog_path.exists():
    return []
  data = yaml.safe_load(backlog_path.read_text()) or []
  if isinstance(data, list):
    return data
  return data.get('items', data.get('backlog', []))


def _save_backlog(backlog_path: Path, items: list[dict]) -> None:
  """Write backlog items back to YAML (block style)."""
  backlog_path.parent.mkdir(parents=True, exist_ok=True)
  backlog_path.write_text(yaml.dump(items, default_flow_style=False, allow_unicode=True, sort_keys=False))


def _next_id(items: list[dict], prefix: str) -> str:
  """Compute the next sequential ID from existing items."""
  max_num = 0
  if prefix:
    pattern = re.compile(rf'^{re.escape(prefix)}-(\d+)$')
    for item in items:
      m = pattern.match(str(item.get('id', '')))
      if m:
        max_num = max(max_num, int(m.group(1)))
    return f'{prefix}-{max_num + 1:03d}'
  else:
    for item in items:
      raw = str(item.get('id', ''))
      if raw.isdigit():
        max_num = max(max_num, int(raw))
    return f'{max_num + 1:03d}'


def _language_rule(language: str) -> str:
  if language == 'zh-CN':
    return 'ALL text (title, description) in simplified Chinese. Field names in English.'
  return 'ALL text in English.'


def _state_files_instructions(state_files: list[str]) -> str:
  parts = []
  for f in state_files:
    if f.endswith('e2e_report.json'):
      parts.append(f'Read {f} (read-only, do not modify).')
    else:
      parts.append(f'Read {f} before acting.')
  return ' '.join(parts)


def _extra_rules_text(extra_rules: list[str]) -> str:
  if not extra_rules:
    return ''
  return ' '.join(extra_rules)


def _check_revision(items: list[dict], backlog_path: Path) -> Optional[str]:
  """Step 0: address revision feedback."""
  for item in items:
    if item.get('status') == 'revision_requested' and item.get('revision_feedback'):
      item_id = item['id']
      feedback = item['revision_feedback']
      return (
          f'Backlog item {item_id} has revision feedback: "{feedback}". '
          f'Read the backlog file at {backlog_path}. '
          f'Update item {item_id}: modify title and/or description based on the feedback. '
          f'Set status to pending. Remove revision_feedback and revision_requested_at fields. '
          f'Commit and push.')
  return None


async def _handle_stale(items: list[dict], backlog_path: Path, cfg: ImprovementLoopConfig, repo_path: Path) -> bool:
  """Step 1: reset stale in_progress items. Returns True if any were reset."""
  now = datetime.now(timezone.utc)
  modified = False
  for item in items:
    if item.get('status') != 'in_progress':
      continue
    created = item.get('created')
    if not created:
      continue
    if isinstance(created, str):
      try:
        created_dt = datetime.fromisoformat(created)
      except ValueError:
        continue
    elif isinstance(created, datetime):
      created_dt = created
    else:
      continue
    if created_dt.tzinfo is None:
      created_dt = created_dt.replace(tzinfo=timezone.utc)
    elapsed_hours = (now - created_dt).total_seconds() / 3600
    if elapsed_hours > cfg.stale_timeout_hours:
      item['status'] = 'failed'
      item['failed_reason'] = f'Timed out after {cfg.stale_timeout_hours} hour(s)'
      modified = True

  if not modified:
    return False

  _save_backlog(backlog_path, items)

  def _git_commit_push():
    subprocess.run(['git', 'add', str(backlog_path)], cwd=repo_path, check=True, capture_output=True)
    subprocess.run(
        ['git', 'commit', '-m', f'loop: reset stale in_progress items in {backlog_path.name}'],
        cwd=repo_path,
        check=True,
        capture_output=True,
    )
    subprocess.run(['git', 'push'], cwd=repo_path, check=True, capture_output=True)

  await asyncio.to_thread(_git_commit_push)
  return True


def _build_implement_prompt(item: dict, cfg: ImprovementLoopConfig, backlog_path: Path) -> str:
  """Step 2: build prompt for implementing an approved item."""
  parts = [
      f'You are the {cfg.role}.',
      f'Implement backlog item {item["id"]}: "{item["title"]}".',
      f'Description: {item.get("description", "")}.',
      f'Files in scope: {", ".join(cfg.scope_files)}.',
  ]
  if cfg.state_files:
    parts.append(_state_files_instructions(cfg.state_files))
  if cfg.verify:
    parts.append(f'After implementation, run: {"; ".join(cfg.verify)}.')
  parts.append(
      f'Then update {backlog_path}: set item {item["id"]} status to done. '
      f'If implementation fails, set status to failed with failed_reason. Commit and push.')
  extra = _extra_rules_text(cfg.extra_rules)
  if extra:
    parts.append(extra)
  return ' '.join(parts)


def _build_generate_prompt(items: list[dict], cfg: ImprovementLoopConfig, backlog_path: Path) -> str:
  """Step 3: build prompt for generating one new idea."""
  next_id = _next_id(items, cfg.id_prefix)
  id_format = f'{cfg.id_prefix}-NNN' if cfg.id_prefix else 'NNN (zero-padded)'
  parts = [
      f'You are the {cfg.role}.',
  ]
  if cfg.scan_prompt:
    parts.append(cfg.scan_prompt)
  if cfg.idea_prompt:
    parts.append(cfg.idea_prompt)
  parts.append(
      f'Generate exactly ONE new improvement idea not already in the backlog. '
      f'Append to {backlog_path} with fields: id (use {next_id}, format: {id_format}), '
      f'title, description (PURPOSE first, then HOW), status: pending, created (ISO 8601), priority.')
  parts.append(_language_rule(cfg.language))
  parts.append('Commit and push.')
  extra = _extra_rules_text(cfg.extra_rules)
  if extra:
    parts.append(extra)
  return ' '.join(parts)


def _build_scan_prompt(cfg: ImprovementLoopConfig, backlog_path: Path) -> str:
  """Step 4: build fallback scan prompt."""
  parts = [
      f'You are the {cfg.role}.',
  ]
  if cfg.scan_prompt:
    parts.append(cfg.scan_prompt)
  parts.append(
      f'If issues found, create ONE backlog item in {backlog_path} (status: pending). '
      f'If clean, do nothing.')
  parts.append(_language_rule(cfg.language))
  parts.append('Commit and push.')
  extra = _extra_rules_text(cfg.extra_rules)
  if extra:
    parts.append(extra)
  return ' '.join(parts)


async def determine_action(backlog_path: Path, loop_cfg: ImprovementLoopConfig,
                           repo_path: Path) -> tuple[str, Optional[str]]:
  """Determine the next improvement-loop action.

  Returns (action_type, prompt_text). action_type is one of:
    'revision', 'stale_reset', 'implement', 'generate', 'scan', 'noop'.
  prompt_text is None when no worker is needed (stale_reset, noop).
  """
  items = _load_backlog(backlog_path)

  # Step 0: revision feedback
  revision_prompt = _check_revision(items, backlog_path)
  if revision_prompt:
    return ('revision', revision_prompt)

  # Step 1: stale in_progress
  if await _handle_stale(items, backlog_path, loop_cfg, repo_path):
    return ('stale_reset', None)

  # Step 2: implement approved (highest priority first)
  approved = [i for i in items if i.get('status') == 'approved']
  if approved:
    approved.sort(key=lambda i: _PRIORITY_ORDER.get(i.get('priority', 'low'), 2))
    prompt = _build_implement_prompt(approved[0], loop_cfg, backlog_path)
    return ('implement', prompt)

  # Step 3: generate idea (no approved/in_progress, under cap)
  has_active = any(i.get('status') in ('approved', 'in_progress') for i in items)
  pending_count = sum(1 for i in items if i.get('status') == 'pending')
  if not has_active and pending_count < loop_cfg.max_pending:
    prompt = _build_generate_prompt(items, loop_cfg, backlog_path)
    return ('generate', prompt)

  # Step 4: fallback scan (skip generate if at cap, go straight to scan)
  if not has_active:
    prompt = _build_scan_prompt(loop_cfg, backlog_path)
    return ('scan', prompt)

  # Nothing applies
  return ('noop', None)
