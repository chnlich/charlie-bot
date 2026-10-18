# Git diff API

The diff viewer page (`GET /diff`, see [pages.md](pages.md#get-diff)) calls these routes. They live in `src/features/diff_view/api.py`, mounted at `/api/git`.

Shared rules:

- Auth: access key.
- Every route runs `git` subprocesses off the event loop. They read the repository only.
- A failing git command gives 500 `{"detail": "<git stderr, stripped>"}`. With empty stderr, the detail is `git diff failed`, `git rev-parse failed` or `git branch failed`.
- A git command past its timeout raises `subprocess.TimeoutExpired`, which gives the plain 500 `Internal Server Error`. The timeout is 10 s for `rev-parse` and `branch`, and 30 s for `diff`.

## Repository check

`/diff/files` and `/diff/file` resolve `repo` with `_resolve_repo_under_workspace` (`src/features/diff_view/api.py:82`):

1. Expand `~` and resolve the path.
2. `<repo>/.git` must exist (a directory or a worktree file). Else 400 `{"detail":"Not a git repo: <repo as given>"}`.
3. The path must be inside one of `paths.workspace_dirs` or a package diff root (`wiring.diff_roots()`). Else 400 `{"detail":"repo must be under configured paths.workspace_dirs or a registered package repository"}`.

`/branches` uses a weaker check (see below).

## `GET /api/git/branches`

- Handler: `src/features/diff_view/api.py:318`.

| Name | In | Type | Required |
|---|---|---|---|
| `repo` | query | string | yes |

- Expand `~` (no resolve). When `<repo>/.git` does not exist and the path's last part is not `.git`: 400 `Not a git repo: <repo>`.
- A path ending in `.git` uses its parent.
- No workspace-root check.

The server runs `git branch -a --sort=-committerdate --format=%(refname:short)`. It memoizes the output per (repo, ref-state signature).

200: a list of at most 50 names. The first is `HEAD`. Then the output lines in order, stripped, without empty lines, `origin` and `HEAD`, and without duplicates.

## `GET /api/git/repos`

- Handler: `src/features/diff_view/api.py:346`.
- Parameters: none.
- 200: `[{"label": "<directory name>", "path": "<resolved absolute path>"}]`, sorted by `label`.

The list comes from `CharlieBotConfig.discover_repos` (`src/infra/config.py:313`):

1. Scan each `paths.workspace_dirs` entry one level deep. Skip an entry that is not a directory.
2. Keep each child directory that holds `.git`.
3. Keep the first entry per resolved path.

## `GET /api/git/diff/files`

- Handler: `src/features/diff_view/api.py:353`.

| Name | In | Type | Required | Default |
|---|---|---|---|---|
| `repo` | query | string | yes | |
| `base` | query | string (git ref) | yes | |
| `head` | query | string (git ref) | yes | |
| `mode` | query | `three-dot` or `two-dot` | no | `three-dot` (another value gives 422 `literal_error`) |

Steps:

1. Repository check (400).
2. Resolve `base` and `head` to commit SHAs with one `git rev-parse <ref>^{commit} …` call.
3. Range: `<base>...<head>` for `three-dot`, `<base>..<head>` for `two-dot`. The range uses the refs as given.
4. Run `git diff --name-status -z <range>` and `git diff --numstat -z <range>` at the same time.
5. Memoize the result per (repo, base SHA, head SHA, mode, `.gitattributes` mtime and size).

200:

```json
{
  "files": [{"path": "src/a.py", "status": "M", "additions": 3, "deletions": 1}, {"path": "src/new.py", "status": "R", "additions": 0, "deletions": 0, "old_path": "src/old.py"}],
  "base": "<base as given>",
  "head": "<head as given>",
  "head_sha": "<full sha>",
  "mode": "three-dot",
  "total_files": 2,
  "total_additions": 3,
  "total_deletions": 1
}
```

- `files` follow the numstat order.
- `status` is the first letter of the name-status code (`A`, `M`, `D`, `R`, `C`, `T`, …).
- A binary file counts 0 additions and 0 deletions.
- `old_path` is present only for a rename or copy.

## `GET /api/git/diff/file`

- Handler: `src/features/diff_view/api.py:414`.

| Name | In | Type | Required | Default |
|---|---|---|---|---|
| `repo` | query | string | yes | |
| `base` | query | string | yes | |
| `head` | query | string | yes | |
| `mode` | query | `three-dot` or `two-dot` | no | `three-dot` |
| `path` | query | string (repository-relative) | yes | |
| `old_path` | query | string | no | null |
| `force` | query | bool | no | false |

Steps:

1. Repository check (400).
2. Resolve `base` and `head` to SHAs.
3. Run `git diff <base sha>...<head sha> -- [<old_path>] <path>` (`..` for `two-dot`).

200, when the diff is at most 5 MiB (5 242 880 bytes as UTF-8) or `force` is true:

```json
{"diff": "<unified diff text>", "path": "<path>", "size_bytes": 1234}
```

200, when the diff is larger and `force` is false:

```json
{"too_large": true, "size_bytes": 9000000, "path": "<path>"}
```
