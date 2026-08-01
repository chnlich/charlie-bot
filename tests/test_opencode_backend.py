import json
from pathlib import Path

import httpx
import pytest

from src.agents.backends.opencode import OpenCodeBackend
from src.core import event_types as ET
from src.core.streaming import handle_compaction_events


def _build_backend(monkeypatch, **kwargs) -> OpenCodeBackend:
  monkeypatch.setattr(
      "src.agents.backends.opencode.resolve_binary",
      lambda name, fallback: "/usr/bin/opencode",
  )
  return OpenCodeBackend(**kwargs)


class _FakeEventStream:
  """Async-iterable of pre-baked SSE event dicts for _consume_sse_events tests."""

  def __init__(self, events: list[dict]) -> None:
    self._events = events

  async def __aiter__(self):
    for event in self._events:
      yield event


class _FakeSseResponse:

  def __init__(self, lines: list[str]) -> None:
    self._lines = lines

  async def aiter_lines(self):
    for line in self._lines:
      yield line


@pytest.mark.asyncio
async def test_iter_sse_events_ignores_comments_and_metadata(monkeypatch) -> None:
  backend = _build_backend(monkeypatch)
  response = _FakeSseResponse(
      [
          ": keepalive",
          "event: message.updated",
          "id: event-1",
          "retry: 1000",
          "data: {\"type\": \"server.connected\"}",
          "",
          ": another keepalive",
          "event: session.idle",
          "x-extra: ignored",
          "data: {\"type\": \"session.idle\",",
          "data: \"properties\": {\"sessionID\": \"session-1\"}}",
          "",
      ])

  events = [event async for event in backend._iter_sse_events(response)]

  assert events == [
      {
          "type": "server.connected"
      },
      {
          "type": "session.idle",
          "properties": {
              "sessionID": "session-1"
          }
      },
  ]


def test_translate_sse_event_buffers_part_until_message_role_known(monkeypatch) -> None:
  backend = _build_backend(monkeypatch)

  assert backend._translate_sse_event(
      {
          "type": "message.part.updated",
          "properties": {
              "part": {
                  "messageID": "message-1",
                  "id": "part-1",
                  "type": "text",
                  "text": "Hello",
              }
          },
      }) == []
  assert backend._translate_sse_event(
      {
          "type": "message.part.updated",
          "properties": {
              "part": {
                  "messageID": "message-1",
                  "id": "part-1",
                  "type": "text",
                  "text": "Hello world",
              }
          },
      }) == []

  translated = backend._translate_sse_event(
      {
          "type": "message.updated",
          "properties": {
              "info": {
                  "id": "message-1",
                  "role": "assistant",
              }
          },
      })

  assert translated == [
      {
          "type": "assistant",
          "message": {
              "content": [{
                  "type": "text",
                  "text": "Hello"
              }]
          }
      },
      {
          "type": "assistant",
          "message": {
              "content": [{
                  "type": "text",
                  "text": " world"
              }]
          }
      },
  ]


def test_translate_sse_event_discards_buffered_non_assistant_parts(monkeypatch) -> None:
  backend = _build_backend(monkeypatch)

  assert backend._translate_sse_event(
      {
          "type": "message.part.updated",
          "properties": {
              "part": {
                  "messageID": "message-1",
                  "id": "part-1",
                  "type": "text",
                  "text": "Hello",
              }
          },
      }) == []

  translated = backend._translate_sse_event(
      {
          "type": "message.updated",
          "properties": {
              "info": {
                  "id": "message-1",
                  "role": "user",
              }
          },
      })

  assert translated == []
  assert backend._pending_parts == {}


def test_prepare_env_sets_charliebot_opencode_config(monkeypatch) -> None:
  backend = _build_backend(monkeypatch)

  env = backend._prepare_env({"PATH": "/usr/bin"})

  data = json.loads(env["OPENCODE_CONFIG_CONTENT"])
  assert data["permission"] == {"*": "allow", "question": "deny"}
  assert data["default_agent"] == "charliebot"
  assert data["agent"]["charliebot"] == {"mode": "primary"}


