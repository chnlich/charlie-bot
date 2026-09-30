"""Unit tests for the Discord REST client (src.core.discord_client).

Every test drives DiscordClient through httpx.MockTransport: the request the
client would put on the wire is captured and asserted, no real network. Ids
are synthetic throughout.
"""

from __future__ import annotations

import json
from unittest.mock import AsyncMock, call, patch

import httpx
import pytest

from src.core.discord_client import (
    BASE_URL,
    REQUIRED_PERMISSIONS,
    DiscordAPIError,
    DiscordClient,
    message_content_intent_enabled,
    message_link,
    missing_permissions,
    parse_message_link,
    snowflake_key,
)

_TOKEN = "synthetic-token-not-a-secret"


def _client(handler) -> DiscordClient:
  """A client wired to an in-memory transport instead of the network."""
  return DiscordClient(httpx.AsyncClient(transport=httpx.MockTransport(handler)), bot_token=_TOKEN)


@pytest.mark.asyncio
async def test_request_headers_carry_bot_token_and_user_agent():
  """Every call authenticates with 'Bot <token>' and the DiscordBot user agent."""
  seen = {}

  def handler(request: httpx.Request) -> httpx.Response:
    seen["url"] = str(request.url)
    seen["headers"] = dict(request.headers)
    return httpx.Response(200, json={"id": "111111111111111111"})

  assert await _client(handler).get_current_user() == {"id": "111111111111111111"}
  assert seen["url"] == "https://discord.com/api/v10/users/@me"
  assert seen["headers"]["authorization"] == f"Bot {_TOKEN}"
  assert seen["headers"]["user-agent"] == "DiscordBot (https://discord.com, charlie-bot)"


@pytest.mark.asyncio
async def test_create_message_json_carries_allowed_mentions():
  """A text-only message posts JSON whose allowed_mentions never pings anyone."""

  def handler(request: httpx.Request) -> httpx.Response:
    seen["body"] = json.loads(request.read())
    return httpx.Response(200, json={"id": "222222222222222222"})

  seen: dict = {}
  result = await _client(handler).create_message("333333333333333333", "hello")
  assert result == {"id": "222222222222222222"}
  assert seen["body"] == {"content": "hello", "allowed_mentions": {"parse": []}}


@pytest.mark.asyncio
async def test_rate_limit_retries_once_with_reported_wait():
  """A 429 sleeps the retry_after the body reports, then the retry succeeds."""

  def handler(request: httpx.Request) -> httpx.Response:
    count = seen["count"]
    seen["count"] = count + 1
    if count == 0:
      return httpx.Response(429, json={"message": "You are being rate limited.", "retry_after": 1.25})
    return httpx.Response(200, json={"url": "wss://gateway.example/synthetic"})

  seen: dict = {"count": 0}
  sleep = AsyncMock()
  with patch("src.core.discord_client.asyncio.sleep", new=sleep):
    result = await _client(handler).get_gateway_url()
  assert result == "wss://gateway.example/synthetic"
  assert seen["count"] == 2
  assert sleep.await_args_list == [call(1.25)]


@pytest.mark.asyncio
async def test_rate_limit_three_times_raises():
  """After the initial call plus two retries still answered with 429, it raises."""

  def handler(request: httpx.Request) -> httpx.Response:
    seen["count"] += 1
    return httpx.Response(429, headers={"Retry-After": "2"}, json={"message": "rate limited"})

  seen: dict = {"count": 0}
  sleep = AsyncMock()
  with patch("src.core.discord_client.asyncio.sleep", new=sleep), pytest.raises(DiscordAPIError) as exc_info:
    await _client(handler).get_current_user()
  error = exc_info.value
  assert error.status == 429
  assert error.path == "/users/@me"
  assert seen["count"] == 3
  # Both retries waited on the Retry-After header fallback.
  assert sleep.await_args_list == [call(2.0), call(2.0)]


@pytest.mark.asyncio
async def test_http_error_carries_code_and_message_without_token():
  """A 403 with Discord's JSON error body raises, naming the failure and never the token."""

  def handler(request: httpx.Request) -> httpx.Response:
    return httpx.Response(403, json={"code": 50013, "message": "Missing Permissions"})

  with pytest.raises(DiscordAPIError) as exc_info:
    await _client(handler).get_channel("555555555555555555")
  error = exc_info.value
  assert (error.method, error.path, error.status, error.code,
          error.message) == ("GET", "/channels/555555555555555555", 403, 50013, "Missing Permissions")
  text = str(error)
  for fragment in ("GET", "/channels/555555555555555555", "403", "50013", "Missing Permissions"):
    assert fragment in text
  assert _TOKEN not in text


@pytest.mark.asyncio
async def test_get_message_fetches_one_message_by_id():
  """get_message GETs the single-message path and returns its object as is."""
  body = {"id": "222222222222222222", "content": "hello", "author": {"id": "333333333333333333"}}

  def handler(request: httpx.Request) -> httpx.Response:
    seen["url"] = str(request.url)
    return httpx.Response(200, json=body)

  seen: dict = {}
  assert await _client(handler).get_message("111111111111111111", "222222222222222222") == body
  assert seen["url"] == f"{BASE_URL}/channels/111111111111111111/messages/222222222222222222"


