"""Official Claude Code hook bridge for the subscription transport.

This module is the only place where Claude Code hook payloads are interpreted.  The
transport helper deliberately forwards the payload as opaque JSON; this module
validates the protocol and produces the existing CharlieBot event shapes.
"""

from __future__ import annotations

import asyncio
import json
import os
import uuid
from dataclasses import dataclass, field
from enum import IntEnum
from pathlib import Path
from typing import Any

from src.core import event_types as ET


class PromptDelivery(IntEnum):
  """Delivery state for the one prompt passed to the interactive Claude process."""

  UNKNOWN = 0
  ACKNOWLEDGED = 1


class HookBridgeError(RuntimeError):
  """Base class for hook protocol and transport failures."""


class HookProtocolError(HookBridgeError):
  """Raised when an official hook payload is malformed or mismatched."""


_SESSION_START_SOURCES = frozenset({"startup", "resume", "clear", "compact"})
NOTIFICATION_TYPES = (
    "permission_prompt",
    "idle_prompt",
    "auth_success",
    "elicitation_dialog",
    "elicitation_complete",
    "elicitation_response",
    "agent_needs_input",
    "agent_completed",
)
_NOTIFICATION_TYPES = frozenset(NOTIFICATION_TYPES)
_SESSION_END_REASONS = frozenset({
    "clear",
    "resume",
    "logout",
    "prompt_input_exit",
    "bypass_permissions_disabled",
    "other",
})
_POST_COMPACT_TRIGGERS = frozenset({"manual", "auto"})
_STOP_FAILURE_TYPES = frozenset({
    "rate_limit",
    "overloaded",
    "authentication_failed",
    "oauth_org_not_allowed",
    "billing_error",
    "invalid_request",
    "model_not_found",
    "server_error",
    "max_output_tokens",
    "unknown",
})
_CORRELATION_FIELDS = ("turn_id", "prompt_id", "turnId", "promptId")


def _required(payload: dict[str, Any], field_name: str, expected_type: type[Any]) -> Any:
  if field_name not in payload:
    raise HookProtocolError(f"hook payload is missing required field '{field_name}'")
  value = payload[field_name]
  if not isinstance(value, expected_type):
    raise HookProtocolError(
        f"hook payload field '{field_name}' must be {expected_type.__name__}, got {type(value).__name__}")
  return value


def _required_string(payload: dict[str, Any], field_name: str) -> str:
  value = _required(payload, field_name, str)
  if not value:
    raise HookProtocolError(f"hook payload field '{field_name}' must not be empty")
  return value


def _canonical_cwd(value: str) -> str:
  return os.path.realpath(value)


def _tool_result_content(value: Any) -> Any:
  """Keep the existing tool-result content shape while accepting structured output."""
  if isinstance(value, (str, list)):
    return value
  if isinstance(value, dict):
    if isinstance(value.get("content"), (str, list)):
      return value["content"]
    stdout = value.get("stdout")
    stderr = value.get("stderr")
    if isinstance(stdout, str) or isinstance(stderr, str):
      parts = [part for part in (stdout, stderr) if isinstance(part, str) and part]
      return "\n".join(parts)
  return json.dumps(value, ensure_ascii=False, separators=(",", ":"))


