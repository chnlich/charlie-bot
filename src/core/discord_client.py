"""Discord REST client — every HTTP call the Discord entrypoint makes.

``DiscordClient`` is the single seam for Discord's v10 REST API: one method
per endpoint, the bot token in an ``Authorization: Bot`` header, and one error
type (``DiscordAPIError``) that names the failing call instead of leaking the
token. The 429 policy lives in one place (``_request``): sleep the
``retry_after`` the API reports and retry, at most twice, then raise.

Nothing calls this module yet — a later step wires the Discord listener and
the CLI onto it. The module-level helpers (snowflake comparison, message
links, the message-content intent check, the permission bit table) are pure,
so the listener can reason about ids and permissions without an HTTP client.
"""

from __future__ import annotations

import asyncio
import json
import re
import urllib.parse
from collections.abc import Sequence
from pathlib import Path
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
  import httpx

# The pinned major version of Discord's REST API every path below hits.
BASE_URL = "https://discord.com/api/v10"

# The application flag bit that unlocks real message text over the gateway,
# plus its limited variant (content without mentions/roles/everyone).
_MESSAGE_CONTENT_FLAG = 1 << 18
_MESSAGE_CONTENT_LIMITED_FLAG = 1 << 19

# The permission that implicitly grants everything else; a bot holding it
# never reports missing permissions.
ADMINISTRATOR = 1 << 3

# The permission bits the Discord entrypoint needs, name to bit, per
# https://discord.com/developers/docs/topics/permissions.
REQUIRED_PERMISSIONS: dict[str, int] = {
    "VIEW_CHANNEL": 1 << 10,
    "SEND_MESSAGES": 1 << 11,
    "SEND_MESSAGES_IN_THREADS": 1 << 38,
    "CREATE_PUBLIC_THREADS": 1 << 35,
    "READ_MESSAGE_HISTORY": 1 << 16,
    "ADD_REACTIONS": 1 << 6,
    "ATTACH_FILES": 1 << 15,
}

# The shape message_link emits and parse_message_link accepts; ids are
# snowflakes, so always digits.
_MESSAGE_LINK_RE = re.compile(r"^https://discord\.com/channels/(\d+)/(\d+)(?:/(\d+))?$")

# Discord's hard cap on a thread name; longer names are truncated, not refused.
_THREAD_NAME_MAX_CHARS = 100

# Retries of one call after a 429; a third rate-limit answer gives up.
_MAX_429_RETRIES = 2


def snowflake_key(snowflake: str) -> int:
  """A Discord id as an integer: ids are compared and sorted as numbers, never
  as strings (string order flips whenever the digit counts differ)."""
  return int(snowflake)


def message_link(guild_id: str, channel_id: str, message_id: str | None = None) -> str:
  """The https://discord.com/channels/... link to a channel, or to one message
  inside it when ``message_id`` is given."""
  link = f"https://discord.com/channels/{guild_id}/{channel_id}"
  if message_id is not None:
    link += f"/{message_id}"
  return link


def parse_message_link(url: str) -> tuple[str, str, str | None]:
  """Inverse of ``message_link``: (guild_id, channel_id, message_id) with the
  message id None when the link points at the channel. Anything that is not
  such a link raises ValueError."""
  match = _MESSAGE_LINK_RE.match(url)
  if match is None:
    raise ValueError(f"not a discord.com channel/message link: {url!r}")
  return match.group(1), match.group(2), match.group(3)


def message_content_intent_enabled(application_flags: int) -> bool:
  """True when the application's flags grant the message-content intent — the
  full flag, or the limited one (content stripped of mentions and @everyone)."""
  return bool(application_flags & (_MESSAGE_CONTENT_FLAG | _MESSAGE_CONTENT_LIMITED_FLAG))


def missing_permissions(permissions: int) -> list[str]:
  """The names from ``REQUIRED_PERMISSIONS`` the permission bitfield lacks, in
  table order; empty when ADMINISTRATOR is set, since it grants everything."""
  if permissions & ADMINISTRATOR:
    return []
  return [name for name, bit in REQUIRED_PERMISSIONS.items() if not permissions & bit]


class DiscordAPIError(Exception):
  """One failed Discord REST call: method, path, HTTP status, Discord's error
  code from the JSON body (None when absent), and the human message."""

  def __init__(self, method: str, path: str, status: int, code: int | None, message: str) -> None:
    super().__init__(message)
    self.method = method
    self.path = path
    self.status = status
    self.code = code
    self.message = message

  def __str__(self) -> str:
    return f"{self.method} {self.path} failed: HTTP {self.status} code={self.code} message={self.message!r}"

  @property
  def too_large(self) -> bool:
    """True for payload-too-large rejections: HTTP 413, or Discord's code 40005."""
    return self.status == 413 or self.code == 40005


