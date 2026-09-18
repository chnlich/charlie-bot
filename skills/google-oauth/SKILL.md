---
name: google-oauth
description: >
  Use when a Google integration (Gmail, Docs, Sheets, Drive, Calendar) needs its
  OAuth client set up, its refresh token obtained again after expiry, or an
  access token minted at runtime.
---

# Google OAuth

All Google integrations share a single OAuth client and refresh token stored in the `google` section of `~/.charliebot/credentials.yaml`.

## Runtime Token Mint

Every integration mints its access token at call time from the stored refresh
token; access tokens are never persisted. Run this before the integration's own
API calls:

```bash
# Read credentials (uses python3+pyyaml)
read GOOGLE_CLIENT_ID GOOGLE_CLIENT_SECRET GOOGLE_REFRESH_TOKEN < <(python3 -c "
import yaml
c = yaml.safe_load(open('$HOME/.charliebot/credentials.yaml'))
print(c['google']['client_id'], c['google']['client_secret'], c['google']['refresh_token'])
")

ACCESS_TOKEN=$(curl -s -X POST https://oauth2.googleapis.com/token \
  -H "Content-Type: application/x-www-form-urlencoded" \
  --data-urlencode "client_id=$GOOGLE_CLIENT_ID" \
  --data-urlencode "client_secret=$GOOGLE_CLIENT_SECRET" \
  --data-urlencode "refresh_token=$GOOGLE_REFRESH_TOKEN" \
  --data-urlencode "grant_type=refresh_token" | jq -r '.access_token')
```

## Bootstrap / Re-Authorization

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
