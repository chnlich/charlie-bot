# Plans

The plan registry routes live in `src/features/artifacts/api.py`. Four verbs sit under `/api/internal/plan/` and the CLI calls them (`charliebot plan …`). One read route sits under `/api/sessions/`.

A session's plans are in `sessions/<session_id>/plans.json` (the state contract documents the file). A plan is a lineage of versions. Each version binds one HTML page inside the session directory.

## Plan states

The state is derived from `closed` and `takeoff`:

| `closed` | `takeoff` | State |
|---|---|---|
| null | null | `awaiting approval` |
| null | set | `approved` |
| `{"as":"superseded",…}` | any | `superseded` |
| `{"as":"abandoned",…}` | any | `abandoned` |
| `{"as":"completed",…}` | any | `completed` |

A session with a plan in state `awaiting approval` shows `has_pending_plan_approval: true` in the sidebar state.

## Shared verb rules

- Auth: access key. The verbs do not call `require_caller`.
- Body: JSON. Unknown keys give 422 `extra_forbidden`.
- The session must exist: 404 `{"detail":"Session not found"}`.
- A domain `ValueError` gives 400 `{"detail":"<message>"}`. This includes a corrupt `plans.json`.
- Each verb holds a per-session lock while it reads and writes `plans.json`.
- Each successful verb broadcasts `{"type":"plan_updated","session_id":"<id>","plan_id":<id>}` on the session channel. The broadcast is not persisted.
- Each successful verb marks the sidebar state dirty.

### Version file checks

`present` and `amend` validate `file`:

1. Normalize it with `posixpath.normpath`.
2. It must resolve inside `sessions/<session_id>/`: else `file '<file>' resolves outside the session directory`.
3. It must exist: else `file '<file>' not found inside the session directory`.
4. It must pass the plan artifact assertions. This runs a headless Chrome subprocess off the event loop. A failure gives `plan artifact fails <n> check assertion(s): <name>: <detail>; …. Measure locally with: charliebot artifact check <artifact.html> --genre plan --assertions-only`.
5. No existing version may bind the same file: else `file '<file>' already bound to plan <id> v<v>`.

The stored `file` is the path relative to the session directory.

`base` is `{"repo": base_repo, "branch": base_branch, "sha": base_sha}` when any of the three is set, else null.

## `POST /api/internal/plan/present`

- Handler: `src/features/artifacts/api.py:110`.

| Field | Type | Required |
|---|---|---|
| `session_id` | string | yes |
| `file` | string | yes |
| `title` | string | yes |
| `base_repo` | string or null | no |
| `base_branch` | string or null | no |
| `base_sha` | string or null | no |

200: `{"plan": <new id>, "v": 1, "state": "awaiting approval"}`.

Side effects: appends a plan `{"id": max id + 1 (1 for the first), "title", "versions": [{"v":1,"file","created_at","trigger":"initial","base","note":null}], "takeoff": null, "closed": null}`. `created_at` is Python `isoformat()` UTC.

## `POST /api/internal/plan/amend`

- Handler: `src/features/artifacts/api.py:129`.

| Field | Type | Required | Default |
|---|---|---|---|
| `session_id` | string | yes | |
| `file` | string | yes | |
| `plan_id` | int or null | no | null |
| `trigger` | `"auto_amend"` or `"feedback"` | no | `"feedback"` |
| `note` | string | yes | |
| `base_repo`, `base_branch`, `base_sha` | string or null | no | null |

Errors (400):

- empty or blank `note`: `amend requires a non-empty --note stating why this version differs from its predecessor`;
- `plan_id` unknown: `plan <id> not found in session`;
- `plan_id` closed: `plan <id> is closed ('<as>')`;
- no `plan_id` and no open, unapproved plan: `no open lineage to amend; pass --plan to amend an approved one`;
- no `plan_id` and several open, unapproved plans: `amend requires --plan (multiple open lineages: <ids>)`.

A `plan_id` may name an approved open plan.

200: `{"plan": <id>, "v": <new version>, "state": "awaiting approval"}`.

Side effects: appends the version `{"v": max + 1, "file", "created_at", "trigger", "base", "note"}` and sets `takeoff` to null.

## `POST /api/internal/plan/approve`

- Handler: `src/features/artifacts/api.py:150`.

| Field | Type | Required |
|---|---|---|
| `session_id` | string | yes |
| `plan_id` | int or null | no |

Errors (400):

- `plan_id` unknown or closed: as in amend;
- `plan_id` already approved: `plan <id> is already approved`;
- no `plan_id` and no candidate: `no plan available for approval`;
- no `plan_id` and several candidates: `approve requires --plan (multiple candidates: <ids>)`.

200: `{"plan": <id>, "v": <latest version>, "state": "approved"}`.

Side effects: sets `takeoff` to `{"v": <latest version>, "at": <isoformat UTC>}`.

## `POST /api/internal/plan/close`

- Handler: `src/features/artifacts/api.py:164`.

| Field | Type | Required |
|---|---|---|
| `session_id` | string | yes |
| `plan_id` | int | yes |
| `close_as` | `"superseded"`, `"abandoned"` or `"completed"` | yes; another value gives 422 `literal_error` |

Errors (400): `plan <id> not found in session`; `plan <id> is already closed ('<as>')`.

200: `{"plan": <id>, "state": "<close_as>"}`.

Side effects: sets `closed` to `{"as": <close_as>, "at": <isoformat UTC>}`.

## `GET /api/sessions/{session_id}/plans`

- Auth: access key.
- Handler: `src/features/artifacts/api.py:85`.
- Dependency: `require_session`.

200, always for a known session:

```json
{
  "plans": [
    {"id": 1, "title": "…", "versions": [{"v": 1, "file": "artifacts/plan.html", "created_at": "…", "trigger": "initial", "base": null, "note": null}], "takeoff": null, "closed": null, "state": "awaiting approval"}
  ],
  "errors": [{"session_id": "…", "plan_id": 1, "error": "…"}]
}
```

- A missing `plans.json` gives `{"plans": [], "errors": []}`.
- An unreadable file, invalid JSON or a top level that is not an object gives `plans: []` and one error with `plan_id: null`. A non-object top level has the error `registry top level is <type>, expected dict`.
- A plan that fails projection or state derivation is left out, and one error names its id.
- Plan keys are `id`, `title`, `versions`, `takeoff`, `closed`, `state`. Version keys are `v`, `file`, `created_at`, `trigger`, `base`, `note`. Other stored keys are dropped.

The plan panel in the browser polls this route.

## Plan pages

The browser opens a plan version page through the file server (`/absolute_filepath<session dir>/<file>`). An HTML page under `sessions/<id>/artifacts/` gets the UI injection described in [files.md](files.md). UNVERIFIED: the plan panel URL form comes from the artifacts static script and was not traced.