@dataclass
class HookTurnState:
  """In-memory state machine for one Claude Code turn."""

  expected_session_id: str
  expected_cwd: str
  expected_prompt: str
  model: str = ""
  expected_source: str = "startup"
  delivery: PromptDelivery = PromptDelivery.UNKNOWN
  session_started: bool = False
  correlation_id: str | None = None
  correlation_field: str | None = None
  stop_seen: bool = False
  stop_candidate: str | None = None
  idle_seen: bool = False
  active: bool = False
  failure: HookBridgeError | None = None
  _seen_message_batches: set[tuple[str, int]] = field(default_factory=set)

  def _validate_common(self, event_name: str, payload: dict[str, Any]) -> None:
    hook_event_name = _required_string(payload, "hook_event_name")
    if hook_event_name != event_name:
      raise HookProtocolError(
          f"hook event name mismatch: bridge received {event_name}, payload says {hook_event_name}")
    session_id = _required_string(payload, "session_id")
    if session_id != self.expected_session_id:
      raise HookProtocolError(
          f"hook session id mismatch: expected {self.expected_session_id}, got {session_id}")
    cwd = _required_string(payload, "cwd")
    if _canonical_cwd(cwd) != _canonical_cwd(self.expected_cwd):
      raise HookProtocolError(
          f"hook cwd mismatch: expected {self.expected_cwd}, got {cwd}")

  def _correlation(self, payload: dict[str, Any], *, event_name: str) -> tuple[str, str]:
    if self.correlation_field is not None:
      if self.correlation_field not in payload:
        raise HookProtocolError(
            f"{event_name} payload is missing established correlation field '{self.correlation_field}'")
      value = payload[self.correlation_field]
      if not isinstance(value, str) or not value:
        raise HookProtocolError(
            f"{event_name} correlation field '{self.correlation_field}' must be a non-empty string")
      return self.correlation_field, value
    for field_name in _CORRELATION_FIELDS:
      if field_name in payload:
        value = payload[field_name]
        if not isinstance(value, str) or not value:
          raise HookProtocolError(
              f"{event_name} correlation field '{field_name}' must be a non-empty string")
        return field_name, value
    fields = ", ".join(_CORRELATION_FIELDS)
    raise HookProtocolError(
        f"{event_name} payload is missing the official turn correlation field; checked {fields}")

  def handle(self, event_name: str, payload: dict[str, Any]) -> list[dict[str, Any]]:
    """Validate one hook payload and return zero or more CharlieBot events."""
    if event_name == "SessionStart":
      return self._handle_session_start(payload)
    if event_name == "UserPromptSubmit":
      return self._handle_user_prompt_submit(payload)
    if event_name == "MessageDisplay":
      return self._handle_message_display(payload)
    if event_name == "PreToolUse":
      return self._handle_pre_tool_use(payload)
    if event_name in ("PostToolUse", "PostToolUseFailure"):
      return self._handle_post_tool_use(event_name, payload)
    if event_name == "PostCompact":
      return self._handle_post_compact(payload)
    if event_name == "Stop":
      return self._handle_stop(payload)
    if event_name == "StopFailure":
      return self._handle_stop_failure(payload)
    if event_name == "Notification":
      return self._handle_notification(payload)
    if event_name == "PermissionRequest":
      return self._handle_permission_request(payload)
    if event_name == "SessionEnd":
      return self._handle_session_end(payload)
    raise HookProtocolError(f"unsupported hook event '{event_name}'")

  def _handle_session_start(self, payload: dict[str, Any]) -> list[dict[str, Any]]:
    self._validate_common("SessionStart", payload)
    source = _required_string(payload, "source")
    model = payload.get("model", self.model)
    if not isinstance(model, str):
      raise HookProtocolError("SessionStart model must be a string when present")
    if source not in _SESSION_START_SOURCES:
      raise HookProtocolError(f"unknown SessionStart source '{source}'")
    if source != self.expected_source:
      raise HookProtocolError(
          f"SessionStart source mismatch: expected {self.expected_source}, got {source}")
    if self.session_started:
      raise HookProtocolError("duplicate SessionStart hook for one Claude process")
    self.session_started = True
    return [{
        "type": ET.SYSTEM,
        "subtype": "init",
        "cwd": self.expected_cwd,
        "session_id": self.expected_session_id,
        "tools": [],
        "mcp_servers": [],
        "model": model,
        "permissionMode": "bypassPermissions",
        "output_style": "default",
        "agents": [],
        "skills": [],
        "plugins": [],
        "uuid": str(uuid.uuid4()),
    }]

  def _handle_user_prompt_submit(self, payload: dict[str, Any]) -> list[dict[str, Any]]:
    self._validate_common("UserPromptSubmit", payload)
    if not self.session_started:
      raise HookProtocolError("UserPromptSubmit arrived before SessionStart")
    prompt = _required(payload, "prompt", str)
    field_name, correlation_id = self._correlation(payload, event_name="UserPromptSubmit")
    if prompt != self.expected_prompt:
      raise HookProtocolError(
          "UserPromptSubmit prompt mismatch: the hook blocked model processing; "
          "the submitted prompt is not the expected full prompt")
    self.correlation_field = field_name
    self.correlation_id = correlation_id
    self.delivery = PromptDelivery.ACKNOWLEDGED
    self.active = True
    return []

  def _validate_current_turn(self, event_name: str, payload: dict[str, Any]) -> None:
    self._validate_common(event_name, payload)
    if not self.session_started:
      raise HookProtocolError(f"{event_name} arrived before SessionStart")
    if self.delivery != PromptDelivery.ACKNOWLEDGED:
      raise HookProtocolError(f"{event_name} arrived before UserPromptSubmit acknowledgement")
    if any(field_name in payload for field_name in _CORRELATION_FIELDS):
      _, correlation_id = self._correlation(payload, event_name=event_name)
      if correlation_id != self.correlation_id:
        raise HookProtocolError(
            f"{event_name} turn correlation mismatch: expected {self.correlation_id}, got {correlation_id}")

  def _handle_message_display(self, payload: dict[str, Any]) -> list[dict[str, Any]]:
    self._validate_current_turn("MessageDisplay", payload)
    _, correlation_id = self._correlation(payload, event_name="MessageDisplay")
    if correlation_id != self.correlation_id:
      raise HookProtocolError(
          f"MessageDisplay turn correlation mismatch: expected {self.correlation_id}, got {correlation_id}")
    turn_id = _required_string(payload, "turn_id")
    message_id = _required_string(payload, "message_id")
    index = _required(payload, "index", int)
    if index < 0:
      raise HookProtocolError("MessageDisplay index must be non-negative")
    _required(payload, "final", bool)
    delta = _required(payload, "delta", str)
    batch_key = (message_id, index)
    if batch_key in self._seen_message_batches:
      raise HookProtocolError(f"duplicate MessageDisplay batch for {message_id} index {index}")
    self._seen_message_batches.add(batch_key)
    if not delta:
      return []
    return [{
        "type": ET.ASSISTANT,
        "message": {
            "role": "assistant",
            "content": [{"type": "text", "text": delta}],
        },
        "parent_tool_use_id": None,
        "session_id": self.expected_session_id,
        "message_id": message_id,
        "turn_id": turn_id,
        "uuid": str(uuid.uuid4()),
    }]

  def _handle_pre_tool_use(self, payload: dict[str, Any]) -> list[dict[str, Any]]:
    self._validate_current_turn("PreToolUse", payload)
    tool_name = _required_string(payload, "tool_name")
    tool_input = _required(payload, "tool_input", dict)
    tool_use_id = _required_string(payload, "tool_use_id")
    return [{
        "type": ET.ASSISTANT,
        "message": {
            "role": "assistant",
            "content": [{
                "type": ET.TOOL_USE,
                "id": tool_use_id,
                "name": tool_name,
                "input": tool_input,
            }],
        },
        "parent_tool_use_id": None,
        "session_id": self.expected_session_id,
        "uuid": str(uuid.uuid4()),
    }]

  def _handle_post_tool_use(self, event_name: str, payload: dict[str, Any]) -> list[dict[str, Any]]:
    self._validate_current_turn(event_name, payload)
    _required_string(payload, "tool_name")
    _required(payload, "tool_input", dict)
    tool_use_id = _required_string(payload, "tool_use_id")
    if event_name == "PostToolUse":
      tool_response = _required(payload, "tool_response", object)
      is_error = False
    else:
      tool_response = _required(payload, "error", str)
      is_error = True
    result_block = {
        "type": ET.TOOL_RESULT,
        "tool_use_id": tool_use_id,
        "content": _tool_result_content(tool_response),
        "is_error": is_error,
    }
    return [{
        "type": ET.USER,
        "message": {
            "role": "user",
            "content": [result_block],
        },
        "parent_tool_use_id": None,
        "session_id": self.expected_session_id,
        "uuid": str(uuid.uuid4()),
        "tool_use_result": tool_response,
    }]

  def _handle_post_compact(self, payload: dict[str, Any]) -> list[dict[str, Any]]:
    self._validate_current_turn("PostCompact", payload)
    trigger = _required_string(payload, "trigger")
    if trigger not in _POST_COMPACT_TRIGGERS:
      raise HookProtocolError(f"unknown PostCompact trigger '{trigger}'")
    _required_string(payload, "compact_summary")
    event: dict[str, Any] = {
        "type": ET.CONTEXT_COMPACTED,
        "trigger": trigger,
        "pre_tokens": payload.get("pre_tokens"),
    }
    if "pre_tokens" in payload:
      pre_tokens = payload["pre_tokens"]
      if not isinstance(pre_tokens, (int, float)):
        raise HookProtocolError("PostCompact pre_tokens must be numeric when present")
      event["pre_tokens"] = pre_tokens
    return [event]

  def _handle_stop(self, payload: dict[str, Any]) -> list[dict[str, Any]]:
    self._validate_current_turn("Stop", payload)
    last_message = _required(payload, "last_assistant_message", str)
    background_tasks = _required(payload, "background_tasks", list)
    session_crons = _required(payload, "session_crons", list)
    if background_tasks or session_crons:
      raise HookProtocolError(
          f"Stop reported active background work: {len(background_tasks)} background tasks, "
          f"{len(session_crons)} scheduled follow-ups")
    stop_hook_active = _required(payload, "stop_hook_active", bool)
    if stop_hook_active:
      raise HookProtocolError("Stop hook reported stop_hook_active=true; final answer is not stable")
    if self.stop_seen:
      raise HookProtocolError("duplicate Stop hook for one turn")
    self.stop_seen = True
    self.stop_candidate = last_message
    return []

  def _handle_stop_failure(self, payload: dict[str, Any]) -> list[dict[str, Any]]:
    self._validate_current_turn("StopFailure", payload)
    error = _required_string(payload, "error")
    if error not in _STOP_FAILURE_TYPES:
      raise HookProtocolError(f"unknown StopFailure error value '{error}'")
    error_details = payload.get("error_details")
    if error_details is not None and not isinstance(error_details, str):
      raise HookProtocolError("StopFailure error_details must be a string when present")
    candidate = payload.get("last_assistant_message")
    if candidate is not None and not isinstance(candidate, str):
      raise HookProtocolError("StopFailure last_assistant_message must be a string when present")
    message = candidate or error_details or error
    error_event: dict[str, Any] = {
        "type": ET.ERROR,
        "message": message,
        "content": message,
        "error": error,
        "error_details": error_details,
        "uuid": str(uuid.uuid4()),
    }
    events: list[dict[str, Any]] = []
    if error == "rate_limit":
      events.append({
          "type": ET.RATE_LIMIT_EVENT,
          "rate_limit_info": {
              "status": "rejected",
              "rateLimitType": "unknown",
              "resetsAt": "unknown",
          },
          "uuid": str(uuid.uuid4()),
      })
    events.append(error_event)
    self.failure = HookBridgeError(f"StopFailure reported {error}: {message}")
    return events

  def _handle_notification(self, payload: dict[str, Any]) -> list[dict[str, Any]]:
    self._validate_common("Notification", payload)
    notification_type = _required_string(payload, "notification_type")
    _required_string(payload, "message")
    if notification_type not in _NOTIFICATION_TYPES:
      raise HookProtocolError(f"unknown Notification notification_type '{notification_type}'")
    if notification_type == "idle_prompt":
      if not self.active:
        raise HookProtocolError("idle_prompt arrived before UserPromptSubmit for the turn")
      if not self.stop_seen:
        raise HookProtocolError("idle_prompt arrived before Stop for the turn")
      if any(field_name in payload for field_name in _CORRELATION_FIELDS):
        _, correlation_id = self._correlation(payload, event_name="Notification")
        if correlation_id != self.correlation_id:
          raise HookProtocolError(
              f"Notification turn correlation mismatch: expected {self.correlation_id}, got {correlation_id}")
      self.idle_seen = True
      return []
    if notification_type in {"permission_prompt", "elicitation_dialog", "agent_needs_input"}:
      raise HookProtocolError(
          f"Claude Code emitted unsupported blocking notification '{notification_type}'; "
          "the GUI channel cannot answer it")
    return []

  def _handle_permission_request(self, payload: dict[str, Any]) -> list[dict[str, Any]]:
    self._validate_current_turn("PermissionRequest", payload)
    tool_name = _required_string(payload, "tool_name")
    _required(payload, "tool_input", dict)
    raise HookProtocolError(
        f"Claude Code requested structured permission for tool '{tool_name}'; "
        "the GUI channel cannot answer it")

  def _handle_session_end(self, payload: dict[str, Any]) -> list[dict[str, Any]]:
    self._validate_common("SessionEnd", payload)
    reason = _required_string(payload, "reason")
    if reason not in _SESSION_END_REASONS:
      raise HookProtocolError(f"unknown SessionEnd reason '{reason}'")
    if self.active and not self.idle_seen:
      raise HookProtocolError(f"SessionEnd occurred during an active turn (reason={reason})")
    return []


