---
name: google-docs
description: This skill should be used when the user asks to read, create, or edit Google Docs documents, or manage document content through the Google Docs and Drive APIs.
version: 1.0.0
---

# Google Docs

Read, create, and edit Google Docs using the Google Docs API with a user refresh token.

**IMPORTANT: User approval required for all writes.** Before any create, update, append, or move action, show the planned content first and wait for explicit `take off` approval. Do not mutate anything before that approval.

## Configuration

The shared Google credential configuration is defined once in the **google-oauth** skill: `skills/google-oauth/SKILL.md`.

Additional key (section `google`): optional `docs_default_folder_id`.

## API Reference

All Google API requests use:

```bash
-H "Authorization: Bearer $ACCESS_TOKEN"
-H "Content-Type: application/json"
```

Read the configured values before making any calls.

### Refresh an Access Token

Mint the access token from the stored refresh token with the recipe in the **google-oauth** skill: `skills/google-oauth/SKILL.md`.

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

If `google.docs_default_folder_id` is set, prefer Drive API placement only after the write plan is approved.

## Bootstrap / Re-Authorization

All Google integrations share one OAuth client and refresh token (the `google` section of `~/.charliebot/credentials.yaml`). One-time setup and re-authorization after expiry follow the **google-oauth** skill: `skills/google-oauth/SKILL.md`.

## Workflow

1. Read the token and client values from `~/.charliebot/credentials.yaml` (section `google`).
2. Refresh an access token with the OAuth token endpoint.
3. For reads, call `documents.get` and convert paragraph text runs to plain text when you only need readable content.
4. For writes, show the exact planned content first and wait for explicit `take off` approval.
5. After approval, create or update via Docs API, and use Drive API only when folder placement or moving is required. After a successful create, return `https://docs.google.com/document/d/{documentId}/edit`.
