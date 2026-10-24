"""Anthropic-compatible proxy routes for non-Anthropic model servers."""

import json
import uuid
from collections.abc import AsyncIterator
from typing import TYPE_CHECKING, Any
from urllib import parse

import fastapi
import orjson
from fastapi import responses

from src.infra import config, http, sse
from src.infra import event_types as ET
from src.runtime.api import deps

if TYPE_CHECKING:
  import httpx

router = fastapi.APIRouter()


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
  for raw_block in content:
    block = _require_dict(raw_block, f"{label} block")
    block_type = block.get("type")
    if block_type != "text":
      raise ValueError(f"unsupported {label} block type: {block_type}")
    parts.append(str(block.get("text", "")))
  return "".join(parts)


def _image_block_to_openai_part(block: dict, label: str) -> dict:
  source = _require_dict(block.get("source"), f"{label} source")
  if source.get("type") != "base64":
    raise ValueError(f"unsupported {label} source type: {source.get('type')}")
  media_type = source.get("media_type")
  data = source.get("data")
  if not media_type or not isinstance(data, str):
    raise ValueError(f"{label} source missing media_type or data")
  return {
      "type": "image_url",
      "image_url": {
          "url": f"data:{media_type};base64,{data}",
      },
  }


def _tool_result_to_openai_content(content: Any) -> tuple[str, list[dict]]:
  if isinstance(content, str):
    return content, []
  if not isinstance(content, list):
    raise ValueError("tool_result content must be a string or content block list")
  text_parts: list[str] = []
  images: list[dict] = []
  for raw_block in content:
    block = _require_dict(raw_block, "tool_result content block")
    block_type = block.get("type")
    if block_type == "text":
      text_parts.append(str(block.get("text", "")))
    elif block_type == "image":
      images.append(_image_block_to_openai_part(block, "tool_result image"))
    else:
      raise ValueError(f"unsupported tool_result content block type: {block_type}")
  return "".join(text_parts), images


def _flush_user_parts(messages: list[dict], parts: list[dict]) -> None:
  if not parts:
    return
  if all(part["type"] == "text" for part in parts):
    content: Any = "".join(part["text"] for part in parts)
  else:
    content = parts.copy()
  messages.append({"role": "user", "content": content})
  parts.clear()


def _convert_user_message(message: dict) -> list[dict]:
  content = message.get("content", "")
  if isinstance(content, str):
    return [{"role": "user", "content": content}]
  if not isinstance(content, list):
    raise ValueError("user message content must be a string or content block list")

  converted: list[dict] = []
  user_parts: list[dict] = []
  tool_result_images: list[dict] = []
  for raw_block in content:
    block = _require_dict(raw_block, "user content block")
    block_type = block.get("type")
    if block_type == "text":
      user_parts.append({"type": "text", "text": str(block.get("text", ""))})
    elif block_type == "image":
      user_parts.append(_image_block_to_openai_part(block, "user image"))
    elif block_type == "tool_result":
      _flush_user_parts(converted, user_parts)
      tool_use_id = block.get("tool_use_id")
      if not tool_use_id:
        raise ValueError("tool_result block missing tool_use_id")
      result_text, result_images = _tool_result_to_openai_content(block.get("content", ""))
      converted.append({
          "role": "tool",
          "tool_call_id": tool_use_id,
          "content": result_text,
      })
      tool_result_images.extend(result_images)
    else:
      raise ValueError(f"unsupported user content block type: {block_type}")
  _flush_user_parts(converted, user_parts)
  if tool_result_images:
    converted.append({"role": "user", "content": tool_result_images})
  return converted


