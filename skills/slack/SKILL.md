---
name: slack
description: This skill should be used when the user asks to read Slack messages, search Slack conversations, look up Slack users, check DMs, or interact with the Slack workspace in any way.
version: 1.0.0
---

# Slack

Read messages, search conversations, and look up users in the Meshy Slack workspace using the Slack Web API.

## Configuration

- Token location: `~/.charliebot/config.yaml` under `slack_user_token`
- Token type: User OAuth Token (`xoxp-`)
- Available scopes: `channels:history`, `channels:read`, `groups:history`, `groups:read`, `im:history`, `im:read`, `mpim:history`, `search:read`, `users:read`

## API Reference

All requests require the header: `-H "Authorization: Bearer TOKEN"`

Read the token from `~/.charliebot/config.yaml` before making any API calls.

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

## Workflow

1. Read the token from `~/.charliebot/config.yaml`
2. If looking up a user, call `users.list` first to resolve the user ID
3. If reading DMs, call `conversations.list?types=im` to find the DM channel ID by matching the user ID
4. Call `conversations.history` to fetch messages
5. Use `python3` to parse JSON responses for readability — format output with timestamps, sender names, and message text
