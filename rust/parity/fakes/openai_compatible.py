"""Fake OpenAI-compatible chat completions endpoint; seeds are text and failure_marker.

The endpoint answers a request whose stream flag is set with three chat-completion chunks and [DONE], and any other
request with one chat completion. A request whose messages contain failure_marker gets a 503 with a plain-text body,
so a scenario reaches the proxy's upstream-error path without a second server.
"""

import json
from collections.abc import Callable

from . import proxy, recorder

DEFAULT_TEXT = "Synthetic upstream answer."
DEFAULT_FAILURE_MARKER = "synthetic-upstream-failure"
USAGE = {"prompt_tokens": 7, "completion_tokens": 4}


def validate_seed(seed: dict) -> None:
  unknown = set(seed) - {"text", "failure_marker"}
  if unknown:
    raise ValueError(f"unknown seed keys {sorted(unknown)}")
  for key in ("text", "failure_marker"):
    if key in seed and not isinstance(seed[key], str):
      raise ValueError(f"{key} must be a string")


def _frame(payload: dict) -> bytes:
  return b"data: " + json.dumps(payload, separators=(",", ":")).encode() + b"\n\n"


class Fake:
  """The deterministic upstream the cc-openai-compatible proxy calls in the other-backends scenarios."""

  def __init__(self, seed: dict, traffic: recorder.Recorder, mark_ready: Callable[[], None]) -> None:
    validate_seed(seed)
    self.text = seed.get("text", DEFAULT_TEXT)
    self.failure_marker = seed.get("failure_marker", DEFAULT_FAILURE_MARKER)
    self.traffic = traffic
    self.mark_ready = mark_ready

  def http(self, request: proxy.Request) -> proxy.HTTPResponse:
    payload = request.body
    if self.failure_marker in json.dumps(payload.get("messages", []), ensure_ascii=False):
      return proxy.HTTPResponse(503, {"Content-Type": "text/plain; charset=utf-8"}, b"synthetic upstream failure")
    if payload.get("stream"):
      half = len(self.text) // 2
      frames = [
          _frame({"choices": [{
              "index": 0,
              "delta": {
                  "role": "assistant",
                  "content": self.text[:half]
              }
          }]}),
          _frame(
              {
                  "choices": [{
                      "index": 0,
                      "delta": {
                          "content": self.text[half:]
                      },
                      "finish_reason": "stop",
                  }],
                  "usage": USAGE,
              }),
          b"data: [DONE]\n\n",
      ]
      return proxy.HTTPResponse(200, {"Content-Type": "text/event-stream"}, b"".join(frames))
    answer = {
        "id": "chatcmpl-synthetic",
        "object": "chat.completion",
        "model": payload.get("model"),
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": self.text
            },
            "finish_reason": "stop",
        }],
        "usage": USAGE,
    }
    return proxy.HTTPResponse(
        200, {"Content-Type": "application/json"},
        json.dumps(answer, separators=(",", ":")).encode())
