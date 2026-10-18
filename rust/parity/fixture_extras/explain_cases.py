"""Create the explain parity session: dividers with and without assistant text, and one stale pending entry.

The chat events go through the server's chat-event writer, so each divider takes its event index in the log. The
stale entry goes through the server's results writer: the GET routes meet it as a restarted server leaves it.
"""

from rust.parity.fixture import Builder
from src.features.explain import explain
from src.runtime.agent_process import base

# A request time far past the one-shot budget, so every read treats the entry as stale.
STALE_REQUESTED_AT = "2020-01-01T00:00:00+00:00"
STALE_UPTO = 12


async def build(b: Builder) -> dict:
  session = await b.node("explain", "Explain parity")
  chat = b.events.chat_events
  divider = base.make_master_done_event(0, still_thinking=False)
  # Index 0: a divider before any assistant text.
  await chat.save_chat_event(session, dict(divider))
  for index, answer in enumerate(("First answer: the plan was approved.", "Second answer.", "Third answer."), start=1):
    question = {
        "type": "user",
        "message": {
            "role": "user",
            "content": [{
                "type": "text",
                "text": f"Question {index}."
            }]
        }
    }
    reply = {
        "type": "assistant",
        "message": {
            "role": "assistant",
            "model": "claude-sonnet-5",
            "content": [{
                "type": "text",
                "text": answer
            }],
        },
    }
    await chat.save_chat_event(session, question)
    await chat.save_chat_event(session, reply)
    await chat.save_chat_event(session, dict(divider))
  stale = {
      "state": "pending",
      "backend": b.option.id,
      "answer": "",
      "error": None,
      "requested_at": STALE_REQUESTED_AT,
      "generated_at": None,
  }
  explain._write_entry(b.events, session, STALE_UPTO, stale)
  return {"stale_upto": STALE_UPTO, "dividers": [0, 3, 6, 9]}
