# Replies

The reply routes let an agent session talk to its outside audience. Three packages own them:

- Slack thread sessions: `src/features/slack/api.py`;
- Discord thread sessions: `src/features/discord/api.py`;
- web sessions (the browser user): `src/features/web_reply/api.py`.

All routes sit under `/api/internal/`. The CLI calls them (`charliebot slack …`, `charliebot discord …`, `charliebot web …`).

Shared rules:

- Auth: access key. No route calls `require_caller`. The session is the body or query `session_id`.
- Bodies are JSON. Unknown keys give 422 `extra_forbidden`.
- A refusal persists nothing.

## Thread sessions (Slack and Discord)

The Slack and Discord routes share the logic in `src/features/chat_threads/thread_entry.py`. The platform values differ:

| Value | Slack | Discord |
|---|---|---|
| Message id key (`<id>` below) | `ts` | `id` |
| Origin slot key | `slack_origin` | `discord_origin` |
| Watermark slot key | `slack_watermark_ts` | `discord_watermark_id` |
| Reply event type | `slack_reply` | `discord_reply` |
| Ack event type | `slack_ack` | `discord_ack` |
| Maximum characters per post | 40000 | 2000 |
| Display name | `Slack` | `Discord` |

### Thread session check

1. Unknown session: 404 `Session not found`.
2. The session has no origin slot value: 409 `Session has no <Display name> thread`.

### Freshness check (reply routes only)

The reply route reads the thread's eligible messages above the session's watermark. When any exist, it refuses with 412:

```json
{"detail": {"error": "stale_thread", "new_messages": [{"<id>": "…", "user": "…", "text_preview": "<first 200 chars>"}], "watermark_<id>": "<watermark or null>"}}
```

The thread read calls the platform API (Slack Web API or Discord REST API).

### `POST /api/internal/slack/reply` and `POST /api/internal/discord/reply`

- Handlers: `src/features/slack/api.py:30`, `src/features/discord/api.py:37`.
- Body: `{"session_id": string, "text": string}`, both required.

Checks after the freshness check:

| Status | Case | `detail` |
|---|---|---|
| 422 | `text` is blank | `Reply text is empty` |
| 422 | `text` links a CharlieBot file-server URL | see the text below the table |
| 502 | A chunk failed after its retries | `<Display name> did not accept chunk <i> of <n> after <attempts> attempts; nothing was persisted` |

The file-server link refusal text (one line; the backticks are part of the text):

```text
reply links a CharlieBot file-server URL, which opens only behind the access key: <url>. Publish the page with `charliebot publish <path>` and write the URL the command prints into the reply
```

200:

```json
{"posted": true, "text": "…", "operator_only_note": null, "chars": 120, "chunks": 1, "over_budget": false, "answers": "<summon id or null>"}
```

- `chunks`: the text split into posts of at most the platform maximum.
- `over_budget`: `chars` is above 500.
- `answers`: the summon that the running turn's newest thread input answers, or null.
- `operator_only_note`: null, or `Application-route links stay as written; the operator alone can open them: <links>` when the text links routes of this server.

Side effects:

1. Post each chunk to the thread through the platform API, with retries.
2. Append and broadcast `{"type":"<reply event type>","content":<text>,"<reply event type>":{"answers","chars","chunks"}}`.
3. Clear the answered summon's pending ack marker.
4. Log `<platform>_reply_posted`.

### `POST /api/internal/slack/ack`

- Handler: `src/features/slack/api.py:55`.
- Body: `{"session_id": string, "message_ids": [string]}`, both required.

| Status | Case | `detail` |
|---|---|---|
| 422 | `message_ids` is empty | `message_ids is empty` |
| 422 | An id is unknown or not eligible | `Unknown or ineligible message id: <id>` |
| 422 | An eligible unread id at or below the largest given id is missing | `Skipped eligible message id at or below <largest>: <id>` |

200: `{"acked": <number of distinct ids>, "watermark_ts": "<watermark>"}`.

Side effects:

1. When the largest id is above the watermark, set `slack_watermark_ts` to it and save the metadata.
2. Append and broadcast `{"type":"slack_ack","content":"Slack thread ack: <n> message(s) read through <largest>","slack_ack":{"message_ids":[…],"watermark_ts":…}}`.
3. Log `slack_thread_acked`.

Ids at or below the watermark are acked again with no change.

### `POST /api/internal/discord/read`

- Handler: `src/features/discord/api.py:62`.
- Body: `{"session_id": string, "url": string or null, "limit": int}`. `limit` defaults to 50 and must be 1..100 (422 otherwise).

