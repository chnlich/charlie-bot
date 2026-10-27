"""Create the session-stream parity session: a chat log that ends in an unflushed assistant draft.

The catch-up walk buffers the draft as its latest stream delta, so the session's replay ends with one
stream frame instead of a committed message.
"""

from rust.parity.fixture import Builder


async def build(b: Builder) -> dict:
  session = await b.node("stream_case", "Stream case")
  chat = b.events.chat_events
  await chat.save_chat_event(
      session,
      {
          "type": "user",
          "message": {
              "role": "user",
              "content": [{
                  "type": "text",
                  "text": "Question for the stream case."
              }]
          }
      })
  await chat.save_chat_event(
      session,
      {
          "type": "assistant",
          "message": {
              "role": "assistant",
              "model": "claude-sonnet-5",
              "content": [{
                  "type": "text",
                  "text": "An unfinished synthetic draft."
              }]
          },
      })
  return {"draft": "An unfinished synthetic draft."}
