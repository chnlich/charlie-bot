---
name: gmail
description: >
  Use when the user asks to read, search, or list Gmail messages, threads, or labels.
  Readonly access only — no sending or modifying.
version: 1.0.0
---

# Gmail

Read and search Gmail messages using the Gmail API with a user refresh token. Readonly scope only.

**Note:** This account uses Gmailify, so emails from linked external accounts are also accessible.

## Configuration

The shared Google credential configuration is defined once in the **google-oauth** skill: `skills/google-oauth/SKILL.md`.

## API Reference

All Gmail API requests use:

```bash
-H "Authorization: Bearer $ACCESS_TOKEN"
```

### Refresh an Access Token

Mint the access token from the stored refresh token with the recipe in the **google-oauth** skill: `skills/google-oauth/SKILL.md`.

### List Messages

```bash
curl -s "https://gmail.googleapis.com/gmail/v1/users/me/messages?maxResults=10" \
  -H "Authorization: Bearer $ACCESS_TOKEN"
```

With search query (Gmail search syntax):

```bash
curl -s "https://gmail.googleapis.com/gmail/v1/users/me/messages?q=from%3Aexample%40gmail.com&maxResults=10" \
  -H "Authorization: Bearer $ACCESS_TOKEN"
```

### Read a Message

```bash
curl -s "https://gmail.googleapis.com/gmail/v1/users/me/messages/MESSAGE_ID?format=full" \
  -H "Authorization: Bearer $ACCESS_TOKEN"
```

To extract readable text from the response:
- The body is in `payload.body.data` (base64url-encoded) for simple messages.
- For multipart messages, walk `payload.parts[]` and find the part with `mimeType: text/plain` (or `text/html` as fallback). The text is in `parts[].body.data`.
- Decode base64url: `echo "$DATA" | tr '_-' '/+' | base64 -d`

### Read a Thread

```bash
curl -s "https://gmail.googleapis.com/gmail/v1/users/me/threads/THREAD_ID?format=full" \
  -H "Authorization: Bearer $ACCESS_TOKEN"
```

Returns all messages in the thread.

### List Labels

```bash
curl -s "https://gmail.googleapis.com/gmail/v1/users/me/labels" \
  -H "Authorization: Bearer $ACCESS_TOKEN"
```

## Bootstrap / Re-Authorization

All Google integrations share one OAuth client and refresh token (the `google` section of `~/.charliebot/credentials.yaml`). One-time setup and re-authorization after expiry follow the **google-oauth** skill: `skills/google-oauth/SKILL.md`.

## Workflow

1. Read credentials from `~/.charliebot/credentials.yaml` (section `google`).
2. Mint an access token using the refresh token.
3. Search or list messages using Gmail search syntax (`q=` parameter).
4. Fetch full message content and decode the body.
5. Present extracted text to the user.

## Common Gmail Search Queries

- `from:someone@example.com` — from a specific sender
- `to:someone@example.com` — to a specific recipient
- `subject:keyword` — subject contains keyword
- `is:unread` — unread messages
- `newer_than:2d` — messages from the last 2 days
- `has:attachment` — messages with attachments
- `label:LABEL_NAME` — messages with a specific label
- Combine with spaces (AND) or `OR`: `from:a@b.com subject:invoice`