def test_prepare_cwd_writes_agents_md_when_instructions_provided(monkeypatch, tmp_path: Path) -> None:
  backend = _build_backend(monkeypatch, instructions_content="# Test Instructions\nDo things.")

  backend._prepare_cwd(str(tmp_path))

  agents_md = tmp_path / "AGENTS.md"
  assert agents_md.exists()
  assert agents_md.read_text(encoding="utf-8") == "# Test Instructions\nDo things."


def test_prepare_cwd_writes_agents_md_even_when_config_exists(monkeypatch, tmp_path: Path) -> None:
  """AGENTS.md must be written even when opencode.json already exists (resumed sessions)."""
  backend = _build_backend(monkeypatch, instructions_content="# Instructions")
  config_dir = tmp_path / ".opencode"
  config_dir.mkdir()
  (config_dir / "opencode.json").write_text("{}", encoding="utf-8")

  backend._prepare_cwd(str(tmp_path))

  agents_md = tmp_path / "AGENTS.md"
  assert agents_md.exists()
  assert agents_md.read_text(encoding="utf-8") == "# Instructions"


def test_prepare_cwd_skips_agents_md_when_no_instructions(monkeypatch, tmp_path: Path) -> None:
  backend = _build_backend(monkeypatch)

  backend._prepare_cwd(str(tmp_path))

  agents_md = tmp_path / "AGENTS.md"
  assert not agents_md.exists()


def test_translate_tool_error_emits_tool_result(monkeypatch) -> None:
  backend = _build_backend(monkeypatch)

  translated = backend.translate_event({
      "type": "tool",
      "part": {
          "callID": "call-1",
          "tool": "glob",
          "state": {
              "input": {"pattern": "AGENTS.md", "path": "/tmp"},
              "error": "The user rejected permission to use this specific tool call.",
          },
      },
  })

  assert translated == [
      {
          "type": "assistant",
          "message": {
              "content": [{
                  "type": "tool_use",
                  "name": "glob",
                  "id": "call-1",
                  "input": {"pattern": "AGENTS.md", "path": "/tmp"},
              }]
          },
      },
      {
          "type": "tool_result",
          "tool_use_id": "call-1",
          "content": "The user rejected permission to use this specific tool call.",
      },
  ]


async def _drain(events):
  return [event async for event in events]


@pytest.mark.asyncio
async def test_consume_sse_events_parent_permission_ask_fails_fast(monkeypatch) -> None:
  """A permission ask for the parent session is a terminal error that ends the turn."""
  backend = _build_backend(monkeypatch)
  backend._session_id = "parent-session"

  events = await _drain(
      backend._consume_sse_events(
          _FakeEventStream([
              {
                  "type": "permission.asked",
                  "properties": {
                      "id": "perm-1",
                      "sessionID": "parent-session",
                      "permission": "external_directory",
                      "patterns": ["/etc"],
                  },
              },
              {
                  "type": "session.idle",
                  "properties": {"sessionID": "parent-session"},
              },
          ])))

  assert len(events) == 1
  assert events[0]["type"] == ET.ERROR
  assert "external_directory" in events[0]["message"]
  assert "/etc" in events[0]["message"]
  assert "parent-session" in events[0]["message"]
  assert backend._failed is True


@pytest.mark.asyncio
async def test_consume_sse_events_child_permission_ask_fails_before_filtering(monkeypatch) -> None:
  """A child-session permission ask must be caught before the parent-session filter drops it."""
  backend = _build_backend(monkeypatch)
  backend._session_id = "parent-session"

  events = await _drain(
      backend._consume_sse_events(
          _FakeEventStream([{
              "type": "permission.asked",
              "properties": {
                  "id": "perm-2",
                  "sessionID": "child-session",
                  "permission": "doom_loop",
                  "patterns": ["task"],
              },
          }])))

  assert len(events) == 1
  assert events[0]["type"] == ET.ERROR
  assert "doom_loop" in events[0]["message"]
  assert "child-session" in events[0]["message"]
  assert backend._failed is True


