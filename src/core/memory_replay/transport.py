"""Replay model transport: one content-only chat-completions request per stage call.

The transport speaks to a configured OpenAI-compatible endpoint (a
``charlie-code`` or ``cc-openai-compatible`` backends.options entry) with the
repo's own config and credential accessors. It offers no tools and accepts no
tool calls, so a replay model has no shell and no write capability by
construction — the isolation the plan demands is a property of the transport,
not a sentence in the prompt. Backend ids, models, endpoints, and credentials
stay runtime configuration; the credential only ever goes into the
Authorization header and is never logged, printed, or recorded.
"""

import time
from dataclasses import dataclass
from typing import Protocol

import httpx

from src.core.config import get_credentials
from src.core.memory_replay.errors import ReplayBackendError, ReplayTransportError
from src.core.models import BackendOption, BackendType

SUPPORTED_BACKEND_TYPES = (BackendType.CHARLIE_CODE, BackendType.CC_OPENAI_COMPATIBLE)


@dataclass
class TransportResult:
  """One completed stage call: the reply text plus whatever usage the endpoint reported."""

  text: str
  model: str
  prompt_tokens: int | None
  output_tokens: int | None
  latency_ms: int


class ReplayTransport(Protocol):
  """What a stage call needs: a system prompt, a user content, one text answer."""

  def complete(self, *, system: str, user: str) -> TransportResult:
    ...


def request_model_for(option: BackendOption) -> str:
  """The model name the endpoint expects, and the visible failure for unsupported types.

  A ``charlie-code`` entry names its model with CLC's provider prefix
  (``openai/<served-name>``); the direct chat request carries the served name.
  A ``cc-openai-compatible`` entry's model goes through verbatim, matching the
  repo's Anthropic-proxy behavior for that type.
  """
  if option.type not in SUPPORTED_BACKEND_TYPES:
    raise ReplayBackendError(
        f"backend '{option.id}' has type '{option.type}'; replay supports "
        f"{', '.join(t.value for t in SUPPORTED_BACKEND_TYPES)} entries")
  if not option.model or not option.api_base:
    raise ReplayBackendError(f"backend '{option.id}' needs both model and api_base for replay")
  if option.type == BackendType.CHARLIE_CODE:
    return option.model.removeprefix("openai/")
  return option.model


class OpenAICompatibleTransport:
  """Content-only transport over an OpenAI-compatible chat-completions endpoint."""

  def __init__(self, *, url: str, model: str, headers: dict[str, str], timeout_s: float = 600.0) -> None:
    self._url = url
    self._model = model
    self._headers = headers
    self._timeout_s = timeout_s

  @classmethod
  def from_config(cls, option: BackendOption) -> "OpenAICompatibleTransport":
    """Build from one configured backend option; credentials come from the profile's credentials file."""
    model = request_model_for(option)
    url = f"{option.api_base.rstrip('/')}/chat/completions"
    headers = {"Content-Type": "application/json"}
    if option.credential:
      headers["Authorization"] = f"Bearer {get_credentials().require(option.credential, 'api_key')}"
    return cls(url=url, model=model, headers=headers)

  def complete(self, *, system: str, user: str) -> TransportResult:
    payload = {
        "model": self._model,
        "messages": [
            {
                "role": "system",
                "content": system
            },
            {
                "role": "user",
                "content": user
            },
        ],
        "temperature": 0,
        "stream": False,
    }
    started = time.monotonic()
    with httpx.Client(timeout=self._timeout_s) as client:
      response = client.post(self._url, json=payload, headers=self._headers)
    latency_ms = int((time.monotonic() - started) * 1000)
    if response.status_code != 200:
      body = response.text[:400].replace("\n", " ")
      raise ReplayTransportError(f"model endpoint returned HTTP {response.status_code}: {body}")
    try:
      data = response.json()
    except ValueError as e:
      raise ReplayTransportError(f"model endpoint returned a non-JSON body: {e}") from e
    try:
      content = data["choices"][0]["message"]["content"]
    except (KeyError, IndexError, TypeError) as e:
      raise ReplayTransportError(f"model response envelope is missing fields: {e}") from e
    if not isinstance(content, str):
      raise ReplayTransportError("model response content is not a string")
    usage = data.get("usage")
    return TransportResult(
        text=content,
        model=self._model,
        prompt_tokens=_usage_int(usage, "prompt_tokens"),
        output_tokens=_usage_int(usage, "completion_tokens"),
        latency_ms=latency_ms)


def _usage_int(usage: object, key: str) -> int | None:
  """The named token count when the endpoint reported it as an int; None otherwise."""
  if not isinstance(usage, dict):
    return None
  value = usage.get(key)
  return value if isinstance(value, int) and not isinstance(value, bool) else None