Without `url` (the session's own thread):

- The thread checks apply (404, 409).
- The window is `limit` messages that start at the earlier of the oldest unread message and the oldest pending summon mention. With neither, the window is the newest `limit` messages.
- The thread's starter message comes first when it exists.
- The unread messages in the window are acked (see the Slack ack side effects, with `discord_*` names).

With `url` (a discord.com channel link):

- 422 when the URL is not a discord.com link.
- 404 `the bot cannot see this channel`.
- The newest `limit` messages, all with `unread: false`. Nothing is acked.

Any other Discord API failure gives 502 with the error text.

200:

```json
{
  "messages": [{"id": "…", "author_id": "…", "author": "…", "person": null, "timestamp": "…", "content": "…", "attachments": ["<url>"], "unread": true, "summon": false}],
  "watermark_id": "<watermark after the ack, or null>",
  "more_unread": 0
}
```

- `author`: the author's `global_name`, else `username`.
- `person`: the configured person for the author id in `discord.allowed_users`, or null.
- `more_unread`: unread messages outside the window. Always 0 with `url`.

### `POST /api/internal/discord/check`

- Handler: `src/features/discord/api.py:88`.
- Body: `{}`. Any key gives 422.

| Status | Case | Body |
|---|---|---|
| 409 | No `credentials.discord.bot_token` | `{"detail":"credentials.discord.bot_token is not set"}` |
| 502 | A Discord API call failed | `{"detail":"<error>"}` |
| 200 | | see below |

```json
{"ok": true, "bot_user": {"id": "…", "username": "…"}, "application_id": "…", "message_content_intent": true, "guilds": [{"id": "…", "name": "…", "missing_permissions": []}]}
```

`ok` is true when the message-content intent is on and no guild misses a permission. The route reads three Discord endpoints (current user, current application, current user guilds). It writes nothing.

## Web sessions

A web session is a manager session with no Slack or Discord origin. Every web route checks:

1. Unknown session: 404 `Session not found`.
2. Not a web session (a thread session or a worker): 409 `Session is not a web session: it is a thread session or a worker`.

### `POST /api/internal/web/reply`

- Handler: `src/features/web_reply/api.py:31`.
- Body: `{"session_id": string, "text": string}`.
- 422 `Reply text is empty` for blank text.
- 200: `{"posted": true, "chars": <len>, "over_budget": <chars > 500>, "answers": "<user event id or null>"}`.

`answers` is the newest user message (or audit nudge, which maps to its user message) among the running turn's inputs.

Side effects:

1. Append and broadcast `{"type":"web_reply","content":<text>,"web_reply":{"answers","chars"}}`.
2. Mark the session unread (sidebar `unread_changed` frame when the flag changes).
3. Log `web_reply_posted`.

### `GET /api/internal/web/dialog`

- Handler: `src/features/web_reply/api.py:49`.
- Query: `session_id` (string, required).
- 200: `{"session_id": "…", "entries": [{"role": "user" | "web_reply" | "web_reply_notice", "content": "…"}]}`.

`entries` are the committed messages of the whole chat log (history projection, final flush) whose role is one of the three, in projection order. The route reads only.

### `POST /api/internal/web/status`

- Handler: `src/features/web_reply/api.py:64`.
- Body: `{"session_id": string, "text": string}`. `text` is the content of a JSON file.

The server parses `text` as a status patch with the optional keys `goal` (non-blank string), `now` (non-blank string) and `waiting` (list of `{"text": non-blank string, "link": string or null}`). Unknown keys are refused.

| Status | Case | `detail` |
|---|---|---|
| 422 | Not JSON | `Status file is not JSON: <message>` |
| 422 | Not an object | `Status file is not a JSON object` |
| 422 | Invalid items | `Status file is invalid: <loc>: <msg>; …` |
| 422 | No item named | `Status file names no item: give goal, now or waiting` |

200: the stored status `{"goal": string or null, "now": string or null, "waiting": [{"text","link"}], "updated_at": "<datetime Z>"}`.

Side effects:

1. Merge the named items into the stored `web_status` slot and set `updated_at`. Items the patch leaves out keep their stored values.
2. Append and broadcast `{"type":"web_status","content":"Status updated: <labels>","web_status":<status>,"web_status_changed":[<items>]}`. The labels are `Goal`, `Now` and `Waiting on you`.
3. Send `{"type":"web_status_subtree","source_session_id":<id>}` to each ancestor's session channel.
4. Log `web_status_set`.

### `GET /api/internal/web/status`

- Handler: `src/features/web_reply/api.py:82`.
- Query: `session_id` (string, required).
- 200:

```json
{"session_id": "…", "web_status": null, "descendants": [{"session_id": "…", "name": "…", "updated_at": "…", "waiting": [{"text": "…", "link": null}]}]}
```

`descendants` lists the descendants whose status has at least one waiting item, whose session is not archived and whose task is open. They are sorted by status `updated_at`, newest first.
