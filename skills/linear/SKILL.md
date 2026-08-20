---
name: linear
description: This skill should be used when the user asks to read, create, update, or search Linear issues, projects, cycles, or teams.
version: 1.1.1
---

# Linear

Read, create, update, and search issues in Linear using the Linear GraphQL API.

## Configuration

- Token location: `~/.charliebot/config.yaml` under `linear_api_key`
- Token type: Personal API Key
- API endpoint: `https://api.linear.app/graphql`

## API Reference

All requests use POST to the GraphQL endpoint with headers:

```
Content-Type: application/json
Authorization: TOKEN
```

Read the token from `~/.charliebot/config.yaml` before making any API calls.

### Example: curl template

```bash
curl -s -X POST https://api.linear.app/graphql \
  -H "Content-Type: application/json" \
  -H "Authorization: $LINEAR_API_KEY" \
  -d '{"query": "GRAPHQL_QUERY"}'
```

Use `python3 -c '...'` or `python3 -m json.tool` to parse and format JSON responses.

### Viewer (who am I)

```graphql
query { viewer { id name email } }
```

### List Teams

```graphql
query { teams { nodes { id name key } } }
```

### List Issues (with filters)

```graphql
query($filter: IssueFilter) {
  issues(filter: $filter, first: 50) {
    nodes {
      id identifier title state { name } priority assignee { name } createdAt updatedAt
    }
  }
}
```

Common filters:
- By team: `{"team": {"key": {"eq": "TEAM_KEY"}}}`
- By assignee: `{"assignee": {"name": {"eq": "Name"}}}`
- By state: `{"state": {"name": {"in": ["Todo", "In Progress"]}}}`
- Combined: nest multiple filters together

### Get Single Issue

```graphql
query($id: String!) {
  issue(id: $id) {
    id identifier title description state { name } priority
    assignee { name } labels { nodes { name } }
    comments { nodes { body user { name } createdAt } }
  }
}
```

You can also look up by identifier (e.g. "ENG-123"); `issue(id:)` accepts the
human identifier and returns one issue:

```graphql
query {
  issue(id: "ENG-123") {
    id identifier title description state { name }
  }
}
```

### Search Issues

```graphql
query($term: String!) {
  searchIssues(term: $term, first: 20) {
    nodes {
      id identifier title state { name } assignee { name } priority
    }
  }
}
```

### Two-path recall

Neither search path alone establishes whether a matching issue exists, so run
both and deduplicate before presenting. The semantic path `searchIssues(term:)`
returns hits for unrelated terms, so it answers "does anything look like this"
but cannot prove that no matching issue exists. The structured path
`issues(filter:{or:[{title:{containsIgnoreCase:K}},{description:{containsIgnoreCase:K}}]})`
answers "does an issue containing K exist"; an empty result is a usable negative.
A conclusion of "no matching issue" requires the structured path; present the
deduplicated union of both paths.

### Create Issue

```graphql
mutation($input: IssueCreateInput!) {
  issueCreate(input: $input) {
    success
    issue { id identifier title url }
  }
}
```

Variables:
```json
{
  "input": {
    "teamId": "TEAM_UUID",
    "title": "Issue title",
    "description": "Markdown description",
    "priority": 2,
    "assigneeId": "USER_UUID"
  }
}
```

Priority: 0=No priority, 1=Urgent, 2=High, 3=Medium, 4=Low

### Update Issue

```graphql
mutation($id: String!, $input: IssueUpdateInput!) {
  issueUpdate(id: $id, input: $input) {
    success
    issue { id identifier title state { name } }
  }
}
```

### Add Comment

```graphql
mutation($input: CommentCreateInput!) {
  commentCreate(input: $input) {
    success
    comment { id body }
  }
}
```

Variables: `{"input": {"issueId": "ISSUE_UUID", "body": "Comment text"}}`

### List Projects

```graphql
query {
  projects(first: 50) {
    nodes { id name state { ... on ProjectState { name } } }
  }
}
```

### List Cycles

```graphql
query($teamId: String!) {
  team(id: $teamId) {
    cycles(first: 10, orderBy: createdAt) {
      nodes { id name startsAt endsAt }
    }
  }
}
```

## Rules

- **English only:** All ticket titles and descriptions must be written in English, regardless of conversation language.
- **Default assignee:** Use the host-configured default assignee if one is set (host-specific skill / MEMORY); otherwise leave unassigned and ask the user.
- **User approval required:** Always show the draft and get user confirmation before creating or updating any issue.
- **Feishu for documentation:** Create a Feishu document for a Linear issue when the user asks for one or when the issue's detail is meant for colleagues to read; otherwise the detail stays in the issue's session artifacts rather than in Linear comments or long descriptions. When a doc exists, each Linear issue maps to **one** Feishu document, subsequent updates append to the same Feishu doc never a new one, the Feishu doc title matches the Linear issue title, and the Feishu doc URL is linked in the Linear issue description.
- **Default Feishu doc sharing:** When creating the Feishu doc associated with a Linear issue, set its public permission to tenant-internal readable via `PATCH https://open.feishu.cn/open-apis/drive/v1/permissions/{document_id}/public?type=docx` with `{"link_share_entity":"tenant_readable"}` after the user explicitly approves that permission write. Do not widen to `tenant_editable` unless the user explicitly asks, and do not enable external public access by default.
- **Permission-write approval:** Any Feishu permission change for an associated Feishu doc, including the default `tenant_readable` setup for a new doc, still requires explicit user approval before sending the Feishu permission write.

## Workflow

1. Read the token from `~/.charliebot/config.yaml`
2. If team context is needed, call `teams` query first to resolve team IDs
3. Use `searchIssues` for text lookups and `issues` with filters for structured queries
4. For creating/updating issues, resolve team and user IDs first
5. Always format output clearly — show identifier, title, state, assignee, and priority
6. **Before creating or updating issues, confirm the action with the user**
7. **When adding detailed content to an issue:**
   a. Create a Feishu doc only when the user asks for one or when the detail is meant for colleagues to read; otherwise leave the detail in the issue's session artifacts
   b. Check if the issue description already contains a Feishu doc URL — if so, append new content to that existing doc
   c. If no Feishu doc exists yet and one is warranted, get the user's approval for the Feishu document creation and permission write, create one with the same title as the Linear issue, set its public permission via `PATCH https://open.feishu.cn/open-apis/drive/v1/permissions/{document_id}/public?type=docx` and `{"link_share_entity":"tenant_readable"}`, then update the Linear issue description to include the Feishu doc URL
   d. When a doc exists, all detailed notes, investigation logs, and updates go into the Feishu doc; Linear issue stays concise (see Content conventions)

## Content conventions

- Issue body: goal and deliverables only. No status updates, no implementation
  detail, no results. When no Feishu doc exists, progress and technical content
  live in the issue's session artifacts; when one exists, they live in the linked
  Feishu doc.
- Externally shared Feishu result docs are written in English, structured as:
  what was done, which optimizations, final results, experiment environment,
  reproducible scripts. No process narrative.
