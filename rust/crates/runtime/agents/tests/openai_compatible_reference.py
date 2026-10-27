"""The Anthropic proxy route run by the Python server over the case matrix the Rust route replays.

Each case posts one body to /api/anthropic-proxy/openai-compatible/case-<index>/v1/messages. The upstream is an httpx
MockTransport that answers with the case's status and byte chunks and records the request it was sent. One JSON record
per case goes to stdout: the case input, and what the Python route did with it: its status, its body with message ids
masked, the request it sent upstream, and whether a stream reached its message_stop frame.
"""

import asyncio
import json
import re
import sys
from collections.abc import AsyncIterator
from pathlib import Path

REPO = Path(__file__).resolve().parents[5]
sys.path.insert(0, str(REPO))

import fastapi  # noqa: E402
import httpx  # noqa: E402
from fastapi import testclient  # noqa: E402

from src.backends import openai_compatible  # noqa: E402
from src.backends.openai_compatible import anthropic_proxy  # noqa: E402
from src.infra import backend_models, config, credentials, http, models  # noqa: E402
from src.runtime.api import deps  # noqa: E402

ABSENT = object()
MODEL = "synthetic-model"
ACCESS_KEY = "synthetic-access-key"
CREDENTIALS_PATH = Path("/nonexistent/credentials.yaml")
MESSAGE_ID = re.compile(rb"msg_[0-9a-f]{32}")
MESSAGE_STOP = b'event: message_stop\ndata: {"type":"message_stop"}\n\n'
DONE = b"data: [DONE]\n\n"

JSON_REPLY = json.dumps(
    {
        "id": "chatcmpl-synthetic",
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "hi"
            },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 1,
            "completion_tokens": 1
        },
    }).encode()


def _data(payload, ensure_ascii: bool = True) -> bytes:
  return b"data: " + json.dumps(payload, ensure_ascii=ensure_ascii).encode() + b"\n\n"


def _chunk(content=None, **fields) -> dict:
  delta = {} if content is None else {"content": content}
  choice = {"delta": delta}
  choice.update(fields.pop("choice", {}))
  return {"choices": [choice], **fields}


TEXT_STREAM = [
    _data(_chunk("Hel")),
    _data(_chunk("lo", choice={"finish_reason": "stop"}, usage={
        "prompt_tokens": 1,
        "completion_tokens": 2
    })),
    DONE,
]

# The same stream with CRLF terminators, cut between the CR and the LF of its first terminator.
CRLF_TEXT = b"".join(TEXT_STREAM).replace(b"\n", b"\r\n")
CRLF_CUT = CRLF_TEXT.index(b"\r\n") + 1
CRLF_CHUNKS = [CRLF_TEXT[:CRLF_CUT], CRLF_TEXT[CRLF_CUT:]]

# A frame whose JSON string carries raw U+0085 and U+2028, cut inside each character's UTF-8 bytes.
NEL = "\x85"
LINE_SEPARATOR = chr(0x2028)
SPLIT_LINE_RAW = _data(_chunk("grep hit: a:48<" + NEL + ">NEL-CHAR" + LINE_SEPARATOR + ">LS-CHAR"), ensure_ascii=False)
SPLIT_LINE_CHUNKS = [
    SPLIT_LINE_RAW[:SPLIT_LINE_RAW.index(NEL.encode()) + 1],
    SPLIT_LINE_RAW[SPLIT_LINE_RAW.index(NEL.encode()) + 1:SPLIT_LINE_RAW.index(LINE_SEPARATOR.encode())],
    SPLIT_LINE_RAW[SPLIT_LINE_RAW.index(LINE_SEPARATOR.encode()):],
    DONE,
]

# A frame with a raw é, cut inside its two UTF-8 bytes.
UTF8_RAW = _data(_chunk("é"), ensure_ascii=False)
UTF8_SPLIT_CHUNKS = [UTF8_RAW[:UTF8_RAW.index("é".encode()) + 1], UTF8_RAW[UTF8_RAW.index("é".encode()) + 1:], DONE]

ONE_TOOL = [{"name": "Bash", "description": "Run a shell command", "input_schema": {"type": "object"}}]

