# Middleware and error responses

## Order

The app is built in `server.py:326`. The middleware stack, outermost first:

1. `ServerErrorMiddleware` (Starlette built-in). It turns an unhandled exception into a 500 response.
2. `_RequestLogMiddleware` (`server.py:148`). It writes one `http_request` log line per HTTP response.
3. `AuthMiddleware` (`src/runtime/api/auth.py:173`). See [auth.md](auth.md).
4. `_CharlieBotGZipMiddleware` (`server.py:137`), with `minimum_size=1000` and `compresslevel=1`.
5. `ExceptionMiddleware` (Starlette built-in). It renders `HTTPException` and validation errors.
6. `AsyncExitStackMiddleware` (FastAPI built-in).
7. The router.

`server.py:336-340` adds 4, then 3, then 2. Starlette inserts each one at the front, so the last one added is the outermost user middleware.

Consequences of the order:

- The request log records the 401 responses of the auth layer.
- The auth layer's 401 responses are never gzipped.
- An unhandled exception reaches `_RequestLogMiddleware`, which logs it and re-raises. Then `ServerErrorMiddleware` sends the 500.
- The 500 response of `ServerErrorMiddleware` is never gzipped.

WebSocket scopes pass through items 2 to 4 with no effect.

## Request log

Every HTTP request produces exactly one structlog event named `http_request` at level `info`.

| Field | Type | Value |
|---|---|---|
| `method` | string | The request method. |
| `path` | string | The ASGI `path`. The query string is not included, because the terminal URL can carry `?token=`. |
| `status` | int or null | The status of the `http.response.start` message. Null when the app sent no start message. |
| `duration_ms` | int | Milliseconds from the middleware entry to the app return, rounded. A streaming response counts until its last body chunk. |
| `client` | string | The client IP from the ASGI `client` tuple, or `-` when it is absent. |
| `error` | string | Only on the exception path: the exception class name, for example `RuntimeError`. |

On the exception path, `status` is 500 and the line is written before the exception propagates.

The server writes each log line to stdout in the grammar of `charliebot_core::logline`, the one module that writes and reads it:

- the UTC time to the millisecond, `YYYY-MM-DD HH:MM:SS.mmmZ`;
- the level in brackets, padded to nine columns;
- the source location of the tracing call as `<path>:<line>`, padded to 30 columns, so the event names stay aligned and a longer location pushes only its own line;
- the event name, padded to 30 columns;
- the fields as `key=value`, in the order the event logs them;
- a value that is empty, or holds whitespace, a control character, `=`, `'` or `"`, is quoted with Python's `repr(str)` escapes, so one event stays on one line.

The location path is the compiler's file path, shortened: a workspace path `crates/<group>/<crate>/src/<rest>` writes `<crate>/<rest>` (so `crates/app/server/src/request_log.rs` writes `server/request_log.rs`), a cargo registry path `…/registry/src/<index>/<package>-<version>/<rest>` writes `<package>-<version>/<rest>`, and any other path stays as the compiler gave it. The line number is the line where the macro call starts.

Example (synthetic values):

```text
2026-01-01 12:00:00.000Z [info     ] server/request_log.rs:39       http_request                   method=GET path=/api/sessions/ status=200 duration_ms=4 client=127.0.0.1
```

The Python server rendered the same layout with structlog's console renderer, in local time with the fields sorted by key, and wrote no location column.

uvicorn runs with `log_level="warning"` (`server.py:600`). Thus uvicorn writes no access log. The `http_request` line is the only request log.

## Gzip

### When the middleware compresses

The middleware looks at the request `Accept-Encoding` header. It compresses only when the header value contains the substring `gzip`. A value such as `gzip;q=0` still counts as gzip.

With gzip accepted, the responder (`_IsalGZipResponder`, `server.py:93`) applies these rules in order:

1. A response that already has a `Content-Encoding` header passes unchanged.
2. A response whose `content-type` starts with an entry of `_TRANSPORT_GZIP_SKIP_MEDIA_PREFIXES` passes unchanged. No `Vary` header is added.
3. A response with one body message shorter than 1000 bytes passes unchanged. No `Vary` header is added.
4. A response with one body message of 1000 bytes or more is compressed. The middleware sets `Content-Encoding: gzip`, sets `Content-Length` to the compressed size, and adds `Accept-Encoding` to `Vary`.
5. A streaming response (first body message has `more_body: true`) is compressed whatever its size. The middleware sets `Content-Encoding: gzip`, removes `Content-Length`, and adds `Accept-Encoding` to `Vary`.

The compressor is ISA-L (`isal.igzip`) at level 1 with `mtime=0`. The gzip bytes need not match the Python output. Clients only decompress them.

`_TRANSPORT_GZIP_SKIP_MEDIA_PREFIXES` (`server.py:68`) is a prefix match on the `content-type` value:

- `text/event-stream`
- `image/gif`
- `image/heic`
- `image/jpeg`
- `image/png`
- `image/webp`
- `image/x-icon`
- `image/vnd.microsoft.icon`
- `video/`
- `audio/`
- `application/pdf`
- `application/zip`
- `application/x-zip-compressed`
- `application/x-7z-compressed`
- `application/gzip`
- `application/x-gzip`
- `application/vnd.openxmlformats-officedocument`
- `application/vnd.ms-powerpoint`
- `application/msword`
- `font/woff` (also matches `font/woff2`)
- `application/font-woff`

`image/svg+xml`, HTML, JSON, CSS, JavaScript, CSV and plain text are compressed.

### Identity path quirk

When `Accept-Encoding` does not contain `gzip`, Starlette's `IdentityResponder` runs. It does not compress. It still adds `Vary: Accept-Encoding` in these cases:

- one body message of 1000 bytes or more;
- a streaming response.

On this path the exclusion check uses only `text/event-stream`, and a response with `Content-Encoding` passes unchanged. Thus a 2000-byte PNG served with no gzip in `Accept-Encoding` gets `Vary: Accept-Encoding`. The Rust port must keep this header behavior.

### Pre-compressed routes

Some handlers compress their own body. They do this only when `Accept-Encoding` contains `gzip` (`src/infra/responses.py:73`). They then set `Content-Encoding: gzip` and `Vary: Accept-Encoding` at any size, even for an empty list `[]`. The middleware passes these responses unchanged (rule 1).

These routes pre-compress:

- `GET /api/sessions/`
- `GET /api/sessions/chat-threads`
- `GET /api/sessions/status`
- `GET /api/sessions/search`
- `GET /api/sessions/{session_id}/bootstrap`
- `GET /api/sessions/{session_id}/events`
- `GET /api/sessions/{session_id}/events.jsonl`

When the client does not accept gzip, these routes send the plain body. The middleware then applies the identity rules.

## Framework error responses

The app registers no custom exception handler. FastAPI and Starlette defaults apply.

| Case | Status | Body | Content type |
|---|---|---|---|
| `HTTPException` | its status | `{"detail":<detail>}`. `detail` is a string, an object or a list, as raised. | `application/json` |
| Request validation error | 422 | `{"detail":[<error>,…]}` | `application/json` |
| No route matches | 404 | `{"detail":"Not Found"}` | `application/json` |
| Path matches, method does not | 405 | `{"detail":"Method Not Allowed"}`, plus an `allow` header | `application/json` |
| Unhandled exception | 500 | `Internal Server Error` | `text/plain; charset=utf-8` |

An `HTTPException` with `headers` sends those headers. The JSON uses compact separators: `{"detail":"Session not found"}`.

A validation error item has these keys, in this order:

- `type` (for example `missing`, `extra_forbidden`, `literal_error`, `greater_than_equal`, `value_error`, `int_parsing`, `json_invalid`);
- `loc` (a list such as `["body","rating"]` or `["query","limit"]`);
- `msg`;
- `input`;
- `ctx` (only for some types).

Example:

```json
{"detail":[{"type":"greater_than_equal","loc":["query","limit"],"msg":"Input should be greater than or equal to 1","input":"0","ctx":{"ge":1}}]}
```

A model validator error gives `"type":"value_error"`, `"msg":"Value error, <message>"` and `"ctx":{"error":{}}`.

The `msg` strings are the pydantic 2 messages. The Rust port must produce the same `type`, `loc` and `msg` for the validations each family file lists. UNVERIFIED: the exact `msg` text for every pydantic error type was not listed; port tasks must check each case against the Python server.

## Routing rules

- An API route with a trailing `/` in its path (`/api/sessions/`) answers a request without the slash with 307. The `location` header is an absolute URL: `http://<host>/api/sessions/`. The redirect keeps the query string.
- The reverse also holds: a request with an extra trailing slash on a route without one gets a 307 to the path without the slash. Example: `GET /home/` gives 307 to `http://<host>/home`.
- The redirect applies to every method, `POST` included. Its body is empty (`content-length: 0`).
- The `location` host and port come from the request `Host` header.
- The auth middleware runs before routing. A request without a credential gets 401, not 307.
- A path that matches no route in either form gets 404 `{"detail":"Not Found"}`.
- An API `GET` route does not answer `HEAD`. A `HEAD` request gets 405 with `allow: GET`.
- The FastAPI documentation routes, the static mounts and `/absolute_filepath/{path:path}` answer both `GET` and `HEAD`.
- Routes match in registration order. `routes.tsv` lists them in that order, and the order matters. For example, `GET /api/sessions/chat-threads` comes before `GET /api/sessions/{session_id}`.
- A path parameter without a converter matches one segment. `{path:path}` matches the rest of the path, slashes included.
