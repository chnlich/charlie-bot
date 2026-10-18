"""Create one fixture session with a synthetic Discord thread origin for reply scenarios."""

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
  return {"session": session}
