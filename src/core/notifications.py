"""Telegram notification support for CharlieBot."""

from src.core import config, http, log_once, timeouts

log = log_once.LazyStructlogLogger()

TELEGRAM_MAX_MESSAGE_LENGTH = 4096


async def send_telegram(message: str, cfg: config.CharlieBotConfig) -> None:
  """Send a message via the Telegram Bot API.

  Raises ValueError if the credentials file has no telegram bot_token, and
  RuntimeError if telegram.chat_id is not configured.
  """
  bot_token = config.get_credentials().require("telegram", "bot_token")
  if not cfg.telegram.chat_id:
    raise RuntimeError("telegram.chat_id is not configured")

  truncated = message[:TELEGRAM_MAX_MESSAGE_LENGTH]
  url = f"https://api.telegram.org/bot{bot_token}/sendMessage"
  payload = {
      "chat_id": cfg.telegram.chat_id,
      "text": truncated,
      "parse_mode": "Markdown",
  }

  client = http.get_http_client()
  resp = await client.post(url, json=payload, timeout=timeouts.NOTIFICATION_TIMEOUT)

  if resp.status_code == 200:
    log.info("telegram_sent", chat_id=cfg.telegram.chat_id, length=len(truncated))
  else:
    log.error("telegram_send_failed", status=resp.status_code, body=resp.text[:200])
    raise RuntimeError(f"Telegram API returned {resp.status_code}: {resp.text[:200]}")
