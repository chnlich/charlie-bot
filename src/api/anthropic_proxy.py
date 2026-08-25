"""Anthropic-compatible proxy routes for non-Anthropic model servers."""

import json
import os
import uuid
from collections.abc import AsyncIterator
from typing import Any

import httpx
import structlog
from fastapi import APIRouter, Depends, HTTPException, Request
from fastapi.responses import JSONResponse, StreamingResponse

from src.core.config import CharlieBotConfig, get_config
from src.core.http import get_http_client

log = structlog.get_logger()

router = APIRouter()


def _join_openai_chat_url(base_url: str) -> str:
  if not base_url:
    raise ValueError("api_base not set for backend")
  return f"{base_url.rstrip('/')}/chat/completions"


def _require_dict(value: Any, label: str) -> dict:
  if not isinstance(value, dict):
    raise ValueError(f"{label} must be an object")
  return value


def _blocks_to_text(content: Any, label: str) -> str:
  if isinstance(content, str):
    return content
  if not isinstance(content, list):
    raise ValueError(f"{label} must be a string or content block list")
  parts: list[str] = []
  for block in content:
    block = _require_dict(block, f"{label} block")
    block_type = block.get("type")
    if block_type != "text":
      raise ValueError(f"unsupported {label} block type: {block_type}")
    parts.append(str(block.get("text", "")))
  return "".join(parts)


def _tool_result_to_text(content: Any) -> str:
  return _blocks_to_text(content, "tool_result content")


def _flush_user_text(messages: list[dict], parts: list[str]) -> None:
  if not parts:
    return
  messages.append({"role": "user", "content": "".join(parts)})
  parts.clear()


def _convert_user_message(message: dict) -> list[dict]:
  content = message.get("content", "")
  if isinstance(content, str):
    return [{"role": "user", "content": content}]
  if not isinstance(content, list):
    raise ValueError("user message content must be a string or content block list")

  converted: list[dict] = []
  text_parts: list[str] = []
  for block in content:
    block = _require_dict(block, "user content block")
    block_type = block.get("type")
    if block_type == "text":
      text_parts.append(str(block.get("text", "")))
    elif block_type == "tool_result":
      _flush_user_text(converted, text_parts)
      tool_use_id = block.get("tool_use_id")
      if not tool_use_id:
        raise ValueError("tool_result block missing tool_use_id")
      converted.append(
          {
              "role": "tool",
              "tool_call_id": tool_use_id,
              "content": _tool_result_to_text(block.get("content", "")),
          })
    else:
      raise ValueError(f"unsupported user content block type: {block_type}")
  _flush_user_text(converted, text_parts)
  return converted


def _convert_assistant_message(message: dict) -> dict:
  content = message.get("content", "")
  if isinstance(content, str):
    return {"role": "assistant", "content": content}
  if not isinstance(content, list):
    raise ValueError("assistant message content must be a string or content block list")

  text_parts: list[str] = []
  tool_calls: list[dict] = []
  for block in content:
    block = _require_dict(block, "assistant content block")
    block_type = block.get("type")
    if block_type == "text":
      text_parts.append(str(block.get("text", "")))
    elif block_type == "tool_use":
      tool_id = block.get("id")
      name = block.get("name")
      if not tool_id or not name:
        raise ValueError("tool_use block missing id or name")
      tool_calls.append(
          {
              "id": tool_id,
              "type": "function",
              "function": {
                  "name": name,
                  "arguments": json.dumps(block.get("input", {}), separators=(",", ":")),
              },
          })
    else:
      raise ValueError(f"unsupported assistant content block type: {block_type}")

  converted: dict[str, Any] = {"role": "assistant", "content": "".join(text_parts) or None}
  if tool_calls:
    converted["tool_calls"] = tool_calls
  return converted


def _convert_messages(messages: list[dict], system: Any) -> list[dict]:
  converted: list[dict] = []
  if system:
    converted.append({"role": "system", "content": _blocks_to_text(system, "system")})

  for message in messages:
    message = _require_dict(message, "message")
    role = message.get("role")
    if role == "user":
      converted.extend(_convert_user_message(message))
    elif role == "assistant":
      converted.append(_convert_assistant_message(message))
    else:
      raise ValueError(f"unsupported message role: {role}")
  return converted