@pytest.mark.asyncio
async def test_consume_sse_events_normal_parent_turn(monkeypatch) -> None:
  """Assistant text, parent-session filtering, idle, and usage aggregation still work."""
  backend = _build_backend(monkeypatch)
  backend._session_id = "parent-session"

  events = await _drain(
      backend._consume_sse_events(
          _FakeEventStream([
              {  # dropped: belongs to an unrelated session
                  "type": "message.part.updated",
                  "properties": {"sessionID": "other", "part": {"messageID": "m9", "id": "p9", "type": "text",
                                                                "text": "ignored"}},
              },
              {
                  "type": "message.updated",
                  "properties": {"sessionID": "parent-session", "info": {"id": "m1", "role": "assistant"}},
              },
              {
                  "type": "message.part.updated",
                  "properties": {"sessionID": "parent-session",
                                 "part": {"messageID": "m1", "id": "p1", "type": "text", "text": "Hello"}},
              },
              {
                  "type": "session.idle",
                  "properties": {"sessionID": "parent-session"},
              },
          ])))

  assert events == [
      {"type": "assistant", "message": {"content": [{"type": "text", "text": "Hello"}]}},
      backend._make_accumulated_result(),
  ]
  assert backend._failed is False


def test_is_cancellation_disconnect_true_after_terminate(monkeypatch) -> None:
  backend = _build_backend(monkeypatch)
  backend.terminated = True

  assert backend._is_cancellation_disconnect(httpx.RemoteProtocolError("incomplete chunked read")) is True


def test_is_cancellation_disconnect_false_without_terminate(monkeypatch) -> None:
  """An unexpected disconnect with no deliberate terminate stays a visible backend failure."""
  backend = _build_backend(monkeypatch)

  assert backend._is_cancellation_disconnect(httpx.RemoteProtocolError("incomplete chunked read")) is False


def test_translate_sse_event_reasoning_part_emits_thinking_delta(monkeypatch) -> None:
  backend = _build_backend(monkeypatch)

  assert backend._translate_sse_event({
      "type": "message.part.updated",
      "properties": {
          "part": {
              "messageID": "message-1",
              "id": "part-r",
              "type": "reasoning",
              "text": "I need",
          }
      },
  }) == []
  assert backend._translate_sse_event({
      "type": "message.part.updated",
      "properties": {
          "part": {
              "messageID": "message-1",
              "id": "part-r",
              "type": "reasoning",
              "text": "I need to think",
          }
      },
  }) == []

  translated = backend._translate_sse_event({
      "type": "message.updated",
      "properties": {
          "info": {
              "id": "message-1",
              "role": "assistant",
          }
      },
  })

  assert translated == [
      {
          "type": ET.THINKING,
          "content": "I need"
      },
      {
          "type": ET.THINKING,
          "content": " to think"
      },
  ]


# ---------------------------------------------------------------------------
# context_snapshot: last step's tokens, reasoning, model limit, catalog failure
# ---------------------------------------------------------------------------


def _step_finish_part(input_t, output_t, reasoning_t, cache_read_t, cache_write_t, cost):
  return {
      "messageID": "m1",
      "id": "p1",
      "type": "step-finish",
      "tokens": {
          "input": input_t,
          "output": output_t,
          "reasoning": reasoning_t,
          "cache": {"read": cache_read_t, "write": cache_write_t},
      },
      "cost": cost,
  }


def test_make_accumulated_result_snapshot_carries_last_step_tokens_not_sum(monkeypatch) -> None:
  backend = _build_backend(monkeypatch, model="meshy-sglang-glm52/nvidia/GLM-5.2-NVFP4")
  backend._model_limit = {"context": 409600, "input": 270000, "output": 131072}
  backend._accumulate_step_finish(
      _step_finish_part(100, 10, 5, 20, 30, 0.1))
  backend._accumulate_step_finish(
      _step_finish_part(200, 20, 8, 40, 60, 0.2))

  result = backend._make_accumulated_result()

  assert result["type"] == ET.RESULT
  snapshot = result["context_snapshot"]
  # The snapshot carries the *last* step's tokens, not the turn's sum.
  assert snapshot["tokens"] == {
      "input": 200, "output": 20, "reasoning": 8, "cache_read": 40, "cache_write": 60}
  # The usage block still carries the turn's accumulated sum.
  assert result["usage"]["input_tokens"] == 300
  assert result["usage"]["output_tokens"] == 30
  assert result["usage"]["cache_read_input_tokens"] == 60
  assert result["usage"]["cache_creation_input_tokens"] == 90
  # reasoning is preserved in the snapshot.
  assert snapshot["tokens"]["reasoning"] == 8
  # cost still accumulates.
  assert result["total_cost_usd"] == pytest.approx(0.3)
  assert snapshot["model"] == "meshy-sglang-glm52/nvidia/GLM-5.2-NVFP4"
  assert snapshot["limit"] == {"context": 409600, "input": 270000, "output": 131072}


