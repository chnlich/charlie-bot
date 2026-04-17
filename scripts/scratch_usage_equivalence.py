"""Scratch verification: incremental _update_usage_cache == usage_from_events full scan.

Feeds the same [event_a, event_b, zero_event] stream through both paths and
asserts the resulting cache dicts are identical.
"""

from unittest.mock import MagicMock

from src.core.sessions import SessionManager


def main() -> None:
  event_a = {
      "type": "result",
      "total_cost_usd": 0.01,
      "usage": {
          "input_tokens": 100,
          "cache_creation_input_tokens": 20,
          "cache_read_input_tokens": 5,
      },
      "modelUsage": {
          "claude-opus-4-5": {
              "contextWindow": 200_000,
          },
      },
  }
  event_b = {
      "type": "result",
      "total_cost_usd": 0.02,
      "usage": {
          "input_tokens": 500,
          "cache_creation_input_tokens": 50,
          "cache_read_input_tokens": 10,
      },
      "modelUsage": {
          "claude-opus-4-5": {
              "contextWindow": 200_000,
          },
      },
  }
  zero_event = {
      "type": "result",
      "total_cost_usd": 0.003,
      "usage": {
          "input_tokens": 0,
          "cache_creation_input_tokens": 0,
          "cache_read_input_tokens": 0,
      },
      "modelUsage": {},
  }
  events = [event_a, event_b, zero_event]

  sm = SessionManager.__new__(SessionManager)
  sm._usage_cache = {}
  sid = "test"
  for ev in events:
    sm._update_usage_cache(sid, ev)
  incremental = sm._usage_cache[sid]

  full_scan = SessionManager.usage_from_events(events)

  assert incremental == full_scan, f"mismatch:\n  incr={incremental}\n  full={full_scan}"
  print("OK:", incremental)


if __name__ == "__main__":
  main()
