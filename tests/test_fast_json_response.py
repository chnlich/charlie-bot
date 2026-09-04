"""FastJsonResponse render contract.

The hot JSON endpoints serve pre-built plain payloads, so the render is the
remaining per-request cost. The fast render escapes non-ASCII instead of
emitting raw UTF-8, which CPython's C JSON encoder produces several times
faster on CJK-bearing payloads; callers rely on the parsed content being
identical and on NaN failing loudly instead of emitting invalid JSON.
"""

import json

from fastapi.responses import JSONResponse

from src.api.responses import FastJsonResponse

_CJK_PAYLOAD = {
    "messages": [{
        "role": "assistant",
        "content": "问候语 — em-dash ünïcode"
    }],
    "has_more": True,
    "next_before": 7,
}


def test_parsed_content_matches_the_starlette_render() -> None:
  fast = FastJsonResponse(_CJK_PAYLOAD)
  slow = JSONResponse(_CJK_PAYLOAD)
  assert json.loads(bytes(fast.body)) == json.loads(bytes(slow.body))


def test_body_is_ascii_escaped() -> None:
  body = bytes(FastJsonResponse(_CJK_PAYLOAD).body)
  assert body.isascii()
  # The escaping is the only body difference: same keys, separators, ordering.
  assert b'"next_before":7' in body
  assert b'"\\u95ee\\u5019\\u8bed' in body


def test_media_type_is_json() -> None:
  response = FastJsonResponse(_CJK_PAYLOAD)
  assert response.media_type == "application/json"
  assert response.headers["content-type"] == "application/json"


def test_nan_raises_instead_of_emitting_invalid_json() -> None:
  try:
    FastJsonResponse({"a": float("nan")})
  except ValueError:
    pass
  else:
    raise AssertionError("NaN must fail the render, not emit NaN into the body")
