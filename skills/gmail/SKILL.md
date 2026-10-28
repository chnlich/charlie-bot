---
name: gmail
description: >
  Use when the user asks to read, search, or list Gmail messages, threads, or labels.
  Readonly access only — no sending or modifying.
version: 1.1.0
---

# Gmail

Read and search Gmail messages using the Gmail API with a selected Google account. Readonly scope only.

## Configuration

The shared Google credential configuration is defined once in the **google-oauth** skill: `skills/google-oauth/SKILL.md`.

## API Reference

Send every Gmail API request through `charliebot google request`.
The command mints the selected account's access token inside its own process, so the token never appears in a command, in the output or in a run record.
The command prints the response body on stdout and exits 0 for an HTTP 2xx answer; another status exits 1.

### Choose the Account

Choose the account using the **Accounts** section in the **google-oauth** skill.
Pass `--account <name>` on every request, and omit `--account` for the default account.

### List Messages

```bash
charliebot google request GET "https://gmail.googleapis.com/gmail/v1/users/me/messages?maxResults=10"
```

With search query (Gmail search syntax):

```bash
charliebot google request GET "https://gmail.googleapis.com/gmail/v1/users/me/messages?q=from%3Aexample%40gmail.com&maxResults=10" --account <name>
```

### Read a Message

```bash
charliebot google request GET "https://gmail.googleapis.com/gmail/v1/users/me/messages/MESSAGE_ID?format=full"
```

To extract readable text from the response:
- The body is in `payload.body.data` (base64url-encoded) for simple messages.
- For multipart messages, walk `payload.parts[]` and find the part with `mimeType: text/plain` (or `text/html` as fallback). The text is in `parts[].body.data`.
- Decode base64url: `echo "$DATA" | tr '_-' '/+' | base64 -d`

### Read a Thread

```bash
charliebot google request GET "https://gmail.googleapis.com/gmail/v1/users/me/threads/THREAD_ID?format=full"
```

Returns all messages in the thread.

### List Labels

```bash
charliebot google request GET "https://gmail.googleapis.com/gmail/v1/users/me/labels"
```

## Bootstrap / Re-Authorization

Choose an account in the **Accounts** section and follow **Add or Re-Authorize an Account** in the **google-oauth** skill: `skills/google-oauth/SKILL.md`.

## Workflow

1. Choose the account that matches the request using the **Accounts** section in the **google-oauth** skill.
2. Send each request with `charliebot google request GET <URL> --account <name>`. Omit `--account` for the default account.
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