SYSTEMS = {
    "absent": ABSENT,
    "string": "Be brief.",
    "blocks": [{
        "type": "text",
        "text": "A"
    }, {
        "type": "text",
        "text": "B"
    }],
    "empty-list": [],
    "empty-string": "",
    "null": None,
    "image-block": [{
        "type": "image"
    }],
    "number": 42,
    "number-block": [42],
}

MESSAGES = {
    "user-string": [{
        "role": "user",
        "content": "hi"
    }],
    "user-blocks": [{
        "role": "user",
        "content": [{
            "type": "text",
            "text": "a"
        }, {
            "type": "text",
            "text": "b"
        }]
    }],
    "user-empty-string": [{
        "role": "user",
        "content": ""
    }],
    "user-empty-list": [{
        "role": "user",
        "content": []
    }],
    "assistant-then-user": [{
        "role": "assistant",
        "content": "yo"
    }, {
        "role": "user",
        "content": "hi"
    }],
    "assistant-text-and-tool-use":
        [
            {
                "role":
                    "assistant",
                "content":
                    [
                        {
                            "type": "text",
                            "text": "x"
                        }, {
                            "type": "tool_use",
                            "id": "t1",
                            "name": "Bash",
                            "input": {
                                "cmd": "ls"
                            }
                        }
                    ],
            }
        ],
    "tool-use-without-input": [{
        "role": "assistant",
        "content": [{
            "type": "tool_use",
            "id": "t1",
            "name": "Bash"
        }]
    }],
    "tool-use-without-id": [{
        "role": "assistant",
        "content": [{
            "type": "tool_use",
            "name": "Bash",
            "input": {}
        }]
    }],
    "tool-use-non-ascii-input":
        [
            {
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": "t1",
                    "name": "Bash",
                    "input": {
                        "b": 1,
                        "a": "é"
                    }
                }],
            }
        ],
    "tool-result-string":
        [
            {
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": "t1",
                    "name": "Bash",
                    "input": {
                        "cmd": "ls"
                    }
                }],
            }, {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "t1",
                    "content": "out"
                }]
            }
        ],
    "tool-result-blocks-then-text":
        [
            {
                "role":
                    "user",
                "content":
                    [
                        {
                            "type": "tool_result",
                            "tool_use_id": "t1",
                            "content": [{
                                "type": "text",
                                "text": "out"
                            }]
                        }, {
                            "type": "text",
                            "text": "next"
                        }
                    ],
            }
        ],
    "tool-result-is-error":
        [
            {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "t1",
                    "is_error": True,
                    "content": "boom"
                }]
            }
        ],
    "tool-result-without-id": [{
        "role": "user",
        "content": [{
            "type": "tool_result",
            "content": "x"
        }]
    }],
    "tool-result-without-content": [{
        "role": "user",
        "content": [{
            "type": "tool_result",
            "tool_use_id": "t1"
        }]
    }],
    "image-block": [{
        "role": "user",
        "content": [{
            "type": "image",
            "source": {}
        }]
    }],
    "text-block-without-text": [{
        "role": "user",
        "content": [{
            "type": "text"
        }]
    }],
    "text-block-number": [{
        "role": "user",
        "content": [{
            "type": "text",
            "text": 5
        }]
    }],
    "tool-role": [{
        "role": "tool",
        "content": "x"
    }],
    "role-missing": [{
        "content": "x"
    }],
    "message-not-object": ["hi"],
    "content-number": [{
        "role": "user",
        "content": 5
    }],
    "assistant-content-number": [{
        "role": "assistant",
        "content": 5
    }],
    "assistant-thinking-block":
        [{
            "role": "assistant",
            "content": [{
                "type": "text",
                "text": "x"
            }, {
                "type": "thinking",
                "thinking": "..."
            }],
        }],
    "non-ascii-text": [{
        "role": "user",
        "content": "h\xe9llo " + LINE_SEPARATOR + " " + chr(0x2713)
    }],
}

