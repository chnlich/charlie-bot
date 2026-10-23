---
name: google-sheets
description: This skill should be used when the user asks to read, create, or edit Google Sheets spreadsheets, or manage cell data through the Google Sheets API.
version: 1.0.0
---

# Google Sheets

Read, create, and edit Google Sheets using the Sheets API v4 with a selected Google account.

**IMPORTANT: User approval required for all writes.** Before any create, update, append, or batch update action, show the planned content first and wait for explicit `take off` approval. Do not mutate anything before that approval.

## Configuration

The shared Google credential configuration is defined once in the **google-oauth** skill: `skills/google-oauth/SKILL.md`.

## API Reference

All Google API requests use:

```bash
-H "Authorization: Bearer $ACCESS_TOKEN"
-H "Content-Type: application/json"
```

Use the selected account's access token before making any calls.

### Refresh an Access Token

Choose the account using the **Accounts** section in the **google-oauth** skill, then mint its access token:

```bash
ACCESS_TOKEN=$(python3 skills/google-oauth/scripts/google_account.py token --account <name>)
```

For the default account, omit `--account`.

If the refresh token is invalid or revoked, follow **Add or Re-Authorize an Account** in the **google-oauth** skill: `skills/google-oauth/SKILL.md`.

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

Choose an account in the **Accounts** section and follow **Add or Re-Authorize an Account** in the **google-oauth** skill: `skills/google-oauth/SKILL.md`.

## Workflow

1. Choose the account that matches the request using the **Accounts** section in the **google-oauth** skill.
2. Mint its access token with `python3 skills/google-oauth/scripts/google_account.py token --account <name>`. Omit `--account` for the default account.
3. For reads, call `spreadsheets.get` for metadata or `spreadsheets.values.get` for cell data.
4. For writes, show the exact planned content first and wait for explicit `take off` approval.
5. After approval, create, write, append, or batch update via Sheets API. After a successful create, return `https://docs.google.com/spreadsheets/d/{spreadsheetId}/edit`.
