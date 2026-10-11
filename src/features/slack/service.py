"""The Slack entrypoint service: the Socket Mode listener and the boot backfill of lost summons."""

from src.features.chat_threads import channel_service
from src.features.slack import slack_listener
from src.infra import config
from src.runtime.hooks import wiring

_service = channel_service.ChannelService("slack", slack_listener)


async def start_service(ctx: wiring.ServiceContext) -> None:
  """Start the listener and the backfill when the Slack credentials and an allowed user are configured."""
  creds = config.get_credentials()
  await _service.start(
      ctx,
      configured=bool(
          creds.get("slack", "bot_token") and creds.get("slack", "app_token") and ctx.cfg.slack.allowed_user_ids))


async def stop_service() -> None:
  """Cancel the listener and the backfill; returns at once when start_service started neither."""
  await _service.stop()