TOOLS = {
    "absent": ABSENT,
    "empty-list": [],
    "one": ONE_TOOL,
    "no-description": [{
        "name": "Bash",
        "input_schema": {}
    }],
    "built-in-type": [{
        "type": "text_editor_20250124",
        "name": "x",
        "input_schema": {}
    }],
    "without-input-schema": [{
        "name": "Bash"
    }],
    "null-input-schema": [{
        "name": "Bash",
        "input_schema": None
    }],
    "without-name": [{
        "input_schema": {}
    }],
    "not-object": ["x"],
    "object-instead-of-list": {
        "name": "x"
    },
}

TOOL_CHOICES = {
    "absent": ABSENT,
    "null": None,
    "auto": {
        "type": "auto"
    },
    "any": {
        "type": "any"
    },
    "none": {
        "type": "none"
    },
    "tool": {
        "type": "tool",
        "name": "Bash"
    },
    "tool-without-name": {
        "type": "tool"
    },
    "unknown-kind": {
        "type": "bogus"
    },
    "string": "auto",
    "null-type": {
        "type": None
    },
}

OBJECT_CHOICES = {
    "auto": {
        "type": "auto"
    },
    "any": {
        "type": "any"
    },
    "none": {
        "type": "none"
    },
    "tool": {
        "type": "tool",
        "name": "Bash"
    },
    "tool-without-name": {
        "type": "tool"
    },
}

DISABLE_PARALLEL = {
    "absent": ABSENT,
    "null": None,
    "true": True,
    "false": False,
    "zero": 0,
    "text": "yes",
}

OPTIONAL = {
    "temperature": [("null", None), ("0.5", 0.5), ("string", "hot")],
    "top_p": [("null", None), ("0.9", 0.9)],
    "top_k": [("null", None), ("5", 5), ("zero", 0)],
    "stop_sequences": [("null", None), ("list", ["X"]), ("string", "X")],
    "max_tokens": [("null", None), ("99", 99), ("zero", 0)],
    "thinking": [("null", None), ("enabled", {
        "type": "enabled"
    })],
    "stream": [("absent", ABSENT), ("true", True), ("false", False), ("null", None), ("text", "yes")],
    "metadata": [("user", {
        "user_id": "u"
    })],
}

RAW_BODIES = {
    "empty": b"",
    "truncated-object": b'{"messages": [',
    "list": b"[]",
    "null": b"null",
    "number": b"1",
    "string": b'"x"',
    "bom-object": b'\xef\xbb\xbf{"messages": []}',
    "invalid-utf8": b'{"messages": [{"role": "user", "content": "\xff"}]}',
    "trailing-data": b'{"messages": []} x',
    "trailing-comma": b'{"messages": [],}',
    "nan-temperature": b'{"messages": [], "temperature": NaN}',
    "messages-object": b'{"messages": {}}',
}

ANSWER_MESSAGE = {"role": "assistant", "content": "hi"}


def _reply(choice=None, **fields) -> bytes:
  """A chat completion with one choice; *choice* replaces the default choice, a missing key is absent."""
  default = {"message": dict(ANSWER_MESSAGE), "finish_reason": "stop"}
  if choice is not None:
    default = choice
  body = {"id": "chatcmpl-synthetic", "choices": [default]}
  body.update(fields)
  return json.dumps(body).encode()


def _tool_call(**changes) -> dict:
  call = {"id": "call_1", "type": "function", "function": {"name": "Bash", "arguments": '{"cmd":"pwd"}'}}
  for key, value in changes.items():
    if value is ABSENT:
      call.pop(key, None)
    elif key == "function":
      call["function"] = value
    else:
      call[key] = value
  return call


def _tool_reply(tool_calls, finish="tool_calls") -> bytes:
  message = {"role": "assistant", "content": None, "tool_calls": tool_calls}
  return _reply({"message": message, "finish_reason": finish})


