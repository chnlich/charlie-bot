import json

import conftest
import pytest

from src.backends.openai_compatible import anthropic_proxy


def test_anthropic_request_translates_text_tools_and_tool_results_to_openai() -> None:
  payload = {
      "model": "synthetic-provider/synthetic-vendor/Synthetic-Pro",
      "system": [{
          "type": "text",
          "text": "Use tools carefully."
      }],
      "messages":
          [
              {
                  "role": "user",
                  "content": "List files.",
              },
              {
                  "role":
                      "assistant",
                  "content":
                      [
                          {
                              "type": "text",
                              "text": "I will inspect the directory.",
                          },
                          {
                              "type": "tool_use",
                              "id": "toolu_1",
                              "name": "Bash",
                              "input": {
                                  "cmd": "ls",
                              },
                          },
                      ],
              },
              {
                  "role":
                      "user",
                  "content":
                      [
                          {
                              "type": "tool_result",
                              "tool_use_id": "toolu_1",
                              "content": [{
                                  "type": "text",
                                  "text": "README.md"
                              }],
                          },
                          {
                              "type": "text",
                              "text": "Summarize.",
                          },
                      ],
              },
          ],
      "max_tokens": 1024,
      "temperature": 0.1,
      "stop_sequences": ["</stop>"],
      "tools":
          [
              {
                  "name": "Bash",
                  "description": "Run a shell command",
                  "input_schema": {
                      "type": "object",
                      "properties": {
                          "cmd": {
                              "type": "string",
                          },
                      },
                      "required": ["cmd"],
                  },
              }
          ],
      "tool_choice": {
          "type": "any",
          "disable_parallel_tool_use": True,
      },
      "stream": True,
  }

  converted = anthropic_proxy.anthropic_to_openai_chat_request(payload, upstream_model="deepseek-ai/DeepSeek-V4-Pro")

  assert converted["model"] == "deepseek-ai/DeepSeek-V4-Pro"
  assert converted["stream"] is True
  assert converted["stream_options"] == {"include_usage": True}
  assert converted["max_tokens"] == 1024
  assert converted["temperature"] == 0.1
  assert converted["stop"] == ["</stop>"]
  assert converted["tool_choice"] == "required"
  assert converted["parallel_tool_calls"] is False
  assert converted["messages"] == [
      {
          "role": "system",
          "content": "Use tools carefully.",
      },
      {
          "role": "user",
          "content": "List files.",
      },
      {
          "role":
              "assistant",
          "content":
              "I will inspect the directory.",
          "tool_calls":
              [{
                  "id": "toolu_1",
                  "type": "function",
                  "function": {
                      "name": "Bash",
                      "arguments": '{"cmd":"ls"}',
                  },
              }],
      },
      {
          "role": "tool",
          "tool_call_id": "toolu_1",
          "content": "README.md",
      },
      {
          "role": "user",
          "content": "Summarize.",
      },
  ]
  assert converted["tools"] == [
      {
          "type": "function",
          "function":
              {
                  "name": "Bash",
                  "description": "Run a shell command",
                  "parameters": {
                      "type": "object",
                      "properties": {
                          "cmd": {
                              "type": "string",
                          },
                      },
                      "required": ["cmd"],
                  },
              },
      }
  ]


def test_stream_translator_emits_anthropic_text_and_tool_events() -> None:
  translator = anthropic_proxy.OpenAIChatStreamToAnthropic("deepseek-v4-pro")

  events = translator.start_events()
  events += translator.events_for_chunk({
      "choices": [{
          "delta": {
              "content": "Checking",
          },
      }],
  })
  events += translator.events_for_chunk(
      {
          "choices":
              [
                  {
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
                                              "arguments": '{"cmd"',
                                          },
                                      }
                                  ],
                          },
                  }
              ],
      })
  events += translator.events_for_chunk(
      {
          "choices":
              [
                  {
                      "delta": {
                          "tool_calls": [{
                              "index": 0,
                              "function": {
                                  "arguments": ':"pwd"}',
                              },
                          }],
                      },
                      "finish_reason": "tool_calls",
                  }
              ],
          "usage": {
              "prompt_tokens": 8,
              "completion_tokens": 3,
          },
      })
  events += translator.finish_events()

  names = [name for name, _data in events]
  assert names == [
      "message_start",
      "content_block_start",
      "content_block_delta",
      "content_block_stop",
      "content_block_start",
      "content_block_delta",
      "content_block_delta",
      "content_block_stop",
      "message_delta",
      "message_stop",
  ]
  assert events[1][1]["content_block"] == {"type": "text", "text": ""}
  assert events[2][1]["delta"] == {"type": "text_delta", "text": "Checking"}
  assert events[4][1]["content_block"] == {
      "type": "tool_use",
      "id": "call_1",
      "name": "Bash",
      "input": {},
  }
  assert events[5][1]["delta"] == {"type": "input_json_delta", "partial_json": '{"cmd"'}
  assert events[6][1]["delta"] == {"type": "input_json_delta", "partial_json": ':"pwd"}'}
  assert events[-2][1]["delta"]["stop_reason"] == "tool_use"
  assert events[-2][1]["usage"]["input_tokens"] == 8
  assert events[-2][1]["usage"]["output_tokens"] == 3