class HookBridge:
  """Authenticated local Unix-socket server for one interactive Claude process."""

  def __init__(self, socket_path: Path, token: str, state: HookTurnState) -> None:
    self.socket_path = socket_path
    self.token = token
    self.state = state
    self.events: asyncio.Queue[dict[str, Any]] = asyncio.Queue()
    self.failure: HookBridgeError | None = None
    self._server: asyncio.AbstractServer | None = None
    self._client_tasks: set[asyncio.Task[None]] = set()

  async def start(self) -> None:
    if self.socket_path.exists():
      raise HookBridgeError(f"hook bridge socket already exists: {self.socket_path}")
    self._server = await asyncio.start_unix_server(self._serve_client, path=str(self.socket_path))

  async def stop(self) -> None:
    if self._server is not None:
      self._server.close()
      await self._server.wait_closed()
      self._server = None
    tasks = list(self._client_tasks)
    for task in tasks:
      task.cancel()
    if tasks:
      results = await asyncio.gather(*tasks, return_exceptions=True)
      for task, result in zip(tasks, results):
        if isinstance(result, BaseException) and not isinstance(result, asyncio.CancelledError):
          raise HookBridgeError(f"hook bridge client shutdown failed for {task}: {result}") from result
    if self.socket_path.exists():
      self.socket_path.unlink()

  def _record_failure(self, error: HookBridgeError) -> None:
    if self.failure is None:
      self.failure = error
      self.state.failure = error

  async def _serve_client(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
    task = asyncio.current_task()
    assert task is not None
    self._client_tasks.add(task)
    try:
      try:
        raw = await reader.readline()
        if not raw:
          raise HookBridgeError("hook bridge received an empty request")
        try:
          envelope = json.loads(raw.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
          raise HookBridgeError(f"hook bridge received malformed JSON: {error}") from error
        if not isinstance(envelope, dict):
          raise HookBridgeError("hook bridge request must be a JSON object")
        if envelope.get("token") != self.token:
          raise HookBridgeError("hook bridge authentication token mismatch")
        payload = envelope.get("payload")
        if not isinstance(payload, dict):
          raise HookBridgeError("hook bridge payload must be a JSON object")
        event_name = payload.get("hook_event_name")
        if not isinstance(event_name, str) or not event_name:
          raise HookProtocolError("hook bridge payload is missing required field 'hook_event_name'")
        events = self.state.handle(event_name, payload)
        for event in events:
          await self.events.put(event)
        if self.state.failure is not None:
          self._record_failure(self.state.failure)
        response = {"ok": True}
      except HookBridgeError as error:
        self._record_failure(error)
        response = {"ok": False, "error": str(error)}
      except asyncio.CancelledError:
        raise
      except Exception as error:
        bridge_error = HookBridgeError(f"hook bridge transport failure: {error}")
        self._record_failure(bridge_error)
        response = {"ok": False, "error": str(bridge_error)}
      try:
        writer.write((json.dumps(response, ensure_ascii=False, separators=(",", ":")) + "\n").encode("utf-8"))
        await writer.drain()
      except asyncio.CancelledError:
        raise
      except (ConnectionError, OSError) as error:
        self._record_failure(HookBridgeError(f"hook bridge transport failure: {error}"))
    finally:
      writer.close()
      try:
        await writer.wait_closed()
      except (ConnectionError, OSError) as error:
        self._record_failure(HookBridgeError(f"hook bridge transport failure while closing: {error}"))
      self._client_tasks.discard(task)
