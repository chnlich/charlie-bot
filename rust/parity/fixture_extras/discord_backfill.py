"""Create one unhandled Discord summon for the boot backfill scenario."""

from rust.parity.fixture import Builder

GUILD = "900000000000000010"
PARENT = "900000000000000020"
THREAD = "900000000000000070"
MENTION = "900000000000000080"


async def build(b: Builder) -> dict[str, str]:
  session = await b.node(
      "discord_backfill",
      "Synthetic lost Discord summon",
      slot_values={
          "discord_origin": {
              "guild_id": GUILD,
              "parent_channel_id": PARENT,
              "thread_id": THREAD,
          },
          "discord_watermark_id": MENTION,
      },
  )
  await b.say(
      session,
      {
          "type": "agent_message",
          "id": "synthetic-discord-lost-summon",
          "content": "Discord 线程召唤",
          "discord": {
              "guild_id": GUILD,
              "channel_id": THREAD,
              "thread_id": THREAD,
              "mention_id": MENTION,
          },
      },
  )
  await b.tree.completion.archive_subtree(
      session, request_id="fixture-archive-discord-session", summary="archived by the user", result_refs=[],
      run_ids=[], caller=b.operator)
  return {"session": session}
