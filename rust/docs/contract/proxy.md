# Anthropic proxy

A backend of type `cc-openai-compatible` runs the Claude Code CLI against a model server that speaks the OpenAI chat completions API. The CLI talks to this server's proxy route, which translates each Anthropic Messages request to OpenAI and the answer back.

The route lives in `src/backends/openai_compatible/anthropic_proxy.py`, mounted at `/api/anthropic-proxy`.

## Caller

`src/backends/openai_compatible/factory.py` builds the backend. The CLI process gets:

- `ANTHROPIC_BASE_URL`: `<server_base_url>/api/anthropic-proxy/openai-compatible/<backend id>`;
- `ANTHROPIC_AUTH_TOKEN`: the access key (`credentials.charliebot.access_key`).

The CLI sends `Authorization: Bearer <access key>`, which passes the auth middleware.

## `POST /api/anthropic-proxy/openai-compatible/{backend_id}/v1/messages`

- Auth: access key.
- Handler: `src/backends/openai_compatible/anthropic_proxy.py:605`.
- Path parameter: `backend_id`, a backend option id.
- Body: an Anthropic Messages API request (JSON object).

### Backend checks

| Status | Case | `detail` |
|---|---|---|
| 404 | No backend option with that id | `unknown backend id: <backend_id>` |
| 400 | The option type is not `cc-openai-compatible` | `backend '<backend_id>' is not type 'cc-openai-compatible' (got '<type>')` |
| 400 | The option has no `api_base` | `backend '<backend_id>' missing api_base` |
| 400 | The option has no `model` | `backend '<backend_id>' missing model` |

### Request translation

Any translation error gives 400 with the error text as `detail`. A body that is not JSON gives 400 with the JSON decoder message. A JSON body that is not an object raises `AttributeError`, which gives 500 `Internal Server Error`.

The OpenAI request:

| OpenAI key | From | Rule |
|---|---|---|
| `model` | the option's `model` | The request `model` is ignored. |
| `messages` | `system` and `messages` | See below. `messages` must be a list, else `messages must be a list`. |
| `stream` | `stream` | `bool(...)`, default false. |
| `max_tokens` | `max_tokens` | Only when not null. |
| `temperature` | `temperature` | Only when not null. |
| `top_p` | `top_p` | Only when not null. |
| `stop` | `stop_sequences` | Only when not null. |
| `tools` | `tools` | See below. |
| `tool_choice` | `tool_choice` | `auto`→`"auto"`, `any`→`"required"`, `none`→`"none"`, `tool`→`{"type":"function","function":{"name":…}}`. |
| `parallel_tool_calls` | `tool_choice.disable_parallel_tool_use` | The negation, only when set. |
| `stream_options` | | `{"include_usage": true}` when streaming. |

Refused request keys:

- `thinking` (not null): `OpenAI-compatible proxy does not support Anthropic thinking blocks`;
- `top_k` (not null): `OpenAI-compatible proxy does not support Anthropic top_k`.

Other request keys are dropped.

Messages:

1. A non-empty `system` becomes the first message, role `system`. It must be a string or a list of `text` blocks. The block texts are joined with no separator.
2. A `user` message with string content stays one `user` message.
3. A `user` message with block content splits in block order:
   - consecutive `text` blocks join into one `user` message;
   - each `tool_result` block becomes `{"role":"tool","tool_call_id":<tool_use_id>,"content":<text>}`. Its content must be a string or `text` blocks.
4. An `assistant` message becomes one message. The `text` blocks join into `content` (null when empty). Each `tool_use` block becomes a `tool_calls` entry `{"id","type":"function","function":{"name","arguments":<compact JSON of input>}}`.
5. Another role: `unsupported message role: <role>`.

Translation error texts:

- `<label> must be an object`
- `<label> must be a string or content block list`
- `unsupported <label> block type: <type>`
- `user message content must be a string or content block list`
- `tool_result block missing tool_use_id`
- `unsupported user content block type: <type>`
- `assistant message content must be a string or content block list`
- `tool_use block missing id or name`
- `unsupported assistant content block type: <type>`
- `unsupported Anthropic built-in tool type: <type>`
- `tool missing name or input_schema`
- `tool_choice type 'tool' missing name`
- `unsupported tool_choice type: <type>`
- `api_base not set for backend`

Tools: each tool must have no `type`. It becomes `{"type":"function","function":{"name","description" (default ""),"parameters":<input_schema>}}`.

Upstream request:

- `POST <api_base without trailing slash>/chat/completions`;
- JSON body;
- header `Content-Type: application/json`;
- header `Authorization: Bearer <credentials.<credential>.api_key>` when the option names a `credential`. A missing key gives 400 with the credentials loader error;
- no timeout.

### Upstream errors

An upstream status of 400 or above passes through: the response has the upstream status and `{"detail": "<upstream body as text>"}`.

A network error reaching upstream gives 500 `Internal Server Error`.

### Non-streaming response

200, a Starlette `JSONResponse`:

```json
{
  "id": "<upstream id, or msg_<32 hex>>",
  "type": "message",
  "role": "assistant",
  "model": "<upstream model, or the option model>",
  "content": [{"type": "text", "text": "…"}, {"type": "tool_use", "id": "…", "name": "…", "input": {}}],
  "stop_reason": "end_turn",
  "stop_sequence": null,
  "usage": {"input_tokens": 10, "output_tokens": 5, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}
}
```

- The proxy uses the first choice only.
- `content` holds one `text` block when the message content is non-empty, then one `tool_use` block per tool call. `input` is the parsed `arguments` (`{}` when empty).
- `stop_reason`: `tool_calls`→`tool_use`, `length`→`max_tokens`, null or `stop`→`end_turn`, another value as given.
- `usage`: `prompt_tokens` and `completion_tokens`, default 0. The cache counts are always 0.

502 `{"detail": "<error>"}` when the upstream JSON cannot be translated:

- `OpenAI response missing choices`;
- `OpenAI tool_call missing id or function.name`;
- an upstream body or tool arguments that are not JSON.

### Streaming response

200, `text/event-stream`, a `StreamingResponse`. The transport gzip middleware compresses it when the client accepts gzip.

Each event is `event: <name>\ndata: <compact JSON>\n\n`. The JSON starts with `"type": "<name>"`.

Event order:

1. `message_start` with `message` `{"id":"msg_<32 hex>","type":"message","role":"assistant","content":[],"model":<option model>,"stop_reason":null,"stop_sequence":null,"usage":<zero usage>}`.
2. For each upstream `data:` line, until `data: [DONE]`:
   - text delta: `content_block_start` (type `text`) when no text block is open, then `content_block_delta` with `{"type":"text_delta","text":…}`. Opening a text block closes the open tool blocks.
   - first delta of a tool call: close the open text block, then `content_block_start` with `{"type":"tool_use","id","name","input":{}}`.
   - tool arguments: `content_block_delta` with `{"type":"input_json_delta","partial_json":…}`.
   - The upstream `usage` and `finish_reason` are kept for the end.
3. `content_block_stop` for the open text block, then for each open tool block.
4. `message_delta` with `{"delta":{"stop_reason","stop_sequence":null},"usage":<usage>}`.
5. `message_stop` with `{}`.

Block indexes count from 0 in opening order.

A translation error inside the stream raises after the headers are sent. The response ends without `message_stop`.