def test_make_accumulated_result_omits_snapshot_when_no_step_finish(monkeypatch) -> None:
  backend = _build_backend(monkeypatch, model="meshy-sglang-glm52/nvidia/GLM-5.2-NVFP4")
  backend._model_limit = {"context": 409600, "input": 270000, "output": 131072}

  result = backend._make_accumulated_result()

  assert "context_snapshot" not in result


def test_make_accumulated_result_snapshot_limit_none_when_catalog_unavailable(monkeypatch) -> None:
  backend = _build_backend(monkeypatch, model="meshy-sglang-glm52/nvidia/GLM-5.2-NVFP4")
  backend._model_limit = None  # catalog unavailable -> limit stays None
  backend._accumulate_step_finish(_step_finish_part(100, 10, 0, 0, 0, 0.1))

  result = backend._make_accumulated_result()

  snapshot = result["context_snapshot"]
  assert snapshot["limit"] is None
  assert snapshot["tokens"] == {
      "input": 100, "output": 10, "reasoning": 0, "cache_read": 0, "cache_write": 0}


def test_reset_run_state_clears_model_limit_and_last_step_tokens(monkeypatch) -> None:
  backend = _build_backend(monkeypatch, model="meshy-sglang-glm52/nvidia/GLM-5.2-NVFP4")
  backend._model_limit = {"context": 409600, "input": 270000, "output": 131072}
  backend._accumulate_step_finish(_step_finish_part(100, 10, 0, 0, 0, 0.1))
  assert backend._last_step_tokens is not None

  backend._reset_run_state()

  assert backend._model_limit is None
  assert backend._last_step_tokens is None


class _FakeConfigResponse:

  def __init__(self, payload=None, exc: Exception | None = None, status: int = 200) -> None:
    self._payload = payload
    self._exc = exc
    self._status = status

  def raise_for_status(self) -> None:
    if self._status >= 400:
      raise httpx.HTTPStatusError("bad status", request=None, response=self)

  def json(self):
    return self._payload


class _FakeConfigClient:
  """Minimal httpx-like client for ``_fetch_model_limit`` tests."""

  def __init__(self, response: _FakeConfigResponse | None = None,
               exc: Exception | None = None) -> None:
    self._response = response
    self._exc = exc

  async def get(self, url: str) -> _FakeConfigResponse:
    assert url == "/config/providers"
    if self._exc is not None:
      raise self._exc
    return self._response


@pytest.mark.asyncio
async def test_fetch_model_limit_returns_limit_from_recorded_providers(monkeypatch) -> None:
  backend = _build_backend(monkeypatch, model="meshy-sglang-glm52/nvidia/GLM-5.2-NVFP4")
  # Recorded /config/providers payload: top-level {"providers": [...]}, each
  # provider's models is a dict keyed by model id.
  providers = {
      "providers": [
          {"id": "other-provider", "models": {"x": {"id": "x", "limit": {"context": 100}}}},
          {
              "id": "meshy-sglang-glm52",
              "models": {
                  "nvidia/GLM-5.2-NVFP4": {
                      "id": "nvidia/GLM-5.2-NVFP4",
                      "limit": {"context": 409600, "input": 270000, "output": 131072},
                  },
              },
          },
      ],
      "default": "meshy-sglang-glm52",
  }
  client = _FakeConfigClient(response=_FakeConfigResponse(payload=providers))

  limit = await backend._fetch_model_limit(client)

  assert limit == {"context": 409600, "input": 270000, "output": 131072}


@pytest.mark.asyncio
async def test_fetch_model_limit_accepts_bare_list_and_list_models(monkeypatch) -> None:
  backend = _build_backend(monkeypatch, model="prov/model-id")
  # Alternative shapes: bare top-level list and list-shaped models.
  providers = [
      {"id": "prov", "models": [{"id": "model-id", "limit": {"context": 8, "output": 4}}]},
  ]
  client = _FakeConfigClient(response=_FakeConfigResponse(payload=providers))

  limit = await backend._fetch_model_limit(client)

  assert limit == {"context": 8, "output": 4}


