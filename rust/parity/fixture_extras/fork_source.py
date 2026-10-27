"""A quiet worker whose chat log has a known number of lines: the source of the fork and elone scenario."""

import json

from rust.parity.fixture import Builder

ANSWERS = 12


async def build(b: Builder) -> dict[str, str]:
  await b.node("fork_root", "Synthetic fork root")
  session_id = await b.node("fork_source", "Synthetic fork source", profile="worker", parent="fork_root")
  path = b.events.get_chat_events_path(session_id)
  created = path.read_text(encoding="utf-8").splitlines()
  if len(created) != 1:
    raise RuntimeError(f"a new worker holds {len(created)} chat lines, not one")
  lines = [
      json.dumps(
          {
              "type": "assistant",
              "id": f"fork-source-answer-{index}",
              "message": {
                  "content": [{
                      "type": "text",
                      "text": f"Synthetic answer {index}"
                  }]
              }
          },
          separators=(",", ":")) for index in range(ANSWERS)
  ]
  path.write_text("\n".join(created + lines) + "\n", encoding="utf-8")
  return {}
