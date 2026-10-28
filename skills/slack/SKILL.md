---
name: slack
description: This skill should be used when the user asks to read Slack messages, search Slack conversations, look up Slack users, check DMs, or interact with the Slack workspace in any way.
version: 1.0.0
---

# Slack

Read messages, search conversations, and look up users in the configured Slack workspace using the Slack Web API.

## Configuration

- Token location: `$CHARLIEBOT_HOME/credentials.yaml` under `slack.user_token` and `slack.bot_token`
- Token type: User OAuth Token (`xoxp-`)
- Available scopes: `channels:history`, `channels:read`, `groups:history`, `groups:read`, `im:history`, `im:read`, `mpim:history`, `search:read`, `users:read`
- Requests: send every Slack Web API call through `charliebot slack request`. The command reads the token from `credentials.yaml` inside its own process, so no token appears in a command, an output or a run record.

- CharlieBot runtime calls use app credentials.
- Use `slack.user_token` for one-off reads and treat it as read-only.
- Use `slack.bot_token` for posting.
- Grant missing runtime scopes to the app.
- Follow the standing approval rules for outbound messages.

## API Reference

Send every request through `charliebot slack request <METHOD> <URL>`:

```bash
charliebot slack request GET 'https://slack.com/api/auth.test'
charliebot slack request GET 'https://slack.com/api/conversations.list?types=im&limit=200'
```

The command adds the bearer header itself and prints the 2xx response body. It uses `slack.user_token` by default; pass `--token bot` to use `slack.bot_token` for a post. Pass a request body from a file with `--data-file <path>`, and add any other header with `--header 'NAME:VALUE'`.

| Exit code | Meaning |
|---|---|
| 0 | A 2xx response whose `ok` is not `false`. |
| 1 | Another status, `"ok": false`, or a network failure. |
| 2 | Invalid arguments. |
| 3 | `credentials.yaml` is unreadable or the chosen `slack` key is missing. |

Pipe the response body to `jq` when you need one field.

### Authentication

```
GET https://slack.com/api/auth.test
```

### Users

```
GET https://slack.com/api/users.list?limit=200
```

To find a user, match against `real_name`, `name`, and `profile.display_name` fields (case-insensitive).

### Channels

```
GET https://slack.com/api/conversations.list?types=public_channel,private_channel&limit=200
```

### Direct Messages

List all DM conversations:
```
GET https://slack.com/api/conversations.list?types=im&limit=200
```

Each DM channel has a `user` field — match it against the target user's ID to find the right channel.

### Read Messages

```
GET https://slack.com/api/conversations.history?channel=CHANNEL_ID&limit=50
```

Use `oldest` and `latest` (Unix timestamps) to filter by date range. Use `cursor` for pagination.

### Read Threads

```
GET https://slack.com/api/conversations.replies?channel=CHANNEL_ID&ts=THREAD_TS
```

### Search Messages

```
GET https://slack.com/api/search.messages?query=QUERY
```

Supports Slack search modifiers: `from:username`, `in:#channel`, `before:2026-01-01`, `after:2026-01-01`, `has:link`, etc.

## Important Workarounds

- Do NOT use `conversations.open` — it requires `im:write` scope which is not available. Instead, list DM conversations with `conversations.list?types=im` and find the channel by user ID.
- Do NOT use `conversations.join` or any write endpoints — only read scopes are available.

- Resolve a private channel by ID because channel listings can omit it while its history remains readable.
- The bot-token listing shows exactly which private channels allow posting; public channels accept posts without bot membership.

## Workflow

1. Send each read through `charliebot slack request` (it uses `slack.user_token` by default)
2. If looking up a user, call `users.list` first to resolve the user ID
3. If reading DMs, call `conversations.list?types=im` to find the DM channel ID by matching the user ID
4. Call `conversations.history` to fetch messages
5. Use `jq` to parse JSON responses for readability — format output with timestamps, sender names, and message text