@pytest.mark.asyncio
async def test_iter_anthropic_sse_translates_frame_with_raw_splitline_chars() -> None:
  """Same regression class as the opencode SSE bug: an upstream chunk whose JSON
  string carries raw U+0085/U+2028 must be translated, not dropped."""
  nel = "\x85"
  ls = "\u2028"
  content = "grep hit: a:48<" + nel + ">NEL-CHAR" + ls + ">LS-CHAR"
  payload = json.dumps({"choices": [{"delta": {"content": content}}]}, ensure_ascii=False)
  raw = ("data: " + payload + "\n\n" + "data: [DONE]\n\n").encode("utf-8")
  cut_nel = raw.index(nel.encode("utf-8")) + 1
  cut_ls = raw.index(ls.encode("utf-8"))
  chunks = [raw[:cut_nel], raw[cut_nel:cut_ls], raw[cut_ls:]]

  blobs = [
      blob async for blob in anthropic_proxy._iter_anthropic_sse(conftest.FakeChunkedResponse(chunks), "test-model")
  ]

  parsed = []
  for blob in blobs:
    lines = blob.decode("utf-8").split("\n")
    event = lines[0][len("event: "):]
    data = json.loads(lines[1][len("data: "):])
    assert event == data["type"], "Anthropic streaming pins data.type to the SSE event line's name"
    parsed.append((event, data))
  text = "".join(
      data["delta"]["text"]
      for event, data in parsed
      if event == "content_block_delta" and data["delta"]["type"] == "text_delta")
  assert text == content
  assert parsed[-1][0] == "message_stop"


def test_stream_tool_call_without_index_is_rejected() -> None:
  translator = anthropic_proxy.OpenAIChatStreamToAnthropic("gemini-3.8-flash")

  with pytest.raises(ValueError, match="streaming tool_call missing index"):
    translator.events_for_chunk(
        {
            "choices":
                [{
                    "delta": {
                        "tool_calls": [{
                            "id": "call-1",
                            "function": {
                                "name": "Bash",
                                "arguments": "{}",
                            },
                        }],
                    },
                }],
        })


def test_gemini_stream_index_less_delta_with_signature_yields_one_tool_block() -> None:
  """The 2026-10-23 Gemini probe shape: one whole tool call in one delta, no index."""
  translator = anthropic_proxy.OpenAIChatStreamToAnthropic("gemini-3.8-flash", gemini_stream_rules=True)
  events = translator.start_events()
  events.extend(
      translator.events_for_chunk(
          {
              "choices":
                  [
                      {
                          "delta":
                              {
                                  "tool_calls":
                                      [
                                          {
                                              "extra_content": {
                                                  "google": {
                                                      "thought_signature": "EqMDCqAD...",
                                                  },
                                              },
                                              "function":
                                                  {
                                                      "arguments":
                                                          '{"description":"Print working directory","command":"pwd"}',
                                                      "name":
                                                          "Bash",
                                                  },
                                              "id": "call_371733",
                                              "type": "function",
                                          }
                                      ],
                              },
                      }
                  ],
          }))
  events.extend(translator.finish_events())

  tool_starts = [
      data["content_block"]
      for event, data in events
      if event == "content_block_start" and data["content_block"]["type"] == "tool_use"
  ]
  argument_fragments = [
      data["delta"]["partial_json"]
      for event, data in events
      if event == "content_block_delta" and data["delta"]["type"] == "input_json_delta"
  ]
  assert tool_starts == [
      {
          "type": "tool_use",
          "id": "call_371733",
          "name": "Bash",
          "input": {},
          "extra_content": {
              "google": {
                  "thought_signature": "EqMDCqAD...",
              },
          },
      }
  ]
  assert argument_fragments == ['{"description":"Print working directory","command":"pwd"}']