@pytest.mark.asyncio
async def test_fetch_model_limit_returns_none_and_warns_on_request_error(monkeypatch, capsys) -> None:
  backend = _build_backend(monkeypatch, model="meshy-sglang-glm52/nvidia/GLM-5.2-NVFP4")
  client = _FakeConfigClient(exc=httpx.ConnectError("connection refused"))

  limit = await backend._fetch_model_limit(client)

  assert limit is None
  out = capsys.readouterr().out
  assert "opencode_context_catalog_unavailable" in out
  assert "meshy-sglang-glm52" in out


@pytest.mark.asyncio
async def test_fetch_model_limit_returns_none_and_warns_when_provider_absent(monkeypatch, capsys) -> None:
  backend = _build_backend(monkeypatch, model="meshy-sglang-glm52/nvidia/GLM-5.2-NVFP4")
  providers = {"providers": [{"id": "other-provider", "models": {}}]}
  client = _FakeConfigClient(response=_FakeConfigResponse(payload=providers))

  limit = await backend._fetch_model_limit(client)

  assert limit is None
  out = capsys.readouterr().out
  assert "opencode_context_catalog_unavailable" in out


@pytest.mark.asyncio
async def test_fetch_model_limit_returns_none_and_warns_when_model_absent(monkeypatch, capsys) -> None:
  backend = _build_backend(monkeypatch, model="meshy-sglang-glm52/nvidia/GLM-5.2-NVFP4")
  providers = {"providers": [{"id": "meshy-sglang-glm52", "models": {"other-model": {"id": "other-model"}}}]}
  client = _FakeConfigClient(response=_FakeConfigResponse(payload=providers))

  limit = await backend._fetch_model_limit(client)

  assert limit is None
  out = capsys.readouterr().out
  assert "opencode_context_catalog_unavailable" in out


@pytest.mark.asyncio
async def test_fetch_model_limit_returns_none_and_warns_on_malformed_payload(monkeypatch, capsys) -> None:
  backend = _build_backend(monkeypatch, model="meshy-sglang-glm52/nvidia/GLM-5.2-NVFP4")
  # Not a recognised shape (no "providers" key, not a list).
  client = _FakeConfigClient(response=_FakeConfigResponse(payload={"random": "shape"}))

  limit = await backend._fetch_model_limit(client)

  assert limit is None
  out = capsys.readouterr().out
  assert "opencode_context_catalog_unavailable" in out


@pytest.mark.asyncio
async def test_fetch_model_limit_returns_none_when_model_unset(monkeypatch) -> None:
  backend = _build_backend(monkeypatch, model=None)
  client = _FakeConfigClient(response=_FakeConfigResponse(payload=[]))

  limit = await backend._fetch_model_limit(client)

  assert limit is None


@pytest.mark.asyncio
async def test_consume_sse_events_emits_snapshot_with_last_step_tokens(monkeypatch) -> None:
  """A recorded SSE step-finish sequence yields a result event whose
  context_snapshot carries the last step's tokens while the usage block carries
  the turn's accumulated sum; the catalog failure (limit None) does not fail the
  turn."""
  backend = _build_backend(monkeypatch, model="meshy-sglang-glm52/nvidia/GLM-5.2-NVFP4")
  backend._session_id = "parent-session"
  backend._model_limit = None  # catalog unavailable

  events = await _drain(
      backend._consume_sse_events(
          _FakeEventStream([
              {"type": "message.updated",
               "properties": {"sessionID": "parent-session", "info": {"id": "m1", "role": "assistant"}}},
              {"type": "message.part.updated",
               "properties": {"sessionID": "parent-session", "part": _step_finish_part(100, 10, 5, 20, 30, 0.1)}},
              {"type": "message.part.updated",
               "properties": {"sessionID": "parent-session", "part": _step_finish_part(200, 20, 8, 40, 60, 0.2)}},
              {"type": "session.idle", "properties": {"sessionID": "parent-session"}},
          ])))

  assert backend._failed is False
  result = events[-1]
  assert result["type"] == ET.RESULT
  snapshot = result["context_snapshot"]
  # Last step's tokens, not the turn's sum.
  assert snapshot["tokens"] == {
      "input": 200, "output": 20, "reasoning": 8, "cache_read": 40, "cache_write": 60}
  # The usage block carries the turn's accumulated sum.
  assert result["usage"]["input_tokens"] == 300
  assert result["usage"]["output_tokens"] == 30
  assert result["usage"]["cache_read_input_tokens"] == 60
  assert result["usage"]["cache_creation_input_tokens"] == 90
  # reasoning preserved.
  assert snapshot["tokens"]["reasoning"] == 8
  # Catalog unavailable -> limit None, but the turn still completed.
  assert snapshot["limit"] is None
  assert snapshot["model"] == "meshy-sglang-glm52/nvidia/GLM-5.2-NVFP4"