def _convert_tools(tools: list[dict]) -> list[dict]:
  converted: list[dict] = []
  for tool in tools:
    tool = _require_dict(tool, "tool")
    if tool.get("type"):
      raise ValueError(f"unsupported Anthropic built-in tool type: {tool.get('type')}")
    name = tool.get("name")
    input_schema = tool.get("input_schema")
    if not name or input_schema is None:
      raise ValueError("tool missing name or input_schema")
    converted.append(
        {
            "type": "function",
            "function": {
                "name": name,
                "description": tool.get("description", ""),
                "parameters": input_schema,
            },
        })
  return converted


def _convert_tool_choice(choice: dict) -> str | dict:
  choice_type = choice.get("type")
  if choice_type == "auto":
    return "auto"
  if choice_type == "any":
    return "required"
  if choice_type == "tool":
    name = choice.get("name")
    if not name:
      raise ValueError("tool_choice type 'tool' missing name")
    return {"type": "function", "function": {"name": name}}
  if choice_type == "none":
    return "none"
  raise ValueError(f"unsupported tool_choice type: {choice_type}")


def anthropic_to_openai_chat_request(payload: dict, upstream_model: str) -> dict:
  """Translate an Anthropic Messages payload into an OpenAI chat completions payload."""
  messages = payload.get("messages")
  if not isinstance(messages, list):
    raise ValueError("messages must be a list")

  converted: dict[str, Any] = {
      "model": upstream_model,
      "messages": _convert_messages(messages, payload.get("system")),
      "stream": bool(payload.get("stream", False)),
  }
  if payload.get("max_tokens") is not None:
    converted["max_tokens"] = payload["max_tokens"]
  if payload.get("temperature") is not None:
    converted["temperature"] = payload["temperature"]
  if payload.get("top_p") is not None:
    converted["top_p"] = payload["top_p"]
  if payload.get("stop_sequences") is not None:
    converted["stop"] = payload["stop_sequences"]
  if payload.get("tools") is not None:
    converted["tools"] = _convert_tools(payload["tools"])
  if payload.get("tool_choice") is not None:
    tool_choice = _require_dict(payload["tool_choice"], "tool_choice")
    converted["tool_choice"] = _convert_tool_choice(tool_choice)
    if tool_choice.get("disable_parallel_tool_use") is not None:
      converted["parallel_tool_calls"] = not bool(tool_choice["disable_parallel_tool_use"])
  if converted["stream"]:
    converted["stream_options"] = {"include_usage": True}
  # Do not pass Anthropic thinking through yet: the OpenAI-compatible upstream's
  # reasoning_content is not an Anthropic thinking block and does not provide
  # Anthropic signatures.
  if payload.get("thinking") is not None:
    raise ValueError("OpenAI-compatible proxy does not support Anthropic thinking blocks")
  if payload.get("top_k") is not None:
    raise ValueError("OpenAI-compatible proxy does not support Anthropic top_k")
  return converted


def _stop_reason(finish_reason: str | None) -> str:
  if finish_reason == "tool_calls":
    return "tool_use"
  if finish_reason == "length":
    return "max_tokens"
  if finish_reason in (None, "stop"):
    return "end_turn"
  return finish_reason


def _usage_from_openai(usage: dict | None) -> dict:
  usage = usage or {}
  return {
      "input_tokens": usage.get("prompt_tokens", 0),
      "output_tokens": usage.get("completion_tokens", 0),
      "cache_creation_input_tokens": 0,
      "cache_read_input_tokens": 0,
  }


def openai_chat_response_to_anthropic(response: dict, requested_model: str) -> dict:
  """Translate a non-streaming OpenAI chat completion response into an Anthropic message."""
  choices = response.get("choices")
  if not choices:
    raise ValueError("OpenAI response missing choices")
  choice = choices[0]
  message = choice.get("message") or {}
  content_blocks: list[dict] = []

  content = message.get("content")
  if content:
    content_blocks.append({"type": "text", "text": content})

  for tool_call in message.get("tool_calls") or []:
    function = tool_call.get("function") or {}
    tool_id = tool_call.get("id")
    name = function.get("name")
    if not tool_id or not name:
      raise ValueError("OpenAI tool_call missing id or function.name")
    arguments = function.get("arguments") or "{}"
    content_blocks.append({
        "type": "tool_use",
        "id": tool_id,
        "name": name,
        "input": json.loads(arguments),
    })

  return {
      "id": response.get("id") or f"msg_{uuid.uuid4().hex}",
      "type": "message",
      "role": "assistant",
      "model": response.get("model") or requested_model,
      "content": content_blocks,
      "stop_reason": _stop_reason(choice.get("finish_reason")),
      "stop_sequence": None,
      "usage": _usage_from_openai(response.get("usage")),
  }


