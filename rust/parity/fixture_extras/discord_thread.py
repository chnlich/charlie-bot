"""Create a Discord thread session and a session without a thread for route scenarios."""

from rust.parity.fixture import Builder


async def build(b: Builder) -> dict:
  session = await b.node(
      "discord_thread",
      "Discord parity thread",
      slot_values={
          "discord_origin":
              {
                  "guild_id": "900000000000000010",
                  "parent_channel_id": "900000000000000020",
                  "thread_id": "900000000000000030",
              }
      },
  )
  no_thread_session = await b.node("discord_no_thread", "Discord parity session without a thread")
  return {"session": session, "no_thread_session": no_thread_session}
