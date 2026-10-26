# Auth

The server has one shared secret, the access key. It is the `charliebot.access_key` value in `credentials.yaml` (`src/infra/credentials.py:182`). The state contract documents that file.

When the access key is empty or absent, auth is off: every HTTP request and every WebSocket connection passes.

Auth has three layers:

1. `AuthMiddleware` (`src/runtime/api/auth.py:173`) gates every HTTP request.
2. The `require_caller` dependency (`src/runtime/api/deps.py:108`) resolves the caller identity on some routes.
3. `check_ws_auth` (`src/runtime/api/auth.py:157`) gates the WebSocket routes. See [websocket.md](websocket.md#auth).

## Auth values in routes.tsv

| Value | Meaning |
|---|---|
| `public` | The middleware lets the request through with no credential. |
| `access key` | The middleware gate. A validly signed run token also passes it. |
| `access key + caller` | The middleware gate, then `require_caller`. |
| `access key + caller, operator only` | As above. The handler then refuses an agent caller. |
| `access key + caller, own run` | As above. An agent caller may act only on its own run. |
| `ws token` | `check_ws_auth`: the `token` query parameter must equal the access key. |

## Middleware gate

Handler: `src/runtime/api/auth.py:190`. It runs only for `http` scopes.

### Public paths

These paths pass with no credential:

- exact path `/`;
- exact path `/api/auth/status`;
- any path that starts with `/static/`.

The match uses the raw ASGI `path`. The query string does not take part. `/static` without the trailing slash is not public.

### Credential order

1. Read the first `Authorization` header. When its value starts with `Bearer ` (with one space), the rest is the bearer.
2. When a bearer is present:
   - a bearer equal to the access key passes (constant-time compare);
   - any other bearer must be a validly signed run token (see below), and then it passes;
   - otherwise the request fails. The middleware does not read the cookie in this case.
3. When no bearer is present, read the `Cookie` headers. Join them with `;` and parse them with Python `http.cookies.SimpleCookie`. A cookie named `charliebot_access_key` equal to the access key passes. A cookie parse error counts as no cookie.

A run token with a valid signature passes the middleware for every route. The middleware does not check that the run is active. Only the `require_caller` routes check that.

### Rejection

Every rejection has status 401. The middleware writes the response itself, so no other middleware header is added (no gzip, no `vary`).

| Request | Response |
|---|---|
| Method `GET` and the `Accept` header contains `text/html` | The login page, `content-type: text/html; charset=utf-8` |
| Any other request | `{"detail": "Unauthorized"}`, `content-type: application/json` |

The JSON body is the Python `json.dumps` form, with a space after the colon: `{"detail": "Unauthorized"}` (26 bytes).

Both responses set `content-length`. The `http_request` log line records the 401.

### Login page

The login page is the `_LOGIN_PAGE` string at `src/runtime/api/auth.py:43`. Its UTF-8 body is 2067 bytes. The Rust port must serve the same bytes.

The page has a password input and an "Unlock" button. On submit, the script:

1. trims the input and stops when it is empty;
2. stores the key in `localStorage` under `charliebot_access_key`;
3. sets the cookie `charliebot_access_key=<key>; path=/; SameSite=Strict`, plus `; Secure` when the page is on `https:`;
4. reloads the page.

The browser code reads `localStorage` for the `Authorization` header on fetch calls and for the `token` parameter on WebSocket URLs.

## Run tokens

A run token is an HMAC-signed claim that a request comes from one agent run. Module: `src/runtime/run_token.py`.

Wire form: `<payload_b64>.<signature_b64>`.

- `payload_b64` is unpadded base64url of the compact JSON `{"agent":…,"run_id":…,"session_id":…}`. The keys are sorted. The separators are `,` and `:` with no spaces.
- `signature_b64` is unpadded base64url of HMAC-SHA256. The key is the access key (UTF-8). The message is the ASCII `payload_b64` string.

Verification (`verify_run_token`, `src/runtime/run_token.py:119`) fails with these reasons:

| Reason | Cause |
|---|---|
| `run token presented but no signing key is configured` | The access key is empty. |
| `malformed run token` | The token has no `.`. |
| `malformed run token signature` | The signature part is not base64url. |
| `run token signature mismatch` | The HMAC differs. |
| `malformed run token payload` | The payload is not base64url JSON. |
| `run token payload must be an object` | The payload JSON is not an object. |
| `run token payload must bind session_id, run_id and agent` | A claim is missing, empty or not a string. |

The server passes the token to an agent process in the environment variable `CHARLIEBOT_RUN_TOKEN`.

## Caller identity (`require_caller`)

Source: `src/runtime/api/deps.py:129`. The result is an operator identity or an agent identity.

1. No bearer: operator. The operator's session is the `X-CharlieBot-Caller-Session` header value, or none when the header is absent or empty.
2. Bearer equal to the access key: operator, with the same header rule.
3. Any other bearer, with no access key configured: verification fails with `run token presented but no signing key is configured`, so 401 `invalid run token: run token presented but no signing key is configured`.
4. A bearer that fails verification: 401 `invalid run token: <reason>`.
5. A verified token whose run fails the active-run check (`src/runtime/run_identity.py:32`):
   - no run record for (`session_id`, `run_id`), or a `run_finished` event for the run: 401 `run token does not reference an active run`;
   - the run record has no `pid` or no `pid_start`: 401 `run token references a run that has not launched`.
6. Otherwise: an agent identity with the token claims. The agent's session is the claim `session_id`. The `X-CharlieBot-Caller-Session` header is ignored.

Each 401 body is the FastAPI form `{"detail":"<message>"}`.

Point 1 means: on a `require_caller` route, a cookie-authenticated browser request is an operator request.

### Routes that use `require_caller`

| Route | Handler line |
|---|---|
| `POST /api/sessions/` | `src/runtime/api/sessions.py:407` |
| `POST /api/sessions/{session_id}/fork` | `src/runtime/api/sessions.py:1215` |
| `POST /api/sessions/{session_id}/elone` | `src/runtime/api/sessions.py:1256` |
| `POST /api/sessions/{session_id}/archive` | `src/runtime/api/sessions.py:1396` |
| `DELETE /api/sessions/{session_id}/permanent` | `src/runtime/api/sessions.py:1422` |
| `POST /api/sessions/{session_id}/unarchive` | `src/runtime/api/sessions.py:1439` |
| `PATCH /api/sessions/{session_id}` | `src/runtime/api/sessions.py:1496` |
| `POST /api/sessions/{session_id}/retry` | `src/runtime/api/sessions.py:1715` |
| `POST /api/sessions/{session_id}/task-inputs/acknowledge` | `src/runtime/api/sessions.py:1773` |
| `POST /api/sessions/{session_id}/cancel` | `src/runtime/api/sessions.py:1804` |
| `POST /api/sessions/{session_id}/runs/{run_id}/cancel` | `src/runtime/api/sessions.py:1841` |
| `POST /api/chat/{session_id}/message` | `src/runtime/api/chat.py:48` |
| `POST /api/internal/delegate` | `src/runtime/api/internal.py:266` |
| `POST /api/internal/session-message` | `src/runtime/api/internal.py:371` |
| `POST /api/internal/slack/reply` | `src/features/slack/api.py:31` |
| `POST /api/internal/discord/reply` | `src/features/discord/api.py:37` |
| `POST /api/internal/web/reply` | `src/features/web_reply/api.py:72` |

The per-route rules after identity resolution are in [sessions.md](sessions.md), [chat.md](chat.md) and [internal.md](internal.md).

## Clients

The `charliebot` CLI (`src/runtime/cli/common.py:396`) sends:

- `Authorization: Bearer <CHARLIEBOT_RUN_TOKEN>` when that variable is set;
- else `Authorization: Bearer <access key>` when a key is configured;
- else no `Authorization` header;
- `X-CharlieBot-Caller-Session: <CHARLIEBOT_SESSION_ID>` when that variable is set, with either bearer.

A Claude Code agent on an openai-compatible backend calls the proxy route with `ANTHROPIC_AUTH_TOKEN` set to the access key (`src/backends/openai_compatible/factory.py:14`). See [proxy.md](proxy.md).

## `GET /api/auth/status`

- Auth: public.
- Handler: `src/app/pages.py:53`.
- Parameters: none.
- 200: `{"auth_enabled": <bool>}`. The value is true when the access key is non-empty.
- The body uses Starlette `JSONResponse` (compact separators): `{"auth_enabled":true}`.
- `HEAD` gives 405 with `allow: GET`.
- Side effects: none.

No code in the repository calls this route. The index page gets the same value from its template context (`auth_enabled`). The route stays for external clients.

## Security observations

These are properties of the Python server. The Rust port must keep them for parity. A later task can change them on purpose.

- `GET /` is public. With a `?session=<id>` query, or with any session in the sidebar, it renders session names, the first page of messages and `SESSIONS_ROOT` into the HTML with no credential. See [pages.md](pages.md).
- A validly signed run token passes the middleware on every route, including routes that do not call `require_caller`. A token of a finished run keeps this access.
- A WebSocket rejection is an HTTP 403 on the wire under uvicorn, not the close code 4401 that the handler asks for.
