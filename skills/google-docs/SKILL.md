---
name: google-docs
description: This skill should be used when the user asks to read, create, or edit Google Docs documents, or manage document content through the Google Docs and Drive APIs.
version: 1.0.0
---

# Google Docs

Read, create, and edit Google Docs using the Google Docs API with a user refresh token.

**IMPORTANT: User approval required for all writes.** Before any create, update, append, or move action, show the planned content first and wait for explicit `take off` approval. Do not mutate anything before that approval.

## Configuration

- Credentials location: `~/.charliebot/config.yaml`
- Keys:
  - `google_docs_client_id`
  - `google_docs_client_secret`
  - `google_docs_refresh_token`
  - optional `google_docs_default_folder_id`
- Auth model: OAuth2 user token flow using a long-lived refresh token
- Store only `google_docs_refresh_token` in config. Access tokens are minted at runtime and discarded after use.

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
read GDOCS_CLIENT_ID GDOCS_CLIENT_SECRET GDOCS_REFRESH_TOKEN < <(python3 -c "
import yaml
c = yaml.safe_load(open('$HOME/.charliebot/config.yaml'))
print(c['google_docs_client_id'], c['google_docs_client_secret'], c['google_docs_refresh_token'])
")

ACCESS_TOKEN=$(curl -s -X POST https://oauth2.googleapis.com/token \
  -H "Content-Type: application/x-www-form-urlencoded" \
  --data-urlencode "client_id=$GDOCS_CLIENT_ID" \
  --data-urlencode "client_secret=$GDOCS_CLIENT_SECRET" \
  --data-urlencode "refresh_token=$GDOCS_REFRESH_TOKEN" \
  --data-urlencode "grant_type=refresh_token" | jq -r '.access_token')
```

If the refresh token is invalid or revoked, re-run the bootstrap flow below.

### Create a Google Doc

Docs API create defaults to the user's root folder:

```bash
curl -s -X POST https://docs.googleapis.com/v1/documents \
  -H "Authorization: Bearer $ACCESS_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"title":"Document Title"}'
```

After a successful create, return the editor URL:

`https://docs.google.com/document/d/{documentId}/edit`

If you need explicit folder placement, use the Drive API instead. Docs API create does not place the doc into a chosen folder.

### Read a Document

```bash
curl -s "https://docs.googleapis.com/v1/documents/DOCUMENT_ID" \
  -H "Authorization: Bearer $ACCESS_TOKEN"
```

To turn `documents.get` JSON into readable plain text, walk `body.content[]`, then each `paragraph.elements[]`, and concatenate any `textRun.content` values. Preserve paragraph breaks when a paragraph ends; ignore non-paragraph structural elements unless you need tables, headers, or footnotes.

### Append or Write Content

Use `documents.batchUpdate` with `insertText` requests:

```bash
curl -s -X POST "https://docs.googleapis.com/v1/documents/DOCUMENT_ID:batchUpdate" \
  -H "Authorization: Bearer $ACCESS_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "requests": [
      {
        "insertText": {
          "location": { "index": 1 },
          "text": "Hello, world\n"
        }
      }
    ]
  }'
```

For appending to an existing doc, fetch the current document first and insert at the last body `endIndex - 1`. For a freshly created empty doc, `index: 1` is the usual insertion point.

### Formatting Content

After inserting text, use `updateParagraphStyle` and `updateTextStyle` in the same `batchUpdate` to apply formatting. Requests are applied in order, so insert text first, then style it.

**Headings** — set a paragraph's named style (HEADING_1 through HEADING_6, or NORMAL_TEXT):

```json
{
  "updateParagraphStyle": {
    "range": { "startIndex": 1, "endIndex": 25 },
    "paragraphStyle": { "namedStyleType": "HEADING_1" },
    "fields": "namedStyleType"
  }
}
```

**Bold / Italic** — set text style on a character range:

```json
{
  "updateTextStyle": {
    "range": { "startIndex": 1, "endIndex": 25 },
    "textStyle": { "bold": true },
    "fields": "bold"
  }
}
```

**Practical tips:**
- Indices refer to the document state *after* all preceding requests in the same batch. Insert text first, then style it in the same `batchUpdate` call.
- Use `fields` to specify which style properties to update (e.g. `"bold"`, `"namedStyleType"`, `"bold,italic"`). Omitting `fields` clears unset properties.
- To find the correct index range for inserted text: if you insert N characters at index I, the text occupies `[I, I+N)`.

### Folder Behavior

- `documents.create` always creates in root.
- To create directly inside a folder or move a document later, use the Drive API.
- Typical Drive operations:
  - create a Google Doc with `mimeType: application/vnd.google-apps.document` and `parents: ["FOLDER_ID"]`
  - move an existing doc with `files.update(addParents=..., removeParents=...)`

If `google_docs_default_folder_id` is set, prefer Drive API placement only after the write plan is approved.

## Bootstrap / Re-Authorization

One-time setup to obtain a refresh token for the desktop-app OAuth flow:

1. In Google Cloud Console, enable the Docs API and Drive API.
2. Create an OAuth client of type Desktop app and use a loopback redirect URI that matches the authorized local pattern for that client. Do not hardcode a path unless it is already registered for that specific OAuth client.
3. For SSH use, bind a local loopback listener on the machine running the browser, then forward that port to the remote session if needed, for example: `ssh -L 8080:127.0.0.1:8080 user@remote`. Open the consent URL in the local browser with `redirect_uri=http://127.0.0.1:8080`.
4. Open the consent URL with the needed scopes and offline access:

```text
response_type=code
client_id=CLIENT_ID
redirect_uri=http://127.0.0.1:PORT
scope=https://www.googleapis.com/auth/documents https://www.googleapis.com/auth/drive.file
access_type=offline
prompt=consent
```

5. Authorize once, capture the code from the local loopback callback, and exchange it for tokens with:

```bash
curl -s -X POST https://oauth2.googleapis.com/token \
  -H "Content-Type: application/x-www-form-urlencoded" \
  --data-urlencode "client_id=CLIENT_ID" \
  --data-urlencode "client_secret=CLIENT_SECRET" \
  --data-urlencode "code=AUTH_CODE" \
  --data-urlencode "grant_type=authorization_code" \
  --data-urlencode "redirect_uri=http://127.0.0.1:PORT"
```

6. Save the returned `refresh_token` to `google_docs_refresh_token`.

For SSH-only use, keep the redirect listener on the browser side reachable through the tunnel, and store only the refresh token in config. The access token is temporary and should not be persisted.

## Workflow

1. Read the token and client values from `~/.charliebot/config.yaml`.
2. Refresh an access token with the OAuth token endpoint.
3. For reads, call `documents.get` and convert paragraph text runs to plain text when you only need readable content.
4. For writes, show the exact planned content first and wait for explicit `take off` approval.
5. After approval, create or update via Docs API, and use Drive API only when folder placement or moving is required. After a successful create, return `https://docs.google.com/document/d/{documentId}/edit`.