ANSWERS = {
    "finish-stop":
        _reply(),
    "finish-length":
        _reply({
            "message": dict(ANSWER_MESSAGE),
            "finish_reason": "length"
        }),
    "finish-tool-calls":
        _reply({
            "message": dict(ANSWER_MESSAGE),
            "finish_reason": "tool_calls"
        }),
    "finish-content-filter":
        _reply({
            "message": dict(ANSWER_MESSAGE),
            "finish_reason": "content_filter"
        }),
    "finish-missing":
        _reply({"message": dict(ANSWER_MESSAGE)}),
    "finish-null":
        _reply({
            "message": dict(ANSWER_MESSAGE),
            "finish_reason": None
        }),
    "finish-unknown":
        _reply({
            "message": dict(ANSWER_MESSAGE),
            "finish_reason": "weird"
        }),
    "content-null":
        _reply({
            "message": {
                "role": "assistant",
                "content": None
            },
            "finish_reason": "stop"
        }),
    "content-empty":
        _reply({
            "message": {
                "role": "assistant",
                "content": ""
            },
            "finish_reason": "stop"
        }),
    "content-number":
        _reply({
            "message": {
                "role": "assistant",
                "content": 5
            },
            "finish_reason": "stop"
        }),
    "content-list":
        _reply({
            "message": {
                "role": "assistant",
                "content": ["x"]
            },
            "finish_reason": "stop"
        }),
    "message-missing":
        _reply({"finish_reason": "stop"}),
    "message-null":
        _reply({
            "message": None,
            "finish_reason": "stop"
        }),
    "message-string":
        _reply({
            "message": "x",
            "finish_reason": "stop"
        }),
    "message-empty":
        _reply({
            "message": {},
            "finish_reason": "stop"
        }),
    "tool-call-valid":
        _tool_reply([_tool_call()]),
    "tool-call-two":
        _tool_reply([_tool_call(), _tool_call(id="call_2")]),
    "tool-call-invalid-arguments":
        _tool_reply([_tool_call(function={
            "name": "Bash",
            "arguments": "{"
        })]),
    "tool-call-arguments-missing":
        _tool_reply([_tool_call(function={"name": "Bash"})]),
    "tool-call-arguments-object":
        _tool_reply([_tool_call(function={
            "name": "Bash",
            "arguments": {
                "cmd": "pwd"
            }
        })]),
    "tool-call-arguments-null":
        _tool_reply([_tool_call(function={
            "name": "Bash",
            "arguments": None
        })]),
    "tool-call-id-missing":
        _tool_reply([_tool_call(id=ABSENT)]),
    "tool-call-type-other":
        _tool_reply([_tool_call(type="other")]),
    "tool-call-name-missing":
        _tool_reply([_tool_call(function={"arguments": "{}"})]),
    "tool-calls-empty":
        _tool_reply([]),
    "tool-calls-string":
        _reply({
            "message": {
                "role": "assistant",
                "content": None,
                "tool_calls": "x"
            },
            "finish_reason": "tool_calls"
        }),
    "tool-calls-null":
        _reply({
            "message": {
                "role": "assistant",
                "content": None,
                "tool_calls": None
            },
            "finish_reason": "tool_calls"
        }),
    "choices-missing":
        json.dumps({
            "id": "chatcmpl-synthetic"
        }).encode(),
    "response-id-missing":
        json.dumps({
            "choices": [{
                "message": dict(ANSWER_MESSAGE),
                "finish_reason": "stop"
            }]
        }).encode(),
    "response-id-empty":
        _reply(id=""),
    "choices-empty":
        json.dumps({
            "choices": []
        }).encode(),
    "choices-string":
        json.dumps({
            "choices": "x"
        }).encode(),
    "choices-null":
        json.dumps({
            "choices": None
        }).encode(),
    "choices-two":
        json.dumps(
            {
                "choices":
                    [
                        {
                            "message": {
                                "role": "assistant",
                                "content": "a"
                            },
                            "finish_reason": "stop"
                        }, {
                            "message": {
                                "role": "assistant",
                                "content": "b"
                            }
                        }
                    ]
            }).encode(),
    "choices-without-message":
        json.dumps({
            "choices": [{
                "finish_reason": "stop"
            }]
        }).encode(),
    "choices-number":
        json.dumps({
            "choices": [5]
        }).encode(),
    "usage-null":
        _reply(usage=None),
    "usage-empty":
        _reply(usage={}),
    "usage-prompt-only":
        _reply(usage={"prompt_tokens": 3}),
    "usage-prompt-string":
        _reply(usage={
            "prompt_tokens": "x",
            "completion_tokens": 1
        }),
    "usage-completion-only":
        _reply(usage={"completion_tokens": 2}),
    "usage-list":
        _reply(usage=[1]),
    "usage-number":
        _reply(usage=5),
    "usage-cache-read":
        _reply(usage={
            "prompt_tokens": 1,
            "completion_tokens": 2,
            "cache_read_input_tokens": 3
        }),
    "body-not-json":
        b"not json",
    "body-empty":
        b"",
    "body-list":
        b"[]",
    "body-null":
        b"null",
    "body-bom":
        b"\xef\xbb\xbf" + JSON_REPLY,
    "body-not-utf8":
        b"\xff",
    "body-nan-content":
        b'{"choices": [{"message": {"content": NaN}, "finish_reason": "stop"}]}',
}

