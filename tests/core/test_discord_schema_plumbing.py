"""Unit tests for the Discord summon schema plumbing (config, session schema, reply render)."""

from __future__ import annotations

import conftest
import pydantic
import pytest

from src.infra import config, models
from src.features.discord.event_types import DISCORD_REPLY
from src.features.discord.metadata import DiscordOrigin
from src.infra import metadata_slots
from src.runtime import message_aggregator

_GUILD = "100000000000000001"
_PARENT = "100000000000000002"
_THREAD = "100000000000000003"
_WATERMARK = "100000000000000004"


def test_config_without_discord_key_yields_an_empty_map() -> None:
  cfg = config.CharlieBotConfig.model_validate({})
  assert cfg.discord.allowed_users == {}


def test_discord_unknown_keys_are_rejected() -> None:
  with pytest.raises(pydantic.ValidationError):
    config.CharlieBotConfig.model_validate({"discord": {"allowed_channels": ["x"]}})


def test_discord_legacy_allow_list_key_is_rejected_with_its_successor_named() -> None:
  """The retired discord.allowed_user_ids list fails at load, naming the map that replaced it."""
  with pytest.raises(pydantic.ValidationError) as excinfo:
    config.CharlieBotConfig.model_validate({"discord": {"allowed_user_ids": ["700000000000000001"]}})
  assert "discord.allowed_user_ids" in str(excinfo.value)
  assert "discord.allowed_users" in str(excinfo.value)


def test_discord_bot_token_comes_from_credentials() -> None:
  conftest.stub_credentials({"discord": {"bot_token": "test-bot-token"}})
  creds = config.get_credentials()
  assert creds.get("discord", "bot_token") == "test-bot-token"
  assert creds.require("discord", "bot_token") == "test-bot-token"


def test_session_metadata_discord_fields_round_trip_through_json() -> None:
  meta = models.SessionMetadata(
      profile="manager",
      name="t",
      discord_origin=DiscordOrigin(guild_id=_GUILD, parent_channel_id=_PARENT, thread_id=_THREAD),
      discord_watermark_id=_WATERMARK)
  loaded = models.SessionMetadata.model_validate_json(meta.model_dump_json())
  fields = metadata_slots.fields_of(loaded, "discord")
  assert fields.discord_origin == metadata_slots.fields_of(meta, "discord").discord_origin
  assert fields.discord_watermark_id == _WATERMARK


def test_session_metadata_without_discord_fields_parses() -> None:
  meta = models.SessionMetadata.model_validate_json(
      models.SessionMetadata(profile="manager", name="t").model_dump_json())
  fields = metadata_slots.fields_of(meta, "discord")
  assert fields.discord_origin is None
  assert fields.discord_watermark_id is None


@pytest.mark.asyncio
async def test_create_session_persists_discord_origin(tmp_path) -> None:
  _, session_mgr, _ = conftest.build_env(tmp_path)
  origin = DiscordOrigin(guild_id=_GUILD, parent_channel_id=_PARENT, thread_id=_THREAD)
  meta = await session_mgr.create_session(models.CreateSessionRequest(name="d", discord_origin=origin))
  reloaded = await session_mgr.read_metadata_fresh(meta.id)
  fields = metadata_slots.fields_of(reloaded, "discord")
  assert fields.discord_origin == origin
  assert fields.discord_watermark_id is None


def test_discord_reply_renders_as_system_row() -> None:
  agg = message_aggregator.MessageAggregator()
  deltas = list(
      agg.feed(
          {
              "type": DISCORD_REPLY,
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
