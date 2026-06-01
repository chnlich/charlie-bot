---
name: feishu
description: This skill should be used when the user asks to read, create, or edit Feishu/Lark documents, search docs in their Feishu workspace, or fetch content from a Feishu wiki or docx URL.
version: 2.0.0
---

# Feishu Docs

Read, create, and edit documents in the Feishu workspace using the Feishu Open API with a user access token.

**IMPORTANT: User approval required for all writes.** Feishu does not support online review/diff. Before creating or editing any document, you MUST show the user the planned content and get explicit approval first.

## Configuration

- Credentials location: `~/.charliebot/config.yaml`
- Keys: `feishu_app_id`, `feishu_app_secret`, `feishu_user_access_token`, `feishu_refresh_token`
- Auth method: OAuth2 user access token (acts as the user's own account)
- **yq note**: yq is snap-installed — always use `cat ~/.charliebot/config.yaml | yq '.key'` pattern (never `yq .key file`)

## API Reference

All requests require: `-H "Authorization: Bearer TOKEN"`

Read the token before making calls:
```bash
FEISHU_TOKEN=$(cat ~/.charliebot/config.yaml | yq '.feishu_user_access_token')
```

### Read a Wiki Page

Wiki URLs look like: `https://xxx.feishu.cn/wiki/{node_token}`

**Step 1** — Resolve the wiki node to get the actual document token:
```
GET https://open.feishu.cn/open-apis/wiki/v2/spaces/get_node?token={node_token}
```
The response contains `data.node.obj_token` (the document ID) and `data.node.obj_type` (usually `docx`).

**Step 2** — Fetch document content:
```
GET https://open.feishu.cn/open-apis/docx/v1/documents/{obj_token}/raw_content
```
Returns plain text in `data.content`.

### Read a Docx Page

Docx URLs look like: `https://xxx.feishu.cn/docx/{doc_token}`

```
GET https://open.feishu.cn/open-apis/docx/v1/documents/{doc_token}/raw_content
```

### Get Document Blocks (Structured)

For richer content with block types (headings, code, tables, etc.):
```
GET https://open.feishu.cn/open-apis/docx/v1/documents/{doc_token}/blocks?page_size=500
```
Returns block tree in `data.items`. Use `page_token` from response for pagination.

### Search Documents

```
POST https://open.feishu.cn/open-apis/suite/docs-api/search/object
Content-Type: application/json

{
  "search_key": "your query",
  "count": 10,
  "docs_types": ["docx", "wiki"]
}
```

Requires the `search:docs:read` user scope. Results are returned in `data.docs_entities[]`, each with `docs_token`, `docs_type`, and `title`.

### Public Permission

Use this to inspect or update the public-sharing policy for a docx document:

```
GET https://open.feishu.cn/open-apis/drive/v1/permissions/{token}/public?type=docx
PATCH https://open.feishu.cn/open-apis/drive/v1/permissions/{token}/public?type=docx
Content-Type: application/json

{
  "link_share_entity": "tenant_readable"
}
```

Use the doc token / `document_id` as `{token}`.

Required user scopes:
- `docs:permission.setting:read`
- `docs:permission.setting:write_only`

`PATCH` is a write and still requires explicit user approval before execution.

Default policy for Linear-linked docs:
- `link_share_entity: tenant_readable` so the document is readable within the configured tenant
- Do **not** widen to `tenant_editable` unless the user explicitly asks
- Do **not** enable external public access by default

### List Wiki Space Nodes

To browse a wiki space:
```
GET https://open.feishu.cn/open-apis/wiki/v2/spaces/{space_id}/nodes?page_size=50
```

### Create a Document

```
POST https://open.feishu.cn/open-apis/docx/v1/documents
Content-Type: application/json

{
  "title": "Document Title",
  "folder_token": "optional_folder_token"
}
```
Returns `data.document.document_id`.

To create in a specific folder, provide `folder_token`. Omit it to create in the user's root folder.

### Add Content Blocks to a Document

```
POST https://open.feishu.cn/open-apis/docx/v1/documents/{document_id}/blocks/{block_id}/children
Content-Type: application/json

{
  "children": [
    {
      "block_type": 2,
      "text": {
        "elements": [
          {
            "text_run": {
              "content": "Your text here",
              "text_element_style": {}
            }
          }
        ]
      }
    }
  ]
}
```

Common block types:
- `2` — Text paragraph
- `3` — Heading 1
- `4` — Heading 2
- `5` — Heading 3
- `12` — Bullet (unordered list)
- `13` — Ordered list
- `14` — Code block
- `31` — Table (use two-step process below)
- `32` — Table cell (auto-created, do not insert manually)

For the top-level document block, `block_id` equals `document_id`.

### Insert a Table

Tables require a two-step process: create the table block, then fill each cell.

**Step 1** — Create the table (block_type 31):
```
POST https://open.feishu.cn/open-apis/docx/v1/documents/{document_id}/blocks/{block_id}/children

{
  "children": [
    {
      "block_type": 31,
      "table": {
        "property": {
          "row_size": 3,
          "column_size": 2
        }
      }
    }
  ]
}
```

The response contains `data.children[0].table.cells` — a flat array of cell block IDs in **row-major order** (row0col0, row0col1, row1col0, row1col1, ...).

**Step 2** — Fill each cell by adding a text block as its child:
```
POST https://open.feishu.cn/open-apis/docx/v1/documents/{document_id}/blocks/{cell_block_id}/children

{
  "children": [
    {
      "block_type": 2,
      "text": {
        "elements": [
          {
            "text_run": {
              "content": "Cell content",
              "text_element_style": {}
            }
          }
        ]
      }
    }
  ]
}
```

**Important notes:**
- Max 5 table blocks per request
- Rate limit: 3 edits/sec per document — add ~0.4s delay between cell writes
- Cell block IDs are auto-generated; do NOT create block_type 32 manually
- Header row styling is cosmetic in Feishu — first row has no special API treatment

### Update a Block

```
PATCH https://open.feishu.cn/open-apis/docx/v1/documents/{document_id}/blocks/{block_id}
Content-Type: application/json

{
  "update_text_elements": {
    "elements": [
      {
        "text_run": {
          "content": "Updated text"
        }
      }
    ]
  }
}
```

### Delete Blocks

```
DELETE https://open.feishu.cn/open-apis/docx/v1/documents/{document_id}/blocks/{block_id}/children/batch_delete
Content-Type: application/json

{
  "start_index": 0,
  "end_index": 1
}
```

## Workflow Rules

- All writes still require explicit user approval first. This includes document creation, document edits, and permission changes, including the default `tenant_readable` permission update for a new Linear-linked doc.
- When creating a Feishu document for a Linear issue, immediately call `PATCH https://open.feishu.cn/open-apis/drive/v1/permissions/{document_id}/public?type=docx` with `{"link_share_entity":"tenant_readable"}` after document creation so the doc is tenant-internal readable by default.
- Do NOT widen sharing to `tenant_editable` unless the user explicitly asks for edit access.
- Do NOT enable external public access by default.
- Before any permission write, show the exact change and get explicit user approval first.

## Token Refresh

The user access token expires every **2 hours**. If you get a 401 or `99991663` (token expired) error code, refresh it:

```bash
# Step 1: Get app access token
APP_TOKEN=$(curl -s -X POST https://open.feishu.cn/open-apis/auth/v3/app_access_token/internal \
  -H 'Content-Type: application/json' \
  -d "{\"app_id\":\"$(cat ~/.charliebot/config.yaml | yq '.feishu_app_id')\",\"app_secret\":\"$(cat ~/.charliebot/config.yaml | yq '.feishu_app_secret')\"}" | jq -r '.app_access_token')

# Step 2: Refresh user token
RESULT=$(curl -s -X POST https://open.feishu.cn/open-apis/authen/v1/oidc/refresh_access_token \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $APP_TOKEN" \
  -d "{\"grant_type\":\"refresh_token\",\"refresh_token\":\"$(cat ~/.charliebot/config.yaml | yq '.feishu_refresh_token')\"}")

# Step 3: Save new tokens to config
NEW_ACCESS=$(echo $RESULT | jq -r '.data.access_token')
NEW_REFRESH=$(echo $RESULT | jq -r '.data.refresh_token')
sed -i "s|^feishu_user_access_token:.*|feishu_user_access_token: $NEW_ACCESS|" ~/.charliebot/config.yaml
sed -i "s|^feishu_refresh_token:.*|feishu_refresh_token: $NEW_REFRESH|" ~/.charliebot/config.yaml
echo "Token refreshed successfully"
```

The refresh token lasts **30 days**. If it expires, ask the user to re-authorize via OAuth.

## OAuth2 Re-authorization

When the refresh token expires or new scopes are needed, the user must re-authorize:

1. Open in browser: `https://open.feishu.cn/open-apis/authen/v1/authorize?app_id={app_id}&redirect_uri=http://localhost:9999/callback&scope={space-separated scopes}`
2. User authorizes and gets redirected to `http://localhost:9999/callback?code=XXX`
3. Exchange code for tokens:
```bash
APP_TOKEN=$(curl -s -X POST https://open.feishu.cn/open-apis/auth/v3/app_access_token/internal \
  -H 'Content-Type: application/json' \
  -d "{\"app_id\":\"$(cat ~/.charliebot/config.yaml | yq '.feishu_app_id')\",\"app_secret\":\"$(cat ~/.charliebot/config.yaml | yq '.feishu_app_secret')\"}" | jq -r '.app_access_token')

curl -s -X POST https://open.feishu.cn/open-apis/authen/v1/oidc/access_token \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $APP_TOKEN" \
  -d '{"grant_type":"authorization_code","code":"THE_CODE"}' | jq .
```
4. Save new tokens to config.

Current scopes: `auth:user.id:read`, `docx:document`, `docx:document:create`, `docx:document:readonly`, `docx:document:write_only`, `wiki:wiki:readonly`, `wiki:node:move`, `docs:permission.setting:read`, `docs:permission.setting:write_only`, `search:docs:read`

## URL Parsing

Extract tokens from Feishu URLs:
- `https://<tenant>.feishu.cn/wiki/WIKINODETOKENEXAMPLE0123` → node_token = `WIKINODETOKENEXAMPLE0123` (wiki, needs 2-step)
- `https://<tenant>.feishu.cn/docx/DOCTOKENEXAMPLE0123` → doc_token = `DOCTOKENEXAMPLE0123` (direct read)

## Workflow

### Reading
1. Read the token from config
2. If the user provides a URL, parse the token from the URL path
3. For wiki URLs, resolve node → doc token first
4. Fetch content via `raw_content` endpoint
5. If token is expired, refresh it and retry
6. Present the document content to the user

### Creating / Editing
1. **Draft the content and show it to the user for approval** — do NOT write to Feishu without explicit confirmation
2. Once approved, create the document and add content blocks
3. If the document is created for a Linear issue and the user approved the permission write, immediately set its public permission to tenant-internal readable by calling `PATCH https://open.feishu.cn/open-apis/drive/v1/permissions/{document_id}/public?type=docx` with `{"link_share_entity":"tenant_readable"}`
4. Return the document URL to the user: `https://<tenant>.feishu.cn/docx/{document_id}`
5. Do not widen the document to `tenant_editable` unless the user explicitly asks
6. Do not enable external public access by default

### Linking with Linear
- **Title format:** `{identifier} {title}` — e.g. `ABC-123 Example Linear issue title`
- Each Linear issue maps to **one** Feishu doc — subsequent updates append to the same doc, never a new one
- After creating the doc and getting explicit user approval for the permission write, set its public permission with `PATCH https://open.feishu.cn/open-apis/drive/v1/permissions/{document_id}/public?type=docx` and `{"link_share_entity":"tenant_readable"}`, then update the Linear issue description to include the Feishu doc URL
- **Organize by date:** Feishu docs linked from Linear issues are living documents with ongoing updates. Structure content with **H1 date headers** (e.g. `# 2026-03-26`) as top-level sections. Each day's work goes under its date. New updates append a new date section — never overwrite previous dates.
- **Use Pacific Time (PT) for dates** — all date headers use `America/Los_Angeles` timezone, not UTC. Run `TZ=America/Los_Angeles date +%Y-%m-%d` to get the correct date.