STREAM_CHUNKS = {
    "text-done":
        TEXT_STREAM,
    "crlf-split":
        CRLF_CHUNKS,
    "cr-only": [line.replace(b"\n", b"\r") for line in TEXT_STREAM],
    "raw-splitline-chars":
        SPLIT_LINE_CHUNKS,
    "two-tool-calls":
        [
            _data(
                _chunk(
                    choice={
                        "delta":
                            {
                                "tool_calls":
                                    [
                                        {
                                            "index": 0,
                                            "id": "call_1",
                                            "type": "function",
                                            "function": {
                                                "name": "Bash",
                                                "arguments": '{"cmd"'
                                            }
                                        }
                                    ]
                            }
                    })),
            _data(_chunk(choice={"delta": {
                "tool_calls": [{
                    "index": 0,
                    "function": {
                        "arguments": ':"pwd"}'
                    }
                }]
            }})),
            _data(
                _chunk(
                    choice={
                        "delta":
                            {
                                "tool_calls":
                                    [
                                        {
                                            "index": 1,
                                            "id": "call_2",
                                            "type": "function",
                                            "function": {
                                                "name": "Read",
                                                "arguments": "{}"
                                            }
                                        }
                                    ]
                            },
                        "finish_reason": "tool_calls"
                    },
                    usage={
                        "prompt_tokens": 2,
                        "completion_tokens": 4
                    })),
            DONE,
        ],
    "tool-call-without-id":
        [_data(_chunk(choice={"delta": {
            "tool_calls": [{
                "index": 0,
                "function": {
                    "name": "Bash"
                }
            }]
        }}))],
    "tool-call-without-index":
        [_data(_chunk(choice={"delta": {
            "tool_calls": [{
                "id": "c",
                "function": {
                    "name": "B"
                }
            }]
        }}))],
    "no-done":
        TEXT_STREAM[:2],
    "noise-lines": [
        b"event: message\n",
        b": comment\n",
        b"\n",
        b"data:\n",
        TEXT_STREAM[0],
        b"data:  [DONE]  \n",
    ],
    "decode-error": [TEXT_STREAM[0], b"data: {not json}\n\n", DONE],
    "chunk-not-object": [b"data: 5\n\n", DONE],
    "choices-null": [b'data: {"choices": null}\n\n', DONE],
    "empty-content-and-role": [_data({"choices": [{
        "delta": {
            "role": "assistant",
            "content": ""
        }
    }]}), DONE],
    "usage-then-no-finish": [_data(_chunk("x", usage={
        "prompt_tokens": 1,
        "completion_tokens": 1
    })), DONE],
    "data-after-done-ignored": [TEXT_STREAM[0], DONE, b"data: {bad json\n\n"],
    "reasoning-only": [_data({"choices": [{
        "delta": {
            "reasoning_content": "thinking"
        }
    }]}), DONE],
    "tool-arguments-absent":
        [_data(_chunk(choice={"delta": {
            "tool_calls": [{
                "index": 0,
                "id": "c",
                "function": {
                    "name": "B"
                }
            }]
        }})), DONE],
    "finish-length": [_data(_chunk("cut", choice={"finish_reason": "length"})), DONE],
    "text-then-tool":
        [
            _data(_chunk("Checking")),
            _data(
                _chunk(
                    choice={
                        "delta":
                            {
                                "tool_calls":
                                    [
                                        {
                                            "index": 0,
                                            "id": "c",
                                            "type": "function",
                                            "function": {
                                                "name": "Bash",
                                                "arguments": "{}"
                                            }
                                        }
                                    ]
                            },
                        "finish_reason": "tool_calls"
                    })),
            DONE,
        ],
    "stream-split-utf8":
        UTF8_SPLIT_CHUNKS,
    "blank-data-line": [b"data: \r\n", TEXT_STREAM[0], DONE],
    "tool-index-string":
        [
            _data(
                _chunk(
                    choice={
                        "delta":
                            {
                                "tool_calls": [{
                                    "index": "0",
                                    "id": "c",
                                    "function": {
                                        "name": "B",
                                        "arguments": "{}"
                                    }
                                }]
                            }
                    })), DONE
        ],
}


