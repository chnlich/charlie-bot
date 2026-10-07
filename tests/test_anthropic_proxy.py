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