def _sse_event(event: str, data: dict) -> bytes:
  return f"event: {event}\ndata: {json.dumps(data, separators=(',', ':'))}\n\n".encode("utf-8")


class OpenAIChatStreamToAnthropic:
  """Stateful translator from OpenAI chat stream chunks to Anthropic SSE events."""

  def __init__(self, model: str):
    self._message_id = f"msg_{uuid.uuid4().hex}"
    self._model = model
    self._next_index = 0
    self._text_index: int | None = None
    self._tool_indexes: dict[int, int] = {}
    self._tool_names: dict[int, str] = {}
    self._tool_ids: dict[int, str] = {}
    self._usage: dict = {}
    self._finish_reason: str | None = None

  def start_events(self) -> list[tuple[str, dict]]:
    return [
        (
            "message_start",
            {
                "type": "message_start",
                "message":
                    {
                        "id": self._message_id,
                        "type": "message",
                        "role": "assistant",
                        "content": [],
                        "model": self._model,
                        "stop_reason": None,
                        "stop_sequence": None,
                        "usage": _usage_from_openai(None),
                    },
            },
        )
    ]

  def events_for_chunk(self, chunk: dict) -> list[tuple[str, dict]]:
    events: list[tuple[str, dict]] = []
    if chunk.get("usage"):
      self._usage = chunk["usage"]

    for choice in chunk.get("choices") or []:
      delta = choice.get("delta") or {}
      if delta.get("content"):
        events.extend(self._ensure_text_block())
        events.append(
            (
                "content_block_delta",
                {
                    "type": "content_block_delta",
                    "index": self._text_index,
                    "delta": {
                        "type": "text_delta",
                        "text": delta["content"],
                    },
                },
            ))
      for tool_call in delta.get("tool_calls") or []:
        events.extend(self._handle_tool_delta(tool_call))
      if choice.get("finish_reason"):
        self._finish_reason = choice["finish_reason"]
    return events

  def finish_events(self) -> list[tuple[str, dict]]:
    events: list[tuple[str, dict]] = []
    events.extend(self._close_text_block())
    for call_index in list(self._tool_indexes):
      block_index = self._tool_indexes.pop(call_index)
      events.append(("content_block_stop", {"type": "content_block_stop", "index": block_index}))
    usage = _usage_from_openai(self._usage)
    events.append(
        (
            "message_delta",
            {
                "type": "message_delta",
                "delta": {
                    "stop_reason": _stop_reason(self._finish_reason),
                    "stop_sequence": None,
                },
                "usage": usage,
            },
        ))
    events.append(("message_stop", {"type": "message_stop"}))
    return events

  def _ensure_text_block(self) -> list[tuple[str, dict]]:
    if self._text_index is not None:
      return []
    events = self._close_tool_blocks()
    self._text_index = self._next_index
    self._next_index += 1
    events.append(
        (
            "content_block_start",
            {
                "type": "content_block_start",
                "index": self._text_index,
                "content_block": {
                    "type": "text",
                    "text": "",
                },
            },
        ))
    return events

  def _close_text_block(self) -> list[tuple[str, dict]]:
    if self._text_index is None:
      return []
    index = self._text_index
    self._text_index = None
    return [("content_block_stop", {"type": "content_block_stop", "index": index})]

  def _close_tool_blocks(self) -> list[tuple[str, dict]]:
    events: list[tuple[str, dict]] = []
    for call_index in list(self._tool_indexes):
      block_index = self._tool_indexes.pop(call_index)
      events.append(("content_block_stop", {"type": "content_block_stop", "index": block_index}))
    return events

  def _handle_tool_delta(self, tool_call: dict) -> list[tuple[str, dict]]:
    call_index = tool_call.get("index")
    if call_index is None:
      raise ValueError("streaming tool_call missing index")
    function = tool_call.get("function") or {}
    events: list[tuple[str, dict]] = []

    if call_index not in self._tool_indexes:
      tool_id = tool_call.get("id")
      name = function.get("name")
      if not tool_id or not name:
        raise ValueError("first streaming tool_call delta missing id or function.name")
      events.extend(self._close_text_block())
      block_index = self._next_index
      self._next_index += 1
      self._tool_indexes[call_index] = block_index
      self._tool_ids[call_index] = tool_id
      self._tool_names[call_index] = name
      events.append(
          (
              "content_block_start",
              {
                  "type": "content_block_start",
                  "index": block_index,
                  "content_block": {
                      "type": "tool_use",
                      "id": tool_id,
                      "name": name,
                      "input": {},
                  },
              },
          ))

    arguments = function.get("arguments")
    if arguments is not None:
      events.append(
          (
              "content_block_delta",
              {
                  "type": "content_block_delta",
                  "index": self._tool_indexes[call_index],
                  "delta": {
                      "type": "input_json_delta",
                      "partial_json": arguments,
                  },
              },
          ))
    return events