def _case(name, body, chunks, status=200, credential=None, unreachable=False) -> dict:
  return {
      "name": name,
      "body": body,
      "status": status,
      "chunks": chunks,
      "credential": credential,
      "unreachable": unreachable,
  }


def _with(**fields) -> dict:
  payload = {"model": "claude-facing", "messages": [{"role": "user", "content": "hi"}], "max_tokens": 16}
  payload.update({key: value for key, value in fields.items() if value is not ABSENT})
  return payload


def _request(name, payload, **overrides) -> dict:
  reply = TEXT_STREAM if payload.get("stream") else [JSON_REPLY]
  return _case(name, json.dumps(payload).encode(), reply, **overrides)


def build_cases() -> list[dict]:
  cases = []
  for system_label, system in SYSTEMS.items():
    for messages_label, messages in MESSAGES.items():
      cases.append(
          _request(f"system:{system_label} messages:{messages_label}", _with(system=system, messages=messages)))
  for tools_label, tools in TOOLS.items():
    for choice_label, choice in TOOL_CHOICES.items():
      cases.append(_request(f"tools:{tools_label} tool_choice:{choice_label}", _with(tools=tools, tool_choice=choice)))
  for tools_label, tools in (("absent", ABSENT), ("one", ONE_TOOL)):
    for choice_label, choice in OBJECT_CHOICES.items():
      for dpt_label, dpt in DISABLE_PARALLEL.items():
        tool_choice = dict(choice)
        if dpt is not ABSENT:
          tool_choice["disable_parallel_tool_use"] = dpt
        cases.append(
            _request(
                f"tools:{tools_label} tool_choice:{choice_label} disable_parallel:{dpt_label}",
                _with(tools=tools, tool_choice=tool_choice)))
  for key, values in OPTIONAL.items():
    for label, value in values:
      cases.append(_request(f"{key}:{label}", _with(**{key: value})))
  for messages_label, messages in MESSAGES.items():
    cases.append(_request(f"stream messages:{messages_label}", _with(messages=messages, stream=True)))
  for label, body in RAW_BODIES.items():
    cases.append(_case(f"raw:{label}", body, [JSON_REPLY]))
  for label, reply in ANSWERS.items():
    cases.append(_case(f"answer:{label}", json.dumps(_with()).encode(), [reply]))
  stream_body = json.dumps(_with(stream=True)).encode()
  for label, chunks in STREAM_CHUNKS.items():
    cases.append(_case(f"stream:{label}", stream_body, chunks))
  cases.append(_case("stream:unreachable", stream_body, TEXT_STREAM, unreachable=True))
  cases.append(_case("stream:upstream-429", stream_body, [b'{"error": "slow"}'], status=429))
  cases.append(_case("stream:upstream-500-text", stream_body, [b"boom"], status=500))
  cases.append(_case("stream:upstream-bad-utf8", stream_body, [b"\xff\xfe bad"], status=502))
  cases.append(_case("request:unreachable", json.dumps(_with()).encode(), [JSON_REPLY], unreachable=True))
  for credential in ("present", "missing"):
    cases.append(_request(f"credential:{credential} request", _with(), credential=credential))
    cases.append(_request(f"credential:{credential} stream", _with(stream=True), credential=credential))
  return cases


CASES = build_cases()
SEEN: dict[int, dict] = {}
CURRENT = {"index": None}


