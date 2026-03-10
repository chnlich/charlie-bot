"""Validate that loop/backlog.yaml is well-formed."""

from pathlib import Path

import pytest
import yaml

BACKLOG_PATH = Path(__file__).resolve().parent.parent / 'loop' / 'backlog.yaml'

VALID_STATUSES = frozenset({
    'pending',
    'approved',
    'in_progress',
    'done',
    'rejected',
    'failed',
    'revision_requested',
})

REQUIRED_FIELDS = ('id', 'title', 'status')


@pytest.fixture(scope='module')
def backlog_items() -> list[dict]:
  text = BACKLOG_PATH.read_text(encoding='utf-8')
  data = yaml.safe_load(text)
  return data


def test_parses_without_error(backlog_items: list[dict]) -> None:
  """yaml.safe_load succeeds and returns a list."""
  assert isinstance(backlog_items, list)


def test_non_empty(backlog_items: list[dict]) -> None:
  assert len(backlog_items) > 0, 'backlog.yaml must contain at least one item'


@pytest.mark.parametrize('field', REQUIRED_FIELDS)
def test_required_fields_present(backlog_items: list[dict], field: str) -> None:
  for item in backlog_items:
    assert field in item, f"item {item.get('id', '???')} missing required field '{field}'"


def test_status_values_valid(backlog_items: list[dict]) -> None:
  for item in backlog_items:
    status = item['status']
    assert status in VALID_STATUSES, (
        f"item {item['id']} has invalid status '{status}'; expected one of {sorted(VALID_STATUSES)}"
    )
