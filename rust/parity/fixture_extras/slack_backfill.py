"""Create one pending Slack summon for the boot backfill scenario."""

from rust.parity.fixture import Builder

SUMMON_TS = "1767600000.002000"


async def build(b: Builder) -> dict[str, str]:
  session = await b.node(
      "slack_backfill",
      "Synthetic lost Slack summon",
      slot_values={
          "slack_origin": {
              "team_id": "T0PARITY",
              "channel_id": "C_LOST",
              "thread_ts": SUMMON_TS,
          },
          "slack_watermark_ts": SUMMON_TS,
      },
  )
  await b.say(
      session,
      {
          "type": "agent_message",
          "id": "synthetic-slack-lost-summon",
          "content": "Slack 线程召唤",
          "slack": {
              "channel_id": "C_LOST",
              "thread_ts": SUMMON_TS,
              "mention_ts": SUMMON_TS,
          },
      },
  )
  await b.tree.completion.archive_subtree(
      session, request_id="fixture-archive-slack-session", summary="archived by the user", result_refs=[], run_ids=[],
      caller=b.operator)
  return {"session": session}
