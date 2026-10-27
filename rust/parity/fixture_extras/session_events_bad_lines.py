"""Create cold event-page sessions whose logs contain blank and malformed lines."""

import json

from rust.parity.fixture import Builder

EVENTS = [
    {
        "type": "user",
        "id": "line-0",
        "content": "Question zero"
    },
    "",
    "{malformed",
    {
        "type": "assistant",
        "id": "line-3",
        "message": {
            "content": [{
                "type": "text",
                "text": "Answer three"
            }]
        }
    },
    {
        "type": "master_done",
        "id": "line-4"
    },
    {
        "type": "user",
        "id": "line-5",
        "content": "Question five"
    },
    "",
    "{malformed",
    {
        "type": "assistant",
        "id": "line-8",
        "message": {
            "content": [{
                "type": "text",
                "text": "Answer eight"
            }]
        }
    },
    {
        "type": "master_done",
        "id": "line-9"
    },
]


async def build(b: Builder) -> dict[str, str]:
  sessions = {}
  for key in ("range_a", "range_b", "range_c", "range_d"):
    session_id = await b.node(key, "Synthetic broken event log")
    path = b.events.get_chat_events_path(session_id)
    path.parent.mkdir(parents=True, exist_ok=True)
    lines = [event if isinstance(event, str) else json.dumps(event, separators=(",", ":")) for event in EVENTS]
    path.write_text("\n".join(lines), encoding="utf-8")
    sessions[key] = session_id
  return sessions
