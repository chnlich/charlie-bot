"""Create a Slack thread with two unacknowledged follow-up messages."""

from rust.parity.fixture import Builder


async def build(b: Builder) -> dict[str, str]:
  session = await b.node(
      "slack_ack",
      "Synthetic Slack ack thread",
      slot_values={
          "slack_origin": {
              "team_id": "T0PARITY",
              "channel_id": "C_ACK",
              "thread_ts": "1767600000.003000",
          },
          "slack_watermark_ts": "1767600000.003100",
      },
  )
  return {"session": session}
