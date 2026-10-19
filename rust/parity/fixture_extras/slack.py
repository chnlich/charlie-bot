"""Create a Slack thread whose watermark makes one later message stale."""

from rust.parity.fixture import Builder


async def build(b: Builder) -> dict[str, str]:
  session = await b.node(
      "slack_stale",
      "Synthetic stale Slack thread",
      slot_values={
          "slack_origin": {
              "team_id": "T0PARITY",
              "channel_id": "C_STALE",
              "thread_ts": "1767600000.001000",
          },
          "slack_watermark_ts": "1767600000.001000",
      },
  )
  return {"session": session}
