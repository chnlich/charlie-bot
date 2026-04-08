"""Telegram notification support for CharlieBot."""

import structlog

from src.core.config import CharlieBotConfig
from src.core.http import get_http_client
from src.core.timeouts import NOTIFICATION_TIMEOUT

log = structlog.get_logger()

TELEGRAM_MAX_MESSAGE_LENGTH = 4096


async def send_telegram(message: str, cfg: CharlieBotConfig) -> None:
  """Send a message via the Telegram Bot API.

  Raises RuntimeError if telegram_bot_token or telegram_chat_id is not configured.
  """
  if not cfg.telegram_bot_token:
    raise RuntimeError("telegram_bot_token is not configured")
  if not cfg.telegram_chat_id:
    raise RuntimeError("telegram_chat_id is not configured")

  truncated = message[:TELEGRAM_MAX_MESSAGE_LENGTH]
  url = f"https://api.telegram.org/bot{cfg.telegram_bot_token}/sendMessage"
  payload = {
      "chat_id": cfg.telegram_chat_id,
      "text": truncated,
      "parse_mode": "Markdown",
  }

  client = get_http_client()
  resp = await client.post(url, json=payload, timeout=NOTIFICATION_TIMEOUT)

  if resp.status_code == 200:
    log.info("telegram_sent", chat_id=cfg.telegram_chat_id, length=len(truncated))
  else:
    log.error("telegram_send_failed", status=resp.status_code, body=resp.text[:200])
    raise RuntimeError(f"Telegram API returned {resp.status_code}: {resp.text[:200]}")
