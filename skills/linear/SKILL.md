---
name: linear
description: This skill should be used when the user asks to read, create, update, or search Linear issues, projects, cycles, or teams.
version: 1.0.0
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

You can also look up by identifier (e.g. "ENG-123"):

```graphql
query {
  issueSearch(query: "ENG-123", first: 1) {
    nodes { id identifier title description state { name } }
  }
}
```

### Search Issues

```graphql
query($query: String!) {
  issueSearch(query: $query, first: 20) {
    nodes {
      id identifier title state { name } assignee { name } priority
    }
  }
}
```

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
- **Use Feishu for documentation:** Detailed updates and notes for a Linear issue should be recorded in a Feishu document (using the `feishu` skill), NOT as Linear comments or long descriptions. Each Linear issue maps to **one** Feishu document — subsequent updates are appended to the same Feishu doc, never a new one. The Feishu doc title should match the Linear issue title. The Feishu doc URL must be linked in the Linear issue description.
- **Default Feishu doc sharing:** When creating the Feishu doc associated with a Linear issue, set its public permission to tenant-internal readable via `PATCH https://open.feishu.cn/open-apis/drive/v1/permissions/{document_id}/public?type=docx` with `{"link_share_entity":"tenant_readable"}` after the user explicitly approves that permission write. Do not widen to `tenant_editable` unless the user explicitly asks, and do not enable external public access by default.
- **Permission-write approval:** Any Feishu permission change for an associated Feishu doc, including the default `tenant_readable` setup for a new doc, still requires explicit user approval before sending the Feishu permission write.

## Workflow

1. Read the token from `~/.charliebot/config.yaml`
2. If team context is needed, call `teams` query first to resolve team IDs
3. Use `issueSearch` for text-based lookups, `issues` with filters for structured queries
4. For creating/updating issues, resolve team and user IDs first
5. Always format output clearly — show identifier, title, state, assignee, and priority
6. **Before creating or updating issues, confirm the action with the user**
7. **When adding detailed content to an issue:**
   a. Check if the issue description already contains a Feishu doc URL — if so, append new content to that existing doc
   b. If no Feishu doc exists yet, get the user's approval for the Feishu document creation and permission write, create one with the same title as the Linear issue, set its public permission via `PATCH https://open.feishu.cn/open-apis/drive/v1/permissions/{document_id}/public?type=docx` and `{"link_share_entity":"tenant_readable"}`, then update the Linear issue description to include the Feishu doc URL
   c. All detailed notes, investigation logs, and updates go into the Feishu doc; Linear issue stays concise (status + Feishu link)