def _convert_assistant_message(message: dict) -> dict:
  content = message.get("content", "")
  if isinstance(content, str):
    return {"role": "assistant", "content": content}
  if not isinstance(content, list):
    raise ValueError("assistant message content must be a string or content block list")

  text_parts: list[str] = []
  tool_calls: list[dict] = []
  for raw_block in content:
    block = _require_dict(raw_block, "assistant content block")
    block_type = block.get("type")
    if block_type == "text":
      text_parts.append(str(block.get("text", "")))
    elif block_type == "tool_use":
      tool_id = block.get("id")
      name = block.get("name")
      if not tool_id or not name:
        raise ValueError("tool_use block missing id or name")
      tool_call = {
          "id": tool_id,
          "type": "function",
          "function": {
              "name": name,
              "arguments": json.dumps(block.get("input", {}), separators=(",", ":")),
          },
      }
      if block.get("extra_content") is not None:
        tool_call["extra_content"] = block["extra_content"]
      tool_calls.append(tool_call)
    else:
      raise ValueError(f"unsupported assistant content block type: {block_type}")

  converted: dict[str, Any] = {"role": "assistant", "content": "".join(text_parts) or None}
  if tool_calls:
    converted["tool_calls"] = tool_calls
  return converted


def _convert_messages(messages: list[dict], system: Any) -> list[dict]:
  system_parts: list[str] = []
  if system:
    system_parts.append(_blocks_to_text(system, "system"))

  converted: list[dict] = []
  for raw_message in messages:
    message = _require_dict(raw_message, "message")
    role = message.get("role")
    if role == "system":
      system_parts.append(_blocks_to_text(message.get("content", ""), "system message"))
    elif role == "user":
      converted.extend(_convert_user_message(message))
    elif role == "assistant":
      converted.append(_convert_assistant_message(message))
    else:
      raise ValueError(f"unsupported message role: {role}")
  if system_parts:
    converted.insert(0, {"role": "system", "content": "\n\n".join(system_parts)})
  return converted


def _is_google_gemini_endpoint(api_base: str) -> bool:
  return parse.urlsplit(api_base).hostname == "generativelanguage.googleapis.com"


def _convert_tools(tools: list[dict]) -> list[dict]:
  converted: list[dict] = []
  for raw_tool in tools:
    tool = _require_dict(raw_tool, "tool")
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
      ET.USAGE_INPUT_TOKENS: usage.get("prompt_tokens", 0),
      ET.USAGE_OUTPUT_TOKENS: usage.get("completion_tokens", 0),
      ET.USAGE_CACHE_CREATION_INPUT_TOKENS: 0,
      ET.USAGE_CACHE_READ_INPUT_TOKENS: 0,
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
    tool_use = {
        "type": "tool_use",
        "id": tool_id,
        "name": name,
        "input": json.loads(arguments),
    }
    if tool_call.get("extra_content") is not None:
      tool_use["extra_content"] = tool_call["extra_content"]
    content_blocks.append(tool_use)

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
  # Anthropic streaming clients dispatch on data["type"], so the wire format
  # pins it to the SSE event line's name; the translators below leave it out.
  data = {"type": event, **data}
  return f"event: {event}\ndata: {json.dumps(data, separators=(',', ':'))}\n\n".encode()


