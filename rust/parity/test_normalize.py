"""Header normalization tests: a fake's recording compares only the headers its feature code sets explicitly."""

import pytest

from rust.parity import normalize


def _context() -> normalize.Context:
  return normalize.Context(tree_root="/tmp/charliebot-parity-synthetic/base")


def test_library_default_headers_are_left_out_of_slack_requests() -> None:
  headers = {
      "authorization": ["Bearer xoxb-parity-synthetic-token"],
      "content-type": ["application/json"],
      "host": ["slack.com"],
      "user-agent": ["python-httpx/0.28.1"],
  }
  compared = normalize.request_headers("slack", headers, _context(), websocket=False)
  assert compared == {"authorization": ["Bearer xoxb-parity-synthetic-token"]}


def test_discord_user_agent_is_compared_on_rest_only() -> None:
  headers = {"user-agent": ["DiscordBot (https://discord.com, charlie-bot)"]}
  assert normalize.request_headers("discord", headers, _context(), websocket=False) == headers
  assert normalize.request_headers("discord", headers, _context(), websocket=True) == {}


def test_unnamed_request_header_fails_fast() -> None:
  with pytest.raises(ValueError, match="'x-unexpected' is neither set explicitly"):
    normalize.request_headers("gemini", {"x-unexpected": ["1"]}, _context(), websocket=True)