@pytest.mark.asyncio
async def test_get_messages_sorts_oldest_first_by_integer_id():
  """Discord returns newest first; ids of different digit counts sort as integers."""

  def handler(request: httpx.Request) -> httpx.Response:
    seen["params"] = dict(request.url.params)
    return httpx.Response(
        200,
        json=[
            {
                "id": "100000000000000000",
                "content": "newest"
            },
            {
                "id": "99999999999999999",
                "content": "oldest"
            },
        ])

  seen: dict = {}
  messages = await _client(handler).get_messages("666666666666666666", after="888888888888888888")
  assert [message["content"] for message in messages] == ["oldest", "newest"]
  assert seen["params"] == {"limit": "100", "after": "888888888888888888"}
  # The same ordering comparison, directly: string order would flip it.
  assert snowflake_key("99999999999999999") < snowflake_key("100000000000000000")
  assert sorted(["100000000000000000", "99999999999999999"]) == ["100000000000000000", "99999999999999999"]


@pytest.mark.asyncio
async def test_thread_name_truncated_to_100_chars():
  """start_thread_from_message truncates a too-long name instead of posting it."""

  def handler(request: httpx.Request) -> httpx.Response:
    seen["body"] = json.loads(request.read())
    seen["url"] = str(request.url)
    return httpx.Response(200, json={"id": "777777777777777777"})

  seen: dict = {}
  await _client(handler).start_thread_from_message("333333333333333333", "222222222222222222", "x" * 140)
  assert seen["url"] == f"{BASE_URL}/channels/333333333333333333/messages/222222222222222222/threads"
  assert seen["body"] == {"name": "x" * 100, "auto_archive_duration": 10080}


@pytest.mark.asyncio
async def test_reaction_emoji_is_url_encoded():
  """Reactions target /reactions/{url-encoded emoji}/@me with the right method."""
  seen: dict = {}

  def handler(request: httpx.Request) -> httpx.Response:
    seen[request.method] = str(request.url)
    return httpx.Response(204)

  client = _client(handler)
  assert await client.add_reaction("333333333333333333", "222222222222222222", "\U0001F440") is None
  assert await client.remove_own_reaction("333333333333333333", "222222222222222222", "\U0001F440") is None
  reaction_path = "/channels/333333333333333333/messages/222222222222222222/reactions/%F0%9F%91%80/@me"
  assert seen["PUT"] == f"{BASE_URL}{reaction_path}"
  assert seen["DELETE"] == seen["PUT"]


def test_message_link_round_trip():
  """message_link and parse_message_link invert each other, message id optional."""
  assert message_link(
      "111111111111111111",
      "222222222222222222") == "https://discord.com/channels/111111111111111111/222222222222222222"
  assert message_link("111111111111111111", "222222222222222222", "333333333333333333") == (
      "https://discord.com/channels/111111111111111111/222222222222222222/333333333333333333")
  assert parse_message_link("https://discord.com/channels/111111111111111111/222222222222222222") == (
      "111111111111111111", "222222222222222222", None)
  assert parse_message_link("https://discord.com/channels/111111111111111111/222222222222222222/333333333333333333"
                           ) == ("111111111111111111", "222222222222222222", "333333333333333333")


def test_parse_message_link_rejects_non_discord_urls():
  """Anything that is not a discord.com channel/message link raises ValueError."""
  for bad in (
      "https://slack.com/channels/111111111111111111/222222222222222222",
      "http://discord.com/channels/111111111111111111/222222222222222222",
      "https://discord.com/channels/111111111111111111",
      "https://discord.com/channels/111111111111111111/222222222222222222/333333333333333333/444444444444444444",
      "https://discord.com/channels/guild/222222222222222222",
      "",
  ):
    with pytest.raises(ValueError):
      parse_message_link(bad)


def test_message_content_intent_bits():
  """The intent is enabled by 1<<18 (full) or 1<<19 (limited), nothing else."""
  assert not message_content_intent_enabled(0)
  assert message_content_intent_enabled(1 << 18)
  assert message_content_intent_enabled(1 << 19)
  assert not message_content_intent_enabled(1 << 17)


def test_missing_permissions_table_and_administrator():
  """missing_permissions names the absent bits; ADMINISTRATOR grants everything."""
  assert list(REQUIRED_PERMISSIONS) == [
      "VIEW_CHANNEL",
      "SEND_MESSAGES",
      "SEND_MESSAGES_IN_THREADS",
      "CREATE_PUBLIC_THREADS",
      "READ_MESSAGE_HISTORY",
      "ADD_REACTIONS",
  ]
  assert missing_permissions(0) == [
      "VIEW_CHANNEL",
      "SEND_MESSAGES",
      "SEND_MESSAGES_IN_THREADS",
      "CREATE_PUBLIC_THREADS",
      "READ_MESSAGE_HISTORY",
      "ADD_REACTIONS",
  ]
  assert missing_permissions(sum(REQUIRED_PERMISSIONS.values())) == []
  assert missing_permissions(1 << 3) == []
  have = (1 << 10) | (1 << 11)
  assert missing_permissions(have) == [
      "SEND_MESSAGES_IN_THREADS",
      "CREATE_PUBLIC_THREADS",
      "READ_MESSAGE_HISTORY",
      "ADD_REACTIONS",
  ]
