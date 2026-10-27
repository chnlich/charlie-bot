"""Feed a JSON list of chat events through the Python aggregator and print its frames.

The cross-implementation test uses this helper to compare the Rust port against the Python
baseline on inputs whose fields take a shape Python accepts but Rust used to reject.
"""

import json
import sys

from src.runtime.message_aggregator import MessageAggregator


def main() -> None:
  with open(sys.argv[1], encoding="utf-8") as events_file:
    events = json.load(events_file)
  aggregator = MessageAggregator(0, emit_stream_deltas=True)
  frames = []
  for event in events:
    frames.extend(aggregator.feed(event))
  json.dump(frames, sys.stdout)


if __name__ == "__main__":
  main()
