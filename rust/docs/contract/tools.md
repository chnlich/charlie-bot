# Tool routes

This file covers the small feature routes that fit no other family:

- `GET /api/code-server/open`: open a folder in code-server;
- `POST /api/diag/switch-events`: session-switch telemetry from the browser;
- `GET /api/host-auth/status` and `POST /api/host-auth/probe`: the host login panel;
- `GET /api/ext-usage`: the quota panel.

Auth for every route: access key.

## `GET /api/code-server/open`

- Handler: `src/features/code_server/api.py:76`.
- The handler is a plain `def`. Starlette runs it in its thread pool, and it blocks that thread while it waits.

| Name | In | Type | Required |
|---|---|---|---|
| `folder` | query | string | yes |

Checks, in order:

| Status | Case | `detail` |
|---|---|---|
| 404 | No code-server binary. The binary is `code_server.bin` (with `~` expanded) when set, else `code-server`, looked up on `PATH`. | `code-server not available on this host` |
| 400 | `folder` (with `~` expanded, resolved) is not a directory | `Not a directory: <folder>` |
| 400 | `folder` is not inside a `paths.workspace_dirs` entry or `paths.worktree_dir` | `folder must be under configured paths.workspace_dirs or paths.worktree_dir` |
| 500 | The code-server config file gives no port | the error text, see below |
| 503 | The spawn raised `OSError`, or the port does not answer within 5 s | `failed to start code-server` |

The config file is `code_server.config` (default `configs/code-server.yaml`). A relative path is relative to the CharlieBot repository. Its `bind-addr` value gives the port, the text after the last `:`. Config errors (500):

- `code-server config must be a YAML mapping: <path>`;
- `code-server config must define bind-addr: <path>` (also for a missing file);
- `code-server bind-addr port must be an integer: <bind-addr>`.

Side effects:

1. Probe `127.0.0.1:<port>` with a 0.2 s TCP connect.
2. When nothing answers, start `<binary> --config <config path>` in a new session, with stdin, stdout and stderr closed. The process outlives the request.
3. Poll the port every 0.2 s for up to 5 s. Stop early when the process exits (log `code_server_exited_before_listening`).

200: `{"port": <int>, "folder": "<resolved folder>"}`.

The browser opens `<page protocol>//<page hostname>:<port>/?folder=<folder>` in a new tab (`src/features/code_server/static/open-in-code-server.js`). The route does not pass the folder to code-server.

## `POST /api/diag/switch-events`

- Handler: `src/features/diag/api.py:32`.

Body (`SwitchEventRequest`, unknown keys give 422 `extra_forbidden`):

| Field | Type | Required |
|---|---|---|
| `phase` | `started`, `completed`, `superseded`, `failed` or `render_error` | yes |
| `from_session` | string or null | yes (null allowed) |
| `to_session` | string | yes |
| `generation` | int | yes |
| `winner_generation` | int or null | yes (null allowed) |
| `elapsed_ms` | int ≥ 0 or null | no, default null |
| `error` | string or null | yes (null allowed) |
| `client_ts` | string | yes |

200: `{"ok": true}`.

Side effect: one `diag_switch` log line with all eight fields. An `error` longer than 300 characters is cut to 300. Nothing is persisted.

## Host login panel

The panel tracks SSH login state for the hosts in the operator's `~/.ssh/config`. Its page is `GET /host-auth` (see [pages.md](pages.md)). Its state file is `<home>/host_auth.json` (the state contract documents it).

A background poller runs one round at startup and one every 1800 s. A round probes each due host with `ssh <alias> true` (20 s timeout) and writes the state file.

### `GET /api/host-auth/status`

- Handler: `src/features/host_auth/api.py:143`.
- Parameters: none.
- The response is a Starlette `JSONResponse`.

200: the state file content. Each host entry gets two more keys:

```json
{
  "probed_at": "2026-01-01T12:00:00+00:00",
  "probe_running": false,
  "interval_sec": 1800,
  "ttl_sec": 604800,
  "hosts": [
    {
      "alias": "agent-host",
      "hostname": "agent-host.example.com",
      "status": "ok",
      "detail": "",
      "last_probe_at": "2026-01-01T12:00:00+00:00",
      "last_change_at": "2026-01-01T12:00:00+00:00",
      "last_ok_at": "2026-01-01T12:00:00+00:00",
      "enrolled_observed_at": "2026-01-01T11:00:00+00:00",
      "estimated_expires_at": "2026-01-08T11:00:00+00:00",
      "remaining_sec": 601200.0
    }
  ]
}
```

- `status`: `ok`, `needs-okta`, `needs-interactive-auth`, `unreachable`, or null for a host never probed.
- `estimated_expires_at`: `enrolled_observed_at` plus 604800 s (`isoformat()`), or null when `enrolled_observed_at` is null.
- `remaining_sec`: float seconds from now to `estimated_expires_at`, negative after it, or null.
- With no state file, or a file that is not an object with a `hosts` list, the body is the empty state: `{"probed_at":null,"probe_running":false,"interval_sec":1800,"ttl_sec":604800,"hosts":[]}`. A bad file also logs `host_auth_state_read_failed`.
- Key order is the stored key order. The two derived keys come last in each host.

The route reads only.

### `POST /api/host-auth/probe`

- Handler: `src/features/host_auth/api.py:152`.
- Body: none.
- 303 with `Location: /host-auth` and an empty body, always.

Side effects:

- When no round runs (a process-wide flag), set the flag and start a background forced round (task `host-auth-manual-round`). A forced round probes every host and ignores the backoff.
- When a round runs, start nothing.

The round:

1. Sets `probe_running: true` in the state file.
2. Starts one `ssh` process per host, all at once.
3. Writes the results, sets `probed_at`, and clears `probe_running`.

A failure logs `host_auth_manual_round_failed`.

## `GET /api/ext-usage`

- Handler: `src/features/usage/ext_usage.py:141`.
- Parameters: none.

200, before the poller has filled the cache:

```json
{"error": "Usage data not yet available"}
```

200, after that:

```json
{
  "providers": {
    "claude:work": {
      "provider": "claude",
      "windows": [{"window_minutes": 300, "utilization": 42.0, "resets_at": "2026-01-01T17:00:00+00:00"}],
      "fetched_at": "2026-01-01T12:00:00+00:00",
      "account": "work",
      "provider_label": "Claude"
    }
  }
}
```

- Keys are `<provider>:<account label>`, in the order the usage sources register the accounts.
- An entry is one of three kinds:
  - a reading: the account's payload plus `account`;
  - a pending marker, for an account not yet read: `{"provider", "account", "pending": true}`;
  - an error: `{"provider", "account", "error": "<last error>"}`.
- Every entry gets `provider_label` last.
- A window has `window_minutes`, `utilization` (float percentage or null) and `resets_at` (ISO string, `""` when unknown). A model-scoped window adds `scope_label`.
- At read time, a window whose reading is expired gets `expired: true`. The server stores no such mark.
- Some accounts also set `login_required` and `state_label` on the entry.

UNVERIFIED: the full payload of each account comes from the usage sources (for example `src/backends/claude_code/claude_accounts.py`). This file lists the shared panel keys from `src/runtime/hooks/usage_sources.py:93`, comment above `PANEL_WINDOWS`.

The route reads the in-memory cache only. The poller fetches one account every 60 s and broadcasts `{"type":"ext_usage","providers":…}` on the sidebar channel after each fetch (see [websocket.md](websocket.md)).
