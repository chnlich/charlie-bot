"""FastJsonResponse render contract.

The hot JSON endpoints serve pre-built plain payloads, so the render is the
remaining per-request cost. orjson renders parsed-identically to the stdlib
encoder and emits raw UTF-8, so callers rely on the parsed content being
identical while the wire body shrinks on non-ASCII payloads. The serializer
boundaries are deliberate and pinned here: NaN/Infinity renders as null
(valid JSON — the wire never breaks), and a non-str dict key raises instead
of the stdlib's silent str coercion.
"""

import json

from fastapi import responses

from src.infra import responses as responses_api

_CJK_PAYLOAD = {
    "messages": [{
        "role": "assistant",
        "content": "问候语 — em-dash ünïcode"
    }],
    "has_more": True,
    "next_before": 7,
}


def test_parsed_content_matches_the_starlette_render() -> None:
  fast = responses_api.FastJsonResponse(_CJK_PAYLOAD)
  slow = responses.JSONResponse(_CJK_PAYLOAD)
  assert json.loads(bytes(fast.body)) == json.loads(bytes(slow.body))


def test_nan_renders_as_null_not_invalid_json() -> None:
  body = bytes(responses_api.FastJsonResponse({"a": float("nan"), "b": float("inf")}).body)
  # orjson's boundary: NaN/Infinity render as null — valid JSON on the wire,
  # the same boundary the stream funnels accepted at their orjson swap.
  assert json.loads(body) == {"a": None, "b": None}


def test_non_str_dict_key_raises_instead_of_silent_coercion() -> None:
  try:
    responses_api.FastJsonResponse({1: "a"})
  except TypeError:
    pass
  else:
    raise AssertionError("a non-str dict key must fail the render loudly")