def test_gemini_stream_two_index_less_calls_split_by_id() -> None:
  translator = anthropic_proxy.OpenAIChatStreamToAnthropic("gemini-3.8-flash", gemini_stream_rules=True)
  events = translator.start_events()
  events.extend(
      translator.events_for_chunk(
          {
              "choices":
                  [
                      {
                          "delta":
                              {
                                  "tool_calls":
                                      [{
                                          "id": "call-a",
                                          "function": {
                                              "name": "Bash",
                                              "arguments": '{"cmd":',
                                          },
                                      },],
                              },
                      }
                  ],
          }))
  events.extend(
      translator.events_for_chunk(
          {
              "choices": [{
                  "delta": {
                      "tool_calls": [{
                          "function": {
                              "arguments": '"pwd"}',
                          },
                      }],
                  },
              }],
          }))
  events.extend(
      translator.events_for_chunk(
          {
              "choices":
                  [
                      {
                          "delta":
                              {
                                  "tool_calls":
                                      [
                                          {
                                              "id": "call-b",
                                              "function": {
                                                  "name": "Read",
                                                  "arguments": '{"file_path":"image.png"}',
                                              },
                                          },
                                      ],
                              },
                      }
                  ],
          }))
  events.extend(translator.finish_events())

  tool_starts = [
      data["content_block"]
      for event, data in events
      if event == "content_block_start" and data["content_block"]["type"] == "tool_use"
  ]
  argument_fragments = [
      data["delta"]["partial_json"]
      for event, data in events
      if event == "content_block_delta" and data["delta"]["type"] == "input_json_delta"
  ]
  assert tool_starts == [
      {
          "type": "tool_use",
          "id": "call-a",
          "name": "Bash",
          "input": {},
      },
      {
          "type": "tool_use",
          "id": "call-b",
          "name": "Read",
          "input": {},
      },
  ]
  assert argument_fragments == ['{"cmd":', '"pwd"}', '{"file_path":"image.png"}']


def test_claude_code_request_shape_preserves_input_schema() -> None:
  payload = {
      "model": "gemini-3.8-flash",
      "system": [{
          "type": "text",
          "text": "Use the available tools.",
      }],
      "messages":
          [
              {
                  "role":
                      "user",
                  "content":
                      [
                          {
                              "type": "text",
                              "text": "Attribution reminder.",
                          },
                          {
                              "type": "text",
                              "text": "Run pwd with the Bash tool.",
                          },
                      ],
              },
              {
                  "role": "system",
                  "content": [{
                      "type": "text",
                      "text": "# Environment\nPrimary working directory: /tmp/run",
                  }],
              },
          ],
      "thinking": {
          "type": "adaptive",
      },
      "output_config": {
          "effort": "medium",
      },
      "tools":
          [
              {
                  "name": "Bash",
                  "description": "Run a shell command",
                  "input_schema":
                      {
                          "$schema": "https://json-schema.org/draft/2020-12/schema",
                          "type": "object",
                          "additionalProperties": False,
                          "properties":
                              {
                                  "cmd": {
                                      "type": "string",
                                      "const": "pwd",
                                  },
                                  "count": {
                                      "type": "number",
                                      "exclusiveMinimum": 0,
                                  },
                                  "choice": {
                                      "type": "string",
                                      "allOf": [{
                                          "type": "string",
                                      }],
                                  },
                              },
                          "required": ["cmd"],
                      },
              }
          ],
      "stream": True,
  }

  converted = anthropic_proxy.anthropic_to_openai_chat_request(payload, upstream_model="gemini-3.8-flash")

  assert converted["messages"][0] == {
      "role": "system",
      "content": "Use the available tools.\n\n# Environment\nPrimary working directory: /tmp/run",
  }
  assert converted["messages"][1] == {
      "role": "user",
      "content": "Attribution reminder.Run pwd with the Bash tool.",
  }
  assert "thinking" not in converted
  assert "output_config" not in converted
  assert converted["tools"][0]["function"]["parameters"] is payload["tools"][0]["input_schema"]