# ---------------------------------------------------------------------------
# Compaction summary suppression: opencode auto-compaction publishes an
# assistant message (summary=true, agent="compaction") whose text/reasoning
# must never reach the chat stream; instead exactly one compact_boundary
# event is synthesized, and the message's own step-finish usage is kept.
# ---------------------------------------------------------------------------

_COMPACTION_TOKENS = {
    "total": 142466,
    "input": 140853,
    "output": 1613,
    "reasoning": 0,
    "cache": {"write": 0, "read": 0},
}
_COMPACTION_COST = 0.42


def _compaction_message_info(message_id: str, *, completed: bool) -> dict:
  """Recorded compaction message shape: first delivery is all-zero tokens with no
  time.completed; the later delivery carries the real tokens once the step finishes."""
  tokens = _COMPACTION_TOKENS if completed else {
      "total": 0, "input": 0, "output": 0, "reasoning": 0, "cache": {"write": 0, "read": 0}}
  time = {"created": 1, "completed": 2} if completed else {"created": 1}
  return {
      "id": message_id,
      "role": "assistant",
      "mode": "compaction",
      "agent": "compaction",
      "summary": True,
      "tokens": tokens,
      "time": time,
  }


def _compaction_parts(message_id: str) -> list[dict]:
  """Recorded part sequence for a compaction message: step-start, reasoning, text,
  step-finish (step-finish carries the compaction call's own tokens/cost)."""
  return [
      {"messageID": message_id, "id": f"{message_id}-step-start", "type": "step-start"},
      {"messageID": message_id, "id": f"{message_id}-reasoning", "type": "reasoning",
       "text": "[placeholder compaction reasoning]"},
      {"messageID": message_id, "id": f"{message_id}-text", "type": "text",
       "text": "[placeholder compaction summary]"},
      {
          "messageID": message_id,
          "id": f"{message_id}-step-finish",
          "type": "step-finish",
          "tokens": _COMPACTION_TOKENS,
          "cost": _COMPACTION_COST,
      },
  ]


def test_compaction_message_full_sequence_emits_no_chat_content(monkeypatch) -> None:
  """message.updated (created) -> parts -> message.updated (completed): zero text/thinking."""
  backend = _build_backend(monkeypatch)
  message_id = "msg_compaction_full"

  translated: list[dict] = []
  translated += backend._translate_sse_event({
      "type": "message.updated",
      "properties": {"info": _compaction_message_info(message_id, completed=False)},
  })
  for part in _compaction_parts(message_id):
    translated += backend._translate_sse_event({
        "type": "message.part.updated",
        "properties": {"part": part},
    })
  translated += backend._translate_sse_event({
      "type": "message.updated",
      "properties": {"info": _compaction_message_info(message_id, completed=True)},
  })

  assert not any(event["type"] == ET.ASSISTANT for event in translated)
  assert not any(event["type"] == ET.THINKING for event in translated)


def test_compaction_message_adversarial_buffered_order_emits_no_chat_content(monkeypatch) -> None:
  """Adversarial order (parts buffered before message.updated arrives): still zero leak."""
  backend = _build_backend(monkeypatch)
  message_id = "msg_compaction_adversarial"

  translated: list[dict] = []
  for part in _compaction_parts(message_id):
    translated += backend._translate_sse_event({
        "type": "message.part.updated",
        "properties": {"part": part},
    })
  translated += backend._translate_sse_event({
      "type": "message.updated",
      "properties": {"info": _compaction_message_info(message_id, completed=True)},
  })

  assert not any(event["type"] == ET.ASSISTANT for event in translated)
  assert not any(event["type"] == ET.THINKING for event in translated)
  assert backend._pending_parts == {}