async def _iter_anthropic_sse(upstream: httpx.Response, model: str) -> AsyncIterator[bytes]:
  translator = OpenAIChatStreamToAnthropic(model)
  try:
    for event, data in translator.start_events():
      yield _sse_event(event, data)

    async for line in upstream.aiter_lines():
      if not line.startswith("data:"):
        continue
      raw = line[len("data:"):].strip()
      if raw == "[DONE]":
        break
      if not raw:
        continue
      chunk = json.loads(raw)
      for event, data in translator.events_for_chunk(chunk):
        yield _sse_event(event, data)

    for event, data in translator.finish_events():
      yield _sse_event(event, data)
  finally:
    await upstream.aclose()


def _upstream_headers(api_key_env: str | None, backend_id: str) -> dict[str, str]:
  headers = {"Content-Type": "application/json"}
  if not api_key_env:
    return headers
  token = os.environ.get(api_key_env)
  if not token:
    raise ValueError(f"api_key_env '{api_key_env}' is not set in the environment for backend '{backend_id}'")
  headers["Authorization"] = f"Bearer {token}"
  return headers


async def _upstream_error(response: httpx.Response) -> HTTPException:
  body = (await response.aread()).decode("utf-8", errors="replace")
  await response.aclose()
  return HTTPException(status_code=response.status_code, detail=body)


@router.post("/openai-compatible/{backend_id}/v1/messages")
async def openai_compatible_messages(
    backend_id: str,
    request: Request,
    cfg: CharlieBotConfig = Depends(get_config),
):
  """Serve Anthropic Messages API requests through a per-backend OpenAI-compatible endpoint."""
  option = cfg.get_backend_option(backend_id)
  if option is None:
    raise HTTPException(status_code=404, detail=f"unknown backend id: {backend_id}")
  if option.type != "cc-openai-compatible":
    raise HTTPException(
        status_code=400, detail=f"backend '{backend_id}' is not type 'cc-openai-compatible' (got '{option.type}')")
  if not option.api_base:
    raise HTTPException(status_code=400, detail=f"backend '{backend_id}' missing api_base")
  if not option.model:
    raise HTTPException(status_code=400, detail=f"backend '{backend_id}' missing model")

  try:
    anthropic_payload = await request.json()
    openai_payload = anthropic_to_openai_chat_request(anthropic_payload, upstream_model=option.model)
    upstream_url = _join_openai_chat_url(option.api_base)
    headers = _upstream_headers(option.api_key_env, backend_id)
  except ValueError as e:
    raise HTTPException(status_code=400, detail=str(e)) from e

  client = get_http_client()
  model = openai_payload["model"]

  if openai_payload["stream"]:
    upstream_request = client.build_request(
        "POST",
        upstream_url,
        json=openai_payload,
        headers=headers,
        timeout=None,
    )
    upstream = await client.send(upstream_request, stream=True)
    if upstream.status_code >= 400:
      raise await _upstream_error(upstream)
    return StreamingResponse(_iter_anthropic_sse(upstream, model), media_type="text/event-stream")

  upstream = await client.post(upstream_url, json=openai_payload, headers=headers, timeout=None)
  if upstream.status_code >= 400:
    raise await _upstream_error(upstream)
  try:
    data = openai_chat_response_to_anthropic(upstream.json(), model)
  except ValueError as e:
    raise HTTPException(status_code=502, detail=str(e)) from e
  return JSONResponse(data)