def test_stream_tool_signatures_round_trip_for_calls_sharing_an_index() -> None:
  translator = anthropic_proxy.OpenAIChatStreamToAnthropic("gemini-3.8-flash", gemini_stream_rules=True)
  events = translator.start_events()
  events.extend(
      translator.events_for_chunk(
          {
              "choices":
                  [
                      {
                          "delta":
                              {
                                  "tool_calls":
                                      [
                                          {
                                              "index": 0,
                                              "id": "call-a",
                                              "type": "function",
                                              "function": {
                                                  "name": "Bash",
                                                  "arguments": '{"cmd":',
                                              },
                                          }
                                      ],
                              },
                      }
                  ],
          }))
  events.extend(
      translator.events_for_chunk(
          {
              "choices":
                  [
                      {
                          "delta":
                              {
                                  "tool_calls":
                                      [
                                          {
                                              "index": 0,
                                              "extra_content": {
                                                  "google": {
                                                      "thought_signature": "SIG-A",
                                                  },
                                              },
                                              "function": {
                                                  "arguments": '"pwd"}',
                                              },
                                          },
                                          {
                                              "index": 0,
                                              "id": "call-b",
                                              "type": "function",
                                              "extra_content": {
                                                  "google": {
                                                      "thought_signature": "SIG-B",
                                                  },
                                              },
                                              "function": {
                                                  "name": "Read",
                                                  "arguments": '{"file_path":"image.png"}',
                                              },
                                          },
                                      ],
                              },
                      }
                  ],
          }))
  events.extend(translator.finish_events())

  tool_blocks_by_index: dict[int, dict] = {}
  partial_json_by_index: dict[int, list[str]] = {}
  for event, data in events:
    if event == "content_block_start" and data["content_block"]["type"] == "tool_use":
      index = data["index"]
      tool_blocks_by_index[index] = dict(data["content_block"])
      partial_json_by_index[index] = []
    elif event == "content_block_delta" and data["delta"]["type"] == "input_json_delta":
      partial_json_by_index[data["index"]].append(data["delta"]["partial_json"])

  tool_blocks = []
  for index, tool_block in tool_blocks_by_index.items():
    tool_block["input"] = json.loads("".join(partial_json_by_index[index]))
    tool_blocks.append(tool_block)
  converted = anthropic_proxy.anthropic_to_openai_chat_request(
      {
          "messages": [{
              "role": "assistant",
              "content": tool_blocks,
          }],
      },
      upstream_model="gemini-3.8-flash",
  )

  tool_calls = converted["messages"][0]["tool_calls"]
  assert [tool_call["id"] for tool_call in tool_calls] == ["call-a", "call-b"]
  assert [tool_call["function"]["name"] for tool_call in tool_calls] == ["Bash", "Read"]
  assert [json.loads(tool_call["function"]["arguments"]) for tool_call in tool_calls] == [
      {
          "cmd": "pwd"
      },
      {
          "file_path": "image.png"
      },
  ]
  assert [tool_call["extra_content"]["google"]["thought_signature"] for tool_call in tool_calls] == ["SIG-A", "SIG-B"]


def test_non_stream_tool_signatures_round_trip_to_next_request() -> None:
  upstream_response = {
      "choices":
          [
              {
                  "finish_reason": "tool_calls",
                  "message":
                      {
                          "tool_calls":
                              [
                                  {
                                      "id": "call-1",
                                      "type": "function",
                                      "function": {
                                          "name": "Bash",
                                          "arguments": '{"cmd":"pwd"}',
                                      },
                                      "extra_content": {
                                          "google": {
                                              "thought_signature": "SIG-1",
                                          },
                                      },
                                  }
                              ],
                      },
              }
          ],
  }

  anthropic_response = anthropic_proxy.openai_chat_response_to_anthropic(upstream_response, "gemini-3.8-flash")
  converted_request = anthropic_proxy.anthropic_to_openai_chat_request(
      {
          "messages": [{
              "role": "assistant",
              "content": anthropic_response["content"],
          }],
      },
      upstream_model="gemini-3.8-flash",
  )

  assert converted_request["messages"][0]["tool_calls"][0]["extra_content"] == {
      "google": {
          "thought_signature": "SIG-1",
      },
  }


def test_user_and_tool_result_images_convert_to_openai_image_url_parts() -> None:
  image_block = {
      "type": "image",
      "source": {
          "type": "base64",
          "media_type": "image/png",
          "data": "AQID",
      },
  }
  image_part = {
      "type": "image_url",
      "image_url": {
          "url": "data:image/png;base64,AQID",
      },
  }

  user_request = anthropic_proxy.anthropic_to_openai_chat_request(
      {
          "messages":
              [{
                  "role": "user",
                  "content": [{
                      "type": "text",
                      "text": "What is in this image?",
                  }, image_block],
              }],
      },
      upstream_model="gemini-3.8-flash",
  )
  tool_result_request = anthropic_proxy.anthropic_to_openai_chat_request(
      {
          "messages":
              [
                  {
                      "role":
                          "user",
                      "content":
                          [
                              {
                                  "type": "tool_result",
                                  "tool_use_id": "call-1",
                                  "content": [{
                                      "type": "text",
                                      "text": "Image loaded.",
                                  }, image_block],
                              }
                          ],
                  }
              ],
      },
      upstream_model="gemini-3.8-flash",
  )

  assert user_request["messages"][0]["content"] == [
      {
          "type": "text",
          "text": "What is in this image?",
      },
      image_part,
  ]
  assert tool_result_request["messages"] == [
      {
          "role": "tool",
          "tool_call_id": "call-1",
          "content": "Image loaded.",
      },
      {
          "role": "user",
          "content": [image_part],
      },
  ]