class OpenAIChatStreamToAnthropic:
  """Stateful translator from OpenAI chat stream chunks to Anthropic SSE events."""

  def __init__(self, model: str, gemini_stream_rules: bool = False) -> None:
    self._message_id = f"msg_{uuid.uuid4().hex}"
    self._model = model
    # One switch for the two Gemini stream shapes: a tool call that arrives in
    # one delta with no index, and a thought signature that arrives with or
    # after the call's fragments (other upstreams send neither).
    self._gemini_stream_rules = gemini_stream_rules
    self._next_index = 0
    self._text_index: int | None = None
    self._tool_indexes: dict[int, int] = {}
    self._tool_names: dict[int, str] = {}
    self._tool_ids: dict[int, str] = {}
    self._tool_extra_content: dict[int, Any] = {}
    self._tool_pending_arguments: dict[int, list[str]] = {}
    self._tool_started: set[int] = set()
    self._usage: dict = {}
    self._finish_reason: str | None = None

  def start_events(self) -> list[tuple[str, dict]]:
    return [
        (
            "message_start",
            {
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
    events.extend(self._close_tool_blocks())
    usage = _usage_from_openai(self._usage)
    events.append(
        (
            "message_delta",
            {
                "delta": {
                    "stop_reason": _stop_reason(self._finish_reason),
                    "stop_sequence": None,
                },
                "usage": usage,
            },
        ))
    events.append(("message_stop", {}))
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
    return [("content_block_stop", {"index": index})]

  def _start_tool_block(self, call_index: int) -> list[tuple[str, dict]]:
    if call_index in self._tool_started:
      return []
    content_block = {
        "type": "tool_use",
        "id": self._tool_ids[call_index],
        "name": self._tool_names[call_index],
        "input": {},
    }
    extra_content = self._tool_extra_content.get(call_index)
    if extra_content is not None:
      content_block["extra_content"] = extra_content
    events = [("content_block_start", {
        "index": self._tool_indexes[call_index],
        "content_block": content_block,
    })]
    self._tool_started.add(call_index)
    pending_arguments = self._tool_pending_arguments[call_index]
    events.extend(
        [
            (
                "content_block_delta",
                {
                    "index": self._tool_indexes[call_index],
                    "delta": {
                        "type": "input_json_delta",
                        "partial_json": arguments,
                    },
                },
            ) for arguments in pending_arguments
        ])
    pending_arguments.clear()
    return events

  def _close_tool_blocks(self) -> list[tuple[str, dict]]:
    events: list[tuple[str, dict]] = []
    for call_index in list(self._tool_indexes):
      events.extend(self._close_tool_block(call_index))
    return events

  def _close_tool_block(self, call_index: int) -> list[tuple[str, dict]]:
    events = self._start_tool_block(call_index)
    block_index = self._tool_indexes.pop(call_index)
    self._tool_names.pop(call_index)
    self._tool_ids.pop(call_index)
    self._tool_extra_content.pop(call_index, None)
    self._tool_pending_arguments.pop(call_index)
    self._tool_started.remove(call_index)
    events.append(("content_block_stop", {"index": block_index}))
    return events

  def _handle_tool_delta(self, tool_call: dict) -> list[tuple[str, dict]]:
    call_index = tool_call.get("index")
    if call_index is None:
      if not self._gemini_stream_rules:
        raise ValueError("streaming tool_call missing index")
      # Gemini sends one whole tool call per delta with no index. Slot it at
      # index 0; the id split below closes the previous call there and starts
      # the next one, and an id-less delta continues the current call.
      call_index = 0
    function = tool_call.get("function") or {}
    events: list[tuple[str, dict]] = []

    tool_id = tool_call.get("id")
    if call_index in self._tool_indexes and tool_id and tool_id != self._tool_ids[call_index]:
      events.extend(self._close_tool_block(call_index))

    if call_index not in self._tool_indexes:
      name = function.get("name")
      if not tool_id or not name:
        raise ValueError("first streaming tool_call delta missing id or function.name")
      events.extend(self._close_text_block())
      block_index = self._next_index
      self._next_index += 1
      self._tool_indexes[call_index] = block_index
      self._tool_ids[call_index] = tool_id
      self._tool_names[call_index] = name
      self._tool_pending_arguments[call_index] = []

    extra_content = tool_call.get("extra_content")
    if extra_content is not None:
      previous_extra_content = self._tool_extra_content.get(call_index)
      if call_index in self._tool_started and previous_extra_content != extra_content:
        raise ValueError("streaming tool_call extra_content changed after its content block started")
      self._tool_extra_content[call_index] = extra_content
    if extra_content is not None or not self._gemini_stream_rules:
      events.extend(self._start_tool_block(call_index))

    arguments = function.get("arguments")
    if arguments is not None:
      if call_index in self._tool_started:
        events.append(
            (
                "content_block_delta",
                {
                    "index": self._tool_indexes[call_index],
                    "delta": {
                        "type": "input_json_delta",
                        "partial_json": arguments,
                    },
                },
            ))
      else:
        self._tool_pending_arguments[call_index].append(arguments)
    return events


async def _iter_anthropic_sse(upstream: httpx.Response,
                              model: str,
                              gemini_stream_rules: bool = False) -> AsyncIterator[bytes]:
  translator = OpenAIChatStreamToAnthropic(model, gemini_stream_rules=gemini_stream_rules)
  try:
    for event, data in translator.start_events():
      yield _sse_event(event, data)

    async for line in sse.iter_sse_lines(upstream, lines_as_bytes=True):
      if not line.startswith(b"data:"):
        continue
      raw = line[len(b"data:"):].strip()
      if raw == b"[DONE]":
        break
      if not raw:
        continue
      chunk = orjson.loads(raw)
      for event, data in translator.events_for_chunk(chunk):
        yield _sse_event(event, data)

    for event, data in translator.finish_events():
      yield _sse_event(event, data)
  finally:
    await upstream.aclose()


def _upstream_headers(credential: str | None, backend_id: str) -> dict[str, str]:
  """Build upstream request headers for a backend.

  Without a credential the bare headers are returned. With a credential the
  Bearer token comes from the credentials file (``config.get_credentials().require``
  on the credential's ``api_key`` entry); a missing key raises the loader's
  ``ValueError``.
  """
  del backend_id  # Kept in the signature for call-site readability; not part of the headers.
  headers = {"Content-Type": "application/json"}
  if not credential:
    return headers
  headers["Authorization"] = f"Bearer {config.get_credentials().require(credential, 'api_key')}"
  return headers


async def _upstream_error(response: httpx.Response) -> fastapi.HTTPException:
  body = (await response.aread()).decode("utf-8", errors="replace")
  await response.aclose()
  return fastapi.HTTPException(status_code=response.status_code, detail=body)


@router.post("/openai-compatible/{backend_id}/v1/messages")
async def openai_compatible_messages(
    backend_id: str,
    request: fastapi.Request,
    cfg: config.CharlieBotConfig = fastapi.Depends(deps.get_config_on_loop),
) -> responses.Response:
  """Serve Anthropic Messages API requests through a per-backend OpenAI-compatible endpoint."""
  option = cfg.get_backend_option(backend_id)
  if option is None:
    raise fastapi.HTTPException(status_code=404, detail=f"unknown backend id: {backend_id}")
  if option.type != "cc-openai-compatible":
    raise fastapi.HTTPException(
        status_code=400, detail=f"backend '{backend_id}' is not type 'cc-openai-compatible' (got '{option.type}')")
  if not option.api_base:
    raise fastapi.HTTPException(status_code=400, detail=f"backend '{backend_id}' missing api_base")
  if not option.model:
    raise fastapi.HTTPException(status_code=400, detail=f"backend '{backend_id}' missing model")

  try:
    anthropic_payload = await request.json()
    google_gemini_endpoint = _is_google_gemini_endpoint(option.api_base)
    openai_payload = anthropic_to_openai_chat_request(anthropic_payload, upstream_model=option.model)
    upstream_url = _join_openai_chat_url(option.api_base)
    headers = _upstream_headers(option.credential, backend_id)
  except ValueError as e:
    raise deps.bad_request(e) from e

  client = http.get_http_client()
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
    return responses.StreamingResponse(
        _iter_anthropic_sse(upstream, model, gemini_stream_rules=google_gemini_endpoint),
        media_type="text/event-stream")

  upstream = await client.post(upstream_url, json=openai_payload, headers=headers, timeout=None)
  if upstream.status_code >= 400:
    raise await _upstream_error(upstream)
  try:
    data = openai_chat_response_to_anthropic(upstream.json(), model)
  except ValueError as e:
    raise fastapi.HTTPException(status_code=502, detail=str(e)) from e
  return responses.JSONResponse(data)