class DiscordClient:
  """Thin Discord v10 REST wrapper: one method per API call, all of them
  funneling through ``_request`` for headers, error mapping and the 429 retry."""

  def __init__(self, http: httpx.AsyncClient, *, bot_token: str) -> None:
    self._http = http
    # Sent with every call; the token never appears anywhere else, and no
    # error path echoes headers.
    self._headers = {
        "Authorization": f"Bot {bot_token}",
        "User-Agent": "DiscordBot (https://discord.com, charlie-bot)",
    }

  @staticmethod
  def _retry_after_seconds(resp: httpx.Response) -> float:
    """The 429 wait Discord reports: the JSON body's ``retry_after`` seconds,
    falling back to the ``Retry-After`` header when the body carries none."""
    try:
      body = resp.json()
    except ValueError:
      body = None
    if isinstance(body, dict) and isinstance(body.get("retry_after"), (int, float)):
      return float(body["retry_after"])
    return float(resp.headers.get("Retry-After", "0"))

  async def _request(
      self,
      method: str,
      path: str,
      *,
      json_body: Any = None,
      data: dict[str, str] | None = None,
      files: Sequence[tuple[str, tuple[str, bytes, str]]] | None = None,
      params: dict[str, Any] | None = None,
  ) -> Any:
    """One Discord REST call: send it with the bot headers, retry a 429 up to
    ``_MAX_429_RETRIES`` times after the reported wait, return the decoded JSON
    body (None when the 2xx answer has none, e.g. the reaction endpoints), and
    raise ``DiscordAPIError`` for anything else non-2xx."""
    url = f"{BASE_URL}{path}"
    for retries in range(_MAX_429_RETRIES + 1):
      resp = await self._http.request(
          method,
          url,
          headers=self._headers,
          json=json_body,
          data=data,
          files=files,
          params=params,
      )
      if resp.status_code == 429 and retries < _MAX_429_RETRIES:
        await asyncio.sleep(self._retry_after_seconds(resp))
        continue
      break
    if resp.is_success:
      return resp.json() if resp.content else None
    try:
      body = resp.json()
    except ValueError:
      body = None
    code = body.get("code") if isinstance(body, dict) else None
    message = body.get("message") if isinstance(body, dict) else None
    if not isinstance(message, str):
      message = resp.text
    raise DiscordAPIError(method, path, resp.status_code, code, message)

  async def get_gateway_url(self) -> str:
    """GET /gateway/bot; returns the wss: gateway url to connect to."""
    payload = await self._request("GET", "/gateway/bot")
    return payload["url"]

  async def get_current_application(self) -> dict:
    """GET /applications/@me; returns the bot's application, flags included."""
    return await self._request("GET", "/applications/@me")

  async def get_current_user(self) -> dict:
    """GET /users/@me; returns the bot's own user object."""
    return await self._request("GET", "/users/@me")

  async def list_current_user_guilds(self) -> list[dict]:
    """GET /users/@me/guilds; returns the guilds the bot is in, each carrying
    its ``permissions`` bitfield as a string."""
    return await self._request("GET", "/users/@me/guilds")

  async def get_channel(self, channel_id: str) -> dict:
    """GET /channels/{channel_id}; returns the channel object."""
    return await self._request("GET", f"/channels/{channel_id}")

  async def start_thread_from_message(
      self,
      channel_id: str,
      message_id: str,
      name: str,
      *,
      auto_archive_duration: int = 10080,
  ) -> dict:
    """POST start-thread-from-message: open a thread rooted at one message.
    The name is truncated to Discord's 100-character cap."""
    return await self._request(
        "POST",
        f"/channels/{channel_id}/messages/{message_id}/threads",
        json_body={
            "name": name[:_THREAD_NAME_MAX_CHARS],
            "auto_archive_duration": auto_archive_duration,
        },
    )

  async def create_message(self, channel_id: str, content: str, *, files: Sequence[Path] = ()) -> dict:
    """POST /channels/{channel_id}/messages.

    ``allowed_mentions`` is always ``{"parse": []}`` so a reply never pings
    anyone. Without files the body is JSON; with files it is multipart — one
    ``payload_json`` part (content, allowed_mentions, and the attachment
    metadata Discord requires) plus one ``files[i]`` part per file.
    """
    path = f"/channels/{channel_id}/messages"
    if not files:
      return await self._request("POST", path, json_body={"content": content, "allowed_mentions": {"parse": []}})
    attachments = [{"id": index, "filename": file.name} for index, file in enumerate(files)]
    payload_json = json.dumps({"content": content, "allowed_mentions": {"parse": []}, "attachments": attachments})
    return await self._request(
        "POST",
        path,
        data={"payload_json": payload_json},
        files=[
            (f"files[{index}]", (file.name, file.read_bytes(), "application/octet-stream"))
            for index, file in enumerate(files)
        ],
    )

  async def add_reaction(self, channel_id: str, message_id: str, emoji: str) -> None:
    """PUT the bot's own reaction (URL-encoded emoji) onto one message."""
    await self._request(
        "PUT",
        f"/channels/{channel_id}/messages/{message_id}/reactions/{urllib.parse.quote(emoji, safe='')}/@me",
    )

  async def remove_own_reaction(self, channel_id: str, message_id: str, emoji: str) -> None:
    """DELETE the bot's own reaction (URL-encoded emoji) from one message."""
    await self._request(
        "DELETE",
        f"/channels/{channel_id}/messages/{message_id}/reactions/{urllib.parse.quote(emoji, safe='')}/@me",
    )

  async def get_messages(self, channel_id: str, *, after: str | None = None, limit: int = 100) -> list[dict]:
    """GET /channels/{channel_id}/messages with optional ``after`` and
    ``limit`` (1 to 100); returned oldest first by integer id, though Discord
    hands them back newest first."""
    params: dict[str, Any] = {"limit": limit}
    if after is not None:
      params["after"] = after
    messages = await self._request("GET", f"/channels/{channel_id}/messages", params=params)
    return sorted(messages, key=lambda message: snowflake_key(message["id"]))