class _Chunks(httpx.AsyncByteStream):
  """An upstream body that yields the case's chunks as they are, so a frame can split across them."""

  def __init__(self, chunks: list[bytes]) -> None:
    self._chunks = chunks

  async def __aiter__(self) -> AsyncIterator[bytes]:
    for chunk in self._chunks:
      yield chunk


def _upstream(request: httpx.Request) -> httpx.Response:
  index = int(request.url.path.split("/")[1])
  case = CASES[index]
  if case["unreachable"]:
    raise httpx.ConnectError("connection refused", request=request)
  SEEN[index] = {"body_hex": request.content.hex(), "authorization": request.headers.get("authorization")}
  return httpx.Response(case["status"], stream=_Chunks(case["chunks"]))


def _credentials() -> credentials.Credentials:
  index = CURRENT["index"]
  sections = {"charliebot": {"access_key": ACCESS_KEY}}
  if CASES[index]["credential"] == "present":
    sections[f"case_{index}"] = {"api_key": f"key-{index}"}
  return credentials.Credentials(path=CREDENTIALS_PATH, sections=sections)


def _option(index: int, case: dict) -> models.BackendOption:
  fields = {
      "id": f"case-{index}",
      "label": f"case {index}",
      "type": "cc-openai-compatible",
      "model": MODEL,
      "api_base": f"http://upstream.example/{index}/v1",
  }
  if case["credential"] is not None:
    fields["credential"] = f"case_{index}"
  return backend_models.parse_option(fields)


def _normalize(body: bytes) -> bytes:
  body = body.replace(str(CREDENTIALS_PATH).encode(), b"<credentials>")
  return MESSAGE_ID.sub(b"msg_MASKED", body)


async def _stream_frames(case: dict) -> tuple[bytes, str | None]:
  """The frames the route's generator yields for the case's upstream, and the exception that ended it, if any.

  TestClient drops the body of a stream that raised, but a server has already sent the frames yielded before the
  failure when it closes the connection. So a stream's bytes come from the generator the route streams.
  """
  upstream = httpx.Response(case["status"], stream=_Chunks(case["chunks"]))
  frames = []
  try:
    async for frame in anthropic_proxy._iter_anthropic_sse(upstream, MODEL):
      frames.append(frame)
  except Exception as error:
    return b"".join(frames), type(error).__name__
  return b"".join(frames), None


def main() -> None:
  openai_compatible.register()
  app = fastapi.FastAPI()
  app.include_router(anthropic_proxy.router, prefix="/api/anthropic-proxy")
  cfg = config.CharlieBotConfig(
      server={"port": 8123}, backends={"options": [_option(i, case) for i, case in enumerate(CASES)]})
  app.dependency_overrides[deps.get_config_on_loop] = lambda: cfg
  config.get_credentials = _credentials
  http.get_http_client = lambda: httpx.AsyncClient(transport=httpx.MockTransport(_upstream))
  client = testclient.TestClient(app, raise_server_exceptions=False)
  for index, case in enumerate(CASES):
    SEEN.pop(index, None)
    CURRENT["index"] = index
    response = client.post(f"/api/anthropic-proxy/openai-compatible/case-{index}/v1/messages", content=case["body"])
    body = response.content
    failure = None
    if response.headers.get("content-type", "").startswith("text/event-stream"):
      frames, failure = asyncio.run(_stream_frames(case))
      if failure is None:
        assert _normalize(frames) == _normalize(body), f"{case['name']}: TestClient and the generator disagree"
      body = frames
    record = {
        "name": case["name"],
        "input":
            {
                "index": index,
                "body_hex": case["body"].hex(),
                "status": case["status"],
                "chunks_hex": [chunk.hex() for chunk in case["chunks"]],
                "credential": case["credential"],
                "unreachable": case["unreachable"],
            },
        "expected":
            {
                "status": response.status_code,
                "body_hex": _normalize(body).hex(),
                "upstream": SEEN.get(index),
                "complete": body.endswith(MESSAGE_STOP),
                "failure": failure,
            },
    }
    sys.stdout.write(json.dumps(record) + "\n")


if __name__ == "__main__":
  main()
