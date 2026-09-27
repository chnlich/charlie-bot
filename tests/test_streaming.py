"""Focused tests for handle_compaction_events."""

import pytest
from conftest import compact_boundary_event

from src.core import event_types as ET
from src.core.streaming import handle_compaction_events


async def _record(persisted: list[dict], event: dict) -> None:
  persisted.append(event)


@pytest.mark.asyncio
async def test_compact_boundary_emits_context_compacted_with_the_payload_whole() -> None:
  persisted: list[dict] = []
  event = compact_boundary_event(pre_tokens=239_708)

  await handle_compaction_events(event, lambda ev: _record(persisted, ev), {"session": "s1"})

  assert persisted == [
      {
          "type": ET.CONTEXT_COMPACTED,
          "trigger": "manual",
          ET.COMPACT_METADATA: {
              "trigger": "manual",
              "pre_tokens": 239_708
          },
      }
  ]


_STATUS_FAILED_ROWS = [
    pytest.param(
        {
            "type": "system",
            "subtype": "status",
            "compact_result": "failed",
            "compact_error": "context too large",
        },
        "context too large",
        id="carrying-compact-error"),
    pytest.param({
        "type": "system",
        "subtype": "status",
        "compact_result": "failed",
    }, None, id="compact-error-absent"),
]


@pytest.mark.asyncio
@pytest.mark.parametrize(("event", "expected_error"), _STATUS_FAILED_ROWS)
async def test_status_failed_emits_context_compact_failed(event: dict, expected_error: str | None) -> None:
  persisted: list[dict] = []

  await handle_compaction_events(event, lambda ev: _record(persisted, ev), {"session": "s1"})

  assert persisted == [{
      "type": ET.CONTEXT_COMPACT_FAILED,
      "error": expected_error,
  }]
