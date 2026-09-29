"""Unit tests for the Discord summon schema plumbing (config, session schema, reply render)."""

from __future__ import annotations

import pytest
from conftest import build_env, stub_credentials
from pydantic import ValidationError

from src.core import event_types as ET
from src.core.config import CharlieBotConfig, get_credentials
from src.core.message_aggregator import MessageAggregator
from src.core.models import CreateSessionRequest, DiscordOrigin, SessionMetadata

_GUILD = "100000000000000001"
_PARENT = "100000000000000002"
_THREAD = "100000000000000003"
_WATERMARK = "100000000000000004"


def test_config_without_discord_key_yields_empty_allow_list() -> None:
  cfg = CharlieBotConfig.model_validate({})
  assert cfg.discord.allowed_user_ids == []


def test_discord_unknown_keys_are_rejected() -> None:
  with pytest.raises(ValidationError):
    CharlieBotConfig.model_validate({"discord": {"allowed_channels": ["x"]}})


def test_discord_bot_token_comes_from_credentials() -> None:
  stub_credentials({"discord": {"bot_token": "test-bot-token"}})
  creds = get_credentials()
  assert creds.get("discord", "bot_token") == "test-bot-token"
  assert creds.require("discord", "bot_token") == "test-bot-token"


def test_session_metadata_discord_fields_round_trip_through_json() -> None:
  meta = SessionMetadata(
      name="t",
      discord_origin=DiscordOrigin(guild_id=_GUILD, parent_channel_id=_PARENT, thread_id=_THREAD),
      discord_watermark_id=_WATERMARK)
  loaded = SessionMetadata.model_validate_json(meta.model_dump_json())
  assert loaded.discord_origin == meta.discord_origin
  assert loaded.discord_watermark_id == _WATERMARK


def test_session_metadata_without_discord_fields_parses() -> None:
  meta = SessionMetadata.model_validate_json(SessionMetadata(name="t").model_dump_json())
  assert meta.discord_origin is None
  assert meta.discord_watermark_id is None


@pytest.mark.asyncio
async def test_create_session_persists_discord_origin(tmp_path) -> None:
  _, session_mgr, _ = build_env(tmp_path)
  origin = DiscordOrigin(guild_id=_GUILD, parent_channel_id=_PARENT, thread_id=_THREAD)
  meta = await session_mgr.create_session(CreateSessionRequest(name="d", discord_origin=origin))
  reloaded = await session_mgr.load_metadata(meta.id)
  assert reloaded.discord_origin == origin
  assert reloaded.discord_watermark_id is None


def test_discord_reply_renders_as_system_row() -> None:
  agg = MessageAggregator()
  deltas = list(
      agg.feed({
          "type": ET.DISCORD_REPLY,
          "content": "hi there",
          "discord_reply": {
              "answers": None,
              "chars": 8,
              "chunks": 1
          },
          "timestamp": "2026-08-26T08:00:00Z",
      }))
  assert len(deltas) == 1
  assert deltas[0]["message"]["role"] == "system"
  assert deltas[0]["message"]["content"] == "Posted to Discord: hi there"
