"""Focused tests for handle_compaction_events."""

import pytest

from src.core import event_types as ET
from src.core.streaming import handle_compaction_events


async def _record(persisted: list[dict], event: dict) -> None:
  persisted.append(event)


@pytest.mark.asyncio
async def test_compact_boundary_still_emits_context_compacted_unchanged() -> None:
  persisted: list[dict] = []
  event = {
      "type": "system",
      "subtype": "compact_boundary",
      "compact_metadata": {"trigger": "manual", "pre_tokens": 239_708},
  }

  await handle_compaction_events(event, lambda ev: _record(persisted, ev), {"session": "s1"})

  assert persisted == [{
      "type": ET.CONTEXT_COMPACTED,
      "trigger": "manual",
      "pre_tokens": 239_708,
  }]


@pytest.mark.asyncio
async def test_status_failed_with_error_emits_context_compact_failed_carrying_it() -> None:
  persisted: list[dict] = []
  event = {
      "type": "system",
      "subtype": "status",
      "compact_result": "failed",
      "compact_error": "context too large",
  }

  await handle_compaction_events(event, lambda ev: _record(persisted, ev), {"session": "s1"})

  assert persisted == [{
      "type": ET.CONTEXT_COMPACT_FAILED,
      "error": "context too large",
  }]


@pytest.mark.asyncio
async def test_status_failed_without_error_emits_error_none() -> None:
  persisted: list[dict] = []
  event = {
      "type": "system",
      "subtype": "status",
      "compact_result": "failed",
  }

  await handle_compaction_events(event, lambda ev: _record(persisted, ev), {"session": "s1"})

  assert persisted == [{
      "type": ET.CONTEXT_COMPACT_FAILED,
      "error": None,
  }]


@pytest.mark.asyncio
async def test_status_success_emits_nothing() -> None:
  persisted: list[dict] = []
  event = {
      "type": "system",
      "subtype": "status",
      "compact_result": "success",
  }

  await handle_compaction_events(event, lambda ev: _record(persisted, ev), {"session": "s1"})

  assert not persisted


@pytest.mark.asyncio
async def test_status_event_with_no_compact_result_emits_nothing() -> None:
  persisted: list[dict] = []
  event = {
      "type": "system",
      "subtype": "status",
  }

  await handle_compaction_events(event, lambda ev: _record(persisted, ev), {"session": "s1"})

  assert not persisted


@pytest.mark.asyncio
async def test_non_system_event_emits_nothing() -> None:
  persisted: list[dict] = []
  event = {
      "type": "assistant",
      "subtype": "status",
      "compact_result": "failed",
  }

  await handle_compaction_events(event, lambda ev: _record(persisted, ev), {"session": "s1"})

  assert not persisted
