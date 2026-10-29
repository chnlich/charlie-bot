# Pages

The server renders HTML pages with Jinja2 (`src/runtime/templating.py`). This file covers the page routes:

- `GET /`
- `GET /home`
- `GET /sessions/{session_id}/events`
- `GET /diff`
- `GET /host-auth`
- `GET /perfetto`
- `GET /token-usage`

The file-server directory listing is in [files.md](files.md). The login page is in [auth.md](auth.md#login-page).

Every page response is a Starlette `TemplateResponse`:

- `content-type: text/html; charset=utf-8`;
- status 200 unless the route says otherwise;
- no `Cache-Control` header in Python. The Rust port sends `Cache-Control: no-store` on every templated
  page (`Templating::render_page`, `rust/crates/runtime/registry/src/templating.rs`), so a back navigation
  or a restored tab re-fetches the page instead of running a cached copy's older JavaScript;
- gzip per [middleware.md](middleware.md#gzip). Pages are larger than 1000 bytes, so a gzip client gets a compressed page.

A page route answers `GET` only. `HEAD` gives 405.

## Template engine

The engine is built on the first render (`templates()`, `src/runtime/templating.py:189`).

Template search path, in order:

1. `web/templates`
2. `src/<kind>/<name>/templates` for each page package, in registration order (see [static.md](static.md#mounts) for the order).

Two directories must not hold the same template name. The engine raises `ValueError` at build time when they do.

### Template globals

| Name | Value |
|---|---|
| `page_parts(slot)` | The template names of the page parts for `slot` that exist on disk, in registration order. A part is `src/<kind>/<name>/templates/<name>/page_<slot>.html`. An unknown slot raises `ValueError`, so the render fails with 500. |
| `code_server_enabled()` | Whether the code-server feature can start (`src/features/code_server/api.py`). |
| `voice_backends()` | A list of `{"id", "label", "live_partials", "unavailable_reason"}`, one per transcription backend in registry order (`src/features/voice/page_globals.py:7`). |
| `voice_default_backend()` | The config value `voice.default_backend`. |

Slots: `head`, `tab_buttons`, `below_header`, `tab_panels`, `input_buttons`, `modals`, `scripts`, `diff_toolbar`, `home_notes`.

Page parts at the baseline:

| Slot | Parts |
|---|---|
| `head` | `artifacts`, `terminal`, `usage`, `voice`, `claude_code`, `codex` |
| `tab_buttons` | `artifacts`, `terminal` |
| `below_header` | `usage` |
| `tab_panels` | `artifacts`, `terminal` |
| `input_buttons` | `voice` |
| `modals` | `cron` |
| `scripts` | `artifacts`, `chat_threads`, `cron`, `diag`, `diff_view`, `explain`, `terminal`, `usage`, `voice` |
| `diff_toolbar` | `code_server` |
| `home_notes` | `trace` |

`index.html` places `head`, `tab_buttons`, `below_header`, `input_buttons`, `tab_panels`, `scripts` and `modals`. `diff.html` places `diff_toolbar`. `home.html` places `home_notes`.

### Static asset version

`static_asset_version()` (`src/runtime/templating.py:140`) returns `<git part>-<digest>`.

- The git part comes from `git_version()`. That is `<short sha> · <MM-DD of the HEAD commit date>`, with ` · ` and spaces replaced by `-`. Example: `abc1234-10-17`.
- `git_version()` is `""` when git fails. Then the token starts with `-`.
- The digest is the first 12 hex characters of a SHA-1 over the served static trees (`web/static` and each package `static/`). For each file, sorted by URL path under `/static/`, the hash takes: the URL path, a zero byte, and the SHA-1 digest of the file bytes.
- The digest is `""` when `web/static` is absent.

The server computes `git_version()` once per process. It recomputes the digest on each render and re-hashes only the files whose (mtime, size) changed.

Templates use the token as `?v={{ static_asset_version|urlencode }}` on static URLs.

## `GET /`

- Auth: public. See the security note in [auth.md](auth.md#security-observations).
- Handler: `src/app/pages.py:128`.
- Template: `web/templates/index.html`.

### Parameters

| Name | In | Type | Required | Default |
|---|---|---|---|---|
| `session` | query | string | no | null |

### Behavior

1. Load the sidebar list: active, non-scheduled sessions with running and pending-trigger state. Remove the sessions that belong to a sequence subtree or to any sidebar view subtree (for example chat-platform threads).
2. A failure in step 1 is logged as `list_sessions_failed`. The list is then empty, and `load_errors` gets `Failed to load sessions. Check server logs for details.`
3. When `session` is absent and the list is not empty, return 307 with `location: /?session=<first session id>`. The location is a relative URL. The body is empty.
4. When `session` is a non-empty string, load that session. When it exists, build its bootstrap (the same data as `GET /api/sessions/{session_id}/bootstrap`). Mark that sidebar row as read in the rendered copy only.
5. A failure in step 4 logs `load_session_data_failed` and adds `Failed to load session data. Check server logs for details.` to `load_errors`.
6. An unknown `session` id, or `?session=` with an empty value, renders the page with no active session.

The route does not write to disk. Rendering the page does not mark the session as read; the browser calls `POST /api/sessions/{session_id}/read`.

### Context

| Variable | Type | Value |
|---|---|---|
| `initial_sessions_json` | string | The sidebar rows as Jinja `tojson` text: the listing rows (each the full session JSON, `task.goal` included, with the six model `schedule_*` fields replaced by the listing fields; see [shapes.md](shapes.md#listing-rows)) with sorted keys and the `< > & '` escapes. The handler renders the text, so the page does not walk every row through the template engine. |
| `active_session` | session or null | The active session model. |
| `pending_draft` | object or null | The bootstrap `pending_draft`. |
| `event_count` | int | The bootstrap total event count. 0 with no active session. |
| `session_bootstrap` | object or null | The same JSON as the bootstrap route. |
| `backend_options` | list | The configured backend options (`id`, `label`, `type`, …). |
| `backend_compact` | object | Per backend type: `{"manual": bool, "hint": string}`. |
| `default_backend_id` | string | The first configured backend id, or `claude` when none is configured. |
| `active_backend` | string | `run_backend` or `backend` of the active session, else the default id. |
| `active_backend_label` | string | The label of that option, or the id when the option is unknown. |
| `active_backend_type` | string | The type of that option, or `""`. |
| `load_errors` | list of strings | See the behavior steps. |
| `auth_enabled` | bool | True when an access key is configured. |
| `hostname` | string | `socket.gethostname()`. |
| `sessions_root` | string | The absolute sessions directory. |
| `version` | string | `git_version()`, for example `abc1234 · 10-17`. |
| `static_asset_version` | string | See above. |

The template uses these expressions, among others:

- `{{ hostname }}` in the title `CharlieBot@<hostname>`;
- `window.SESSIONS_ROOT = {{ sessions_root|tojson }}`;
- `{{ session_bootstrap|tojson }}`, `{{ initial_sessions_json|safe }}`, `{{ load_errors|tojson }}`, `{{ backend_compact|tojson }}`, `{{ auth_enabled|tojson }}`;
- `active_session.id`, `active_session.name`, `active_session.profile` and `active_session.thinking_since.isoformat()`;
- `pending_draft.event_index` and `pending_draft.content`;
- the includes `partials/prologue.html`, `partials/close_button.html` and `partials/welcome_view.html`.

Jinja's `tojson` filter sorts object keys (`json.dumps_kwargs` policy `sort_keys=True`). It escapes `<`, `>`, `&` and `'` as `\u003c`, `\u003e`, `\u0026` and `\u0027`.

## `GET /home`

- Auth: access key.
- Handler: `src/app/pages.py:89`.
- Template: `web/templates/home.html`.
- Parameters: none.

The handler probes each configured service in `ui.home_services` (`name`, `description`, `url`):

- It parses the host and port from `url`. The port defaults to 443 for `https` and 80 for other schemes.
- It opens a TCP connection with a 0.3 s timeout (`HOME_SERVICE_PROBE_TIMEOUT`). The probes run at the same time, off the event loop.
- A failed connect, a bad URL or a URL with no host means `down`.

Context:

| Variable | Value |
|---|---|
| `hostname` | `socket.gethostname()` |
| `destinations` | The cards: first `{"name":"Chat","url":"/","description":"The CharlieBot chat and session UI."}`, then the registered home cards. |
| `services` | `[{"name","description","url","status"}]` in config order. `status` is `up` or `down`. |

Registered home cards, in order:

1. `Host login authorization`, `/host-auth`, `Per-host ssh login state and the estimated Okta renewal deadline.`
2. `Diff viewer`, `/diff`, `Browse a repository diff between two refs.`
3. `Token usage by model`, `/token-usage`, `Tokens per model across every agent log on this host.`
4. `File browser`, `/absolute_filepath/`, `Browse any file on this host's filesystem.`

The page has no static-asset version. It persists nothing.

## `GET /sessions/{session_id}/events`

- Auth: access key.
- Handler: `src/app/pages.py:59`.
- Template: `web/templates/events_viewer.html`.

| Name | In | Type | Required |
|---|---|---|---|
| `session_id` | path | string | yes |

Responses:

| Status | Case | Body |
|---|---|---|
| 200 | Session found | HTML |
| 404 | Session not found, or the store raised `KeyError` or `FileNotFoundError` | `{"detail":"Session not found"}` |
| 500 | The store raised another exception | `{"detail":"Failed to load session"}` |

Context:

| Variable | Value |
|---|---|
| `session` | the session model (the template reads `session.name`) |
| `session_id` | the path value |
| `events_url` | `/api/sessions/<session_id>/events.jsonl` |
| `hostname` | `socket.gethostname()` |
| `static_asset_version` | see above |

The page script fetches `events_url` in the browser.

## `GET /diff`

- Auth: access key.
- Handler: `src/features/diff_view/page.py:13`.
- Template: `src/features/diff_view/templates/diff.html`.
- Parameters: none. The page script reads its own query string in the browser.
- Context: `hostname`, `static_asset_version`.
- 200 only. The page calls the routes in [git.md](git.md).

## `GET /host-auth`

- Auth: access key.
- Handler: `src/features/host_auth/api.py:123`.
- Template: `src/features/host_auth/templates/host_auth.html`.
- Parameters: none.

The handler reads the host-auth state file (`host_auth.json` under the home; the state contract documents it). A missing or corrupt file gives the empty state.

Context:

| Variable | Value |
|---|---|
| `probe_running` | bool |
| `hosts` | One row per state host, in state order. Row keys: `alias`, `hostname`, `status`, `status_label`, `status_class`, `detail_lines`, `renew_cmd` (`ssh <alias> true`), `has_url`, `baseline`, `last_probe`, `deadline_kind`, `remaining`, `expires`, `under_24h`. |
| `summary` | `{"blocked": [aliases with status needs-okta], "soonest_alias", "soonest_remaining"}` |
| `last_probe` | `probed_at` as local time `%Y-%m-%d %H:%M %Z`, or null when absent |
| `interval_label` | `30 min` |
| `backoff_label` | `6 h` |
| `ttl_label` | `7 days` |

Status values (`src/features/host_auth/api.py:22`):

| `status` | `status_label` | `status_class` |
|---|---|---|
| `ok` | `Direct` | `ok` |
| `needs-okta` | `Needs Okta` | `okta` |
| `needs-interactive-auth` | `Needs interactive auth` | `iact` |
| `unreachable` | `Unreachable` | `down` |

Row rules:

- `baseline` is `enrolled_observed_at` as local `%Y-%m-%d %H:%M %Z`, or null.
- `last_probe` is `last_probe_at` as local `%H:%M`, or null.
- `has_url` is true when the detail text contains `http`.
- `deadline_kind` is `not_applicable` by default and `pending_okta` for `needs-okta`.
- For `ok`: `expires` is the estimated expiry. `deadline_kind` is `no_baseline` with no baseline, `expired` when the remaining time is 0 or less, else `counting`.
- For `counting`: `remaining` is the two largest units (`3 d 21 h`, `12 h 36 m` or `9 m`), and `under_24h` is true below 86 400 s.
- `summary.soonest_*` names the `ok` host with the smallest positive remaining time.

The page has no JavaScript. It persists nothing.

## `GET /perfetto`

- Auth: access key.
- Handler: `src/features/trace/api.py:78`.
- Template: `src/features/trace/templates/perfetto.html`.

| Name | In | Type | Required | Default |
|---|---|---|---|---|
| `trace` | query, repeatable | list of strings | no | `[]` |
| `dir` | query | string | no | null |
| `pattern` | query | string | no | `*.json` |
| `title` | query | string | no | null |
| `slim` | query | bool | no | null |

Input rules:

- A `trace` value that starts with `/absolute_filepath/` keeps that URL. Its local path is the rest after `/absolute_filepath`.
- A `trace` value that starts with `/` becomes the URL `/absolute_filepath<value>`. Its local path is the value.
- Any other `trace` value is a remote URL with no local path.
- With `dir`, the handler lists the regular files in that directory whose names match `pattern` (`fnmatch`), sorted by name. A missing directory adds nothing and logs `perfetto_dir_not_found`.

| Status | Case | Body |
|---|---|---|
| 400 | No input after discovery | `{"detail":"No trace files specified. Provide 'trace' or 'dir' query params."}` |
| 200 | Otherwise | HTML |

`trace_url` rules:

- When every input is a local regular file whose first non-space byte in the first 64 bytes is `{` or `[`, `trace_url` is `/perfetto/merged?<query>`. The query has the original `trace` values, then `dir` and `pattern` when `dir` is set, then `slim=True` or `slim=False` when `slim` is set.
- Otherwise `trace_url` is the URL of the first input. With more than one input, `warn` is `⚠ Remote or non-JSON traces cannot be merged, showing first trace only`.

Context:

| Variable | Value |
|---|---|
| `trace_url` | see the rules above |
| `title` | `title`, else `dir`, else the last path segment of the first input URL |
| `warn` | string or null |
| `merge_count` | number of inputs |
| `is_merge` | `trace_url` starts with `/perfetto/merged`, and there is more than one input or `slim` is true |
| `is_direct_pass` | `trace_url` starts with `/perfetto/merged` and `is_merge` is false |

The route persists nothing. The browser fetches `trace_url`. See `GET /perfetto/merged` in [files.md](files.md).

## `GET /token-usage`

- Auth: access key.
- Handler: `src/features/usage/api.py:304`.
- Template: `src/features/usage/templates/token_usage.html`.
- Parameters: none.

The handler reads the usage ledger in a thread (`_read_ledger`, `src/features/usage/api.py:76`). It does not capture new usage. A ledger read error propagates as a 500.

Context keys:

| Variable | Value |
|---|---|
| `ctx` | The aggregate figures: `rows` (merged per-model rows), the compact totals (`tot_compact`, `in_compact`, `out_compact`, `cr_compact`, `cw_compact`, `fresh_compact`), `fresh_percent`, `out_share`, `per_src`, `usage_sources`, `tot_calls`, `top_escaped`, `top_compact`, `top_out_escaped`, `top_out_compact`, `elapsed_s` |
| `payload` | `json.dumps({"rows": merged}, ensure_ascii=False)` for the page charts |
| `window` | (first, last) timestamps over the rows, or `("", "")` |
| `window_str` | `<first> → <last>`, or `""` with no rows |
| `cache_share` | cache-read tokens as a percentage of all tokens |
| `generated` | local time now, `%Y-%m-%d %H:%M %Z` |
| `as_of` | the last capture time for this host |

UNVERIFIED: the field sets of `rows` and `per_src` come from `_token_usage_context` (`src/features/usage/api.py:216`) and the ledger schema; the state contract owns the ledger schema.
