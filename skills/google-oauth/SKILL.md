---
name: google-oauth
description: >
  Use when a Google integration (Gmail, Docs, Sheets, Drive, Calendar) needs its
  OAuth client set up or its refresh token obtained again after expiry.
---

# Google OAuth Bootstrap

All Google integrations share a single OAuth client and refresh token stored in the `google` section of `~/.charliebot/credentials.yaml`.

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

5. Save the returned `refresh_token` to the `refresh_token` key of the `google` section in `~/.charliebot/credentials.yaml`.

**Note:** If the GCP project is in Testing mode, the refresh token expires in ~7 days. Publish the OAuth consent screen to Production for non-expiring tokens.
