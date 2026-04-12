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

- Credentials location: `~/.charliebot/config.yaml`
- Keys:
  - `google_client_id`
  - `google_client_secret`
  - `google_refresh_token`
- Auth model: OAuth2 user token flow using a long-lived refresh token
- Store only `google_refresh_token` in config. Access tokens are minted at runtime and discarded after use.

## API Reference

All Gmail API requests use:

```bash
-H "Authorization: Bearer $ACCESS_TOKEN"
```

### Refresh an Access Token

```bash
read GOOGLE_CLIENT_ID GOOGLE_CLIENT_SECRET GOOGLE_REFRESH_TOKEN < <(python3 -c "
import yaml
c = yaml.safe_load(open('$HOME/.charliebot/config.yaml'))
print(c['google_client_id'], c['google_client_secret'], c['google_refresh_token'])
")

ACCESS_TOKEN=$(curl -s -X POST https://oauth2.googleapis.com/token \
  -H "Content-Type: application/x-www-form-urlencoded" \
  --data-urlencode "client_id=$GOOGLE_CLIENT_ID" \
  --data-urlencode "client_secret=$GOOGLE_CLIENT_SECRET" \
  --data-urlencode "refresh_token=$GOOGLE_REFRESH_TOKEN" \
  --data-urlencode "grant_type=refresh_token" | jq -r '.access_token')
```

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

All Google integrations (Gmail, Docs, Sheets, Drive, Calendar) share a single OAuth client and refresh token stored under the unified `google_*` config keys.

One-time setup to obtain a refresh token for the desktop-app OAuth flow:

1. In Google Cloud Console, enable the APIs you need (Gmail, Docs, Sheets, Drive, Calendar).
2. Create an OAuth client of type **Desktop app**.
3. Open the consent URL with all scopes and offline access. Use `redirect_uri=http://localhost` (not `http://127.0.0.1:PORT`):

```text
response_type=code
client_id=CLIENT_ID
redirect_uri=http://localhost
scope=https://www.googleapis.com/auth/spreadsheets https://www.googleapis.com/auth/documents https://www.googleapis.com/auth/drive.file https://www.googleapis.com/auth/gmail.readonly https://www.googleapis.com/auth/calendar
access_type=offline
prompt=consent
```

4. Authorize once, capture the `code` from the redirect, and exchange it for tokens:

```bash
curl -s -X POST https://oauth2.googleapis.com/token \
  -H "Content-Type: application/x-www-form-urlencoded" \
  --data-urlencode "client_id=CLIENT_ID" \
  --data-urlencode "client_secret=CLIENT_SECRET" \
  --data-urlencode "code=AUTH_CODE" \
  --data-urlencode "grant_type=authorization_code" \
  --data-urlencode "redirect_uri=http://localhost"
```

5. Save the returned `refresh_token` to `google_refresh_token` in `~/.charliebot/config.yaml`.

**Note:** If the GCP project is in Testing mode, the refresh token expires in ~7 days. Publish the OAuth consent screen to Production for non-expiring tokens.

## Workflow

1. Read credentials from `~/.charliebot/config.yaml`.
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