def test_compaction_step_finish_usage_is_conserved(monkeypatch) -> None:
  """Accumulated usage after one normal message and one compaction message, each with a
  step-finish part, equals the sum over every step-finish part fed."""
  backend = _build_backend(monkeypatch)
  normal_step_finish = _step_finish_part(500, 50, 10, 5, 7, 0.03)
  message_id = "msg_compaction_usage"

  backend._translate_sse_event({
      "type": "message.updated",
      "properties": {"info": {"id": normal_step_finish["messageID"], "role": "assistant"}},
  })
  backend._translate_sse_event({
      "type": "message.part.updated",
      "properties": {"part": normal_step_finish},
  })
  backend._translate_sse_event({
      "type": "message.updated",
      "properties": {"info": _compaction_message_info(message_id, completed=False)},
  })
  for part in _compaction_parts(message_id):
    backend._translate_sse_event({
        "type": "message.part.updated",
        "properties": {"part": part},
    })

  assert backend._usage_input == normal_step_finish["tokens"]["input"] + _COMPACTION_TOKENS["input"]
  assert backend._usage_output == normal_step_finish["tokens"]["output"] + _COMPACTION_TOKENS["output"]
  assert backend._usage_cache_read == (
      normal_step_finish["tokens"]["cache"]["read"] + _COMPACTION_TOKENS["cache"]["read"])
  assert backend._usage_cache_write == (
      normal_step_finish["tokens"]["cache"]["write"] + _COMPACTION_TOKENS["cache"]["write"])
  assert backend._usage_cost == pytest.approx(normal_step_finish["cost"] + _COMPACTION_COST)


def test_compaction_boundary_emitted_exactly_once_per_message(monkeypatch) -> None:
  """Two summary messages, each message.updated delivered twice, yield exactly two
  compact_boundary events; pre_tokens reflects the _last_step_tokens snapshot in effect
  at registration (None before any step has completed)."""
  backend = _build_backend(monkeypatch)

  events: list[dict] = []
  events += backend._translate_sse_event({
      "type": "message.updated",
      "properties": {"info": _compaction_message_info("msg_c1", completed=False)},
  })
  events += backend._translate_sse_event({
      "type": "message.updated",
      "properties": {"info": _compaction_message_info("msg_c1", completed=True)},
  })

  backend._accumulate_step_finish(_step_finish_part(999, 88, 7, 3, 4, 0.02))

  events += backend._translate_sse_event({
      "type": "message.updated",
      "properties": {"info": _compaction_message_info("msg_c2", completed=False)},
  })
  events += backend._translate_sse_event({
      "type": "message.updated",
      "properties": {"info": _compaction_message_info("msg_c2", completed=True)},
  })

  boundary_events = [e for e in events if e.get("type") == "system" and e.get("subtype") == "compact_boundary"]
  assert len(boundary_events) == 2
  assert boundary_events[0]["compact_metadata"] == {"trigger": "auto", "pre_tokens": None}
  assert boundary_events[1]["compact_metadata"] == {
      "trigger": "auto", "pre_tokens": backend._last_step_tokens["input"]}


@pytest.mark.asyncio
async def test_compaction_boundary_event_wires_into_handle_compaction_events(monkeypatch) -> None:
  """The synthesized compact_boundary event feeds handle_compaction_events and yields
  exactly one persisted ET.CONTEXT_COMPACTED event carrying the same trigger/pre_tokens."""
  backend = _build_backend(monkeypatch)

  events = backend._translate_sse_event({
      "type": "message.updated",
      "properties": {"info": _compaction_message_info("msg_wire", completed=False)},
  })
  assert len(events) == 1
  boundary_event = events[0]

  persisted: list[dict] = []

  async def _record(event: dict) -> None:
    persisted.append(event)

  await handle_compaction_events(boundary_event, _record, {"thread_id": "t1"})

  assert len(persisted) == 1
  assert persisted[0]["type"] == ET.CONTEXT_COMPACTED
  assert persisted[0]["trigger"] == boundary_event["compact_metadata"]["trigger"]
  assert persisted[0]["pre_tokens"] == boundary_event["compact_metadata"]["pre_tokens"]
