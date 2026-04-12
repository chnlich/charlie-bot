---
name: google-sheets
description: This skill should be used when the user asks to read, create, or edit Google Sheets spreadsheets, or manage cell data through the Google Sheets API.
version: 1.0.0
---

# Google Sheets

Read, create, and edit Google Sheets using the Sheets API v4 with a user refresh token.

**IMPORTANT: User approval required for all writes.** Before any create, update, append, or batch update action, show the planned content first and wait for explicit `take off` approval. Do not mutate anything before that approval.

## Configuration

- Credentials location: `~/.charliebot/config.yaml`
- Keys:
  - `google_client_id`
  - `google_client_secret`
  - `google_refresh_token`
- Auth model: OAuth2 user token flow using a long-lived refresh token
- Store only `google_refresh_token` in config. Access tokens are minted at runtime and discarded after use.

## API Reference

All Google API requests use:

```bash
-H "Authorization: Bearer $ACCESS_TOKEN"
-H "Content-Type: application/json"
```

Read the configured values before making any calls.

### Refresh an Access Token

Use the Google OAuth token endpoint at runtime:

```bash
# Read credentials (uses python3+pyyaml; yq may not be installed)
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

If the refresh token is invalid or revoked, re-run the bootstrap flow below.

### Create a Spreadsheet

```bash
curl -s -X POST https://sheets.googleapis.com/v4/spreadsheets \
  -H "Authorization: Bearer $ACCESS_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"properties":{"title":"Spreadsheet Title"}}'
```

After a successful create, return the editor URL:

`https://docs.google.com/spreadsheets/d/{spreadsheetId}/edit`

### Read Spreadsheet Metadata

```bash
curl -s "https://sheets.googleapis.com/v4/spreadsheets/SPREADSHEET_ID" \
  -H "Authorization: Bearer $ACCESS_TOKEN"
```

Returns spreadsheet properties, sheet names, grid dimensions, and other metadata. To list only sheet titles, inspect `sheets[].properties.title` in the response.

### Read Values

```bash
curl -s "https://sheets.googleapis.com/v4/spreadsheets/SPREADSHEET_ID/values/RANGE" \
  -H "Authorization: Bearer $ACCESS_TOKEN"
```

`RANGE` uses A1 notation, e.g. `Sheet1!A1:D10` or `Sheet1!A:A`. The response contains a `values` array of rows.

### Write Values

```bash
curl -s -X PUT "https://sheets.googleapis.com/v4/spreadsheets/SPREADSHEET_ID/values/RANGE?valueInputOption=USER_ENTERED" \
  -H "Authorization: Bearer $ACCESS_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "range": "RANGE",
    "majorDimension": "ROWS",
    "values": [
      ["A1 value", "B1 value"],
      ["A2 value", "B2 value"]
    ]
  }'
```

`valueInputOption=USER_ENTERED` parses values as if typed into the UI (numbers, dates, formulas are interpreted). Use `RAW` to store literal strings.

### Append Values

```bash
curl -s -X POST "https://sheets.googleapis.com/v4/spreadsheets/SPREADSHEET_ID/values/RANGE:append?valueInputOption=USER_ENTERED" \
  -H "Authorization: Bearer $ACCESS_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "range": "RANGE",
    "majorDimension": "ROWS",
    "values": [
      ["A value", "B value"]
    ]
  }'
```

Appends rows after the last row with data in the specified range. The `RANGE` determines which table to append to (e.g. `Sheet1!A:B`).

### Batch Update

Use `spreadsheets.batchUpdate` for structural changes (add/delete sheets, formatting, merge cells, resize, conditional formatting, etc.):

```bash
curl -s -X POST "https://sheets.googleapis.com/v4/spreadsheets/SPREADSHEET_ID:batchUpdate" \
  -H "Authorization: Bearer $ACCESS_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "requests": [
      {
        "addSheet": {
          "properties": { "title": "New Sheet" }
        }
      }
    ]
  }'
```

Common request types: `addSheet`, `deleteSheet`, `updateSheetProperties`, `mergeCells`, `repeatCell` (for formatting), `autoResizeDimensions`, `addConditionalFormatRule`.

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

1. Read the token and client values from `~/.charliebot/config.yaml`.
2. Refresh an access token with the OAuth token endpoint.
3. For reads, call `spreadsheets.get` for metadata or `spreadsheets.values.get` for cell data.
4. For writes, show the exact planned content first and wait for explicit `take off` approval.
5. After approval, create, write, append, or batch update via Sheets API. After a successful create, return `https://docs.google.com/spreadsheets/d/{spreadsheetId}/edit`.
