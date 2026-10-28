# Static mounts

The server serves CSS and JavaScript files from static mounts. The served static trees hold only `.css` and `.js` files at the baseline.

## Mounts

The mounts register after every route (`server.py:569` and `server.py:574`). The package mounts come first and in registration order. The `/static` mount is last, so a package mount answers its own prefix first.

| Mount path | Directory (repository-relative) | Directory at baseline |
|---|---|---|
| `/static/features/artifacts` | `src/features/artifacts/static` | present |
| `/static/features/backup` | `src/features/backup/static` | absent |
| `/static/features/backend_pool` | `src/features/backend_pool/static` | absent |
| `/static/features/chat_threads` | `src/features/chat_threads/static` | present |
| `/static/features/code_server` | `src/features/code_server/static` | present |
| `/static/features/cron` | `src/features/cron/static` | present |
| `/static/features/diag` | `src/features/diag/static` | present |
| `/static/features/slack` | `src/features/slack/static` | absent |
| `/static/features/discord` | `src/features/discord/static` | absent |
| `/static/features/disk_sweep` | `src/features/disk_sweep/static` | absent |
| `/static/features/web_reply` | `src/features/web_reply/static` | absent |
| `/static/features/host_auth` | `src/features/host_auth/static` | absent |
| `/static/features/diff_view` | `src/features/diff_view/static` | present |
| `/static/features/explain` | `src/features/explain/static` | present |
| `/static/features/improve` | `src/features/improve/static` | absent |
| `/static/features/memory` | `src/features/memory/static` | absent |
| `/static/features/router` | `src/features/router/static` | absent |
| `/static/features/terminal` | `src/features/terminal/static` | present |
| `/static/features/trace` | `src/features/trace/static` | absent |
| `/static/features/usage` | `src/features/usage/static` | present |
| `/static/features/voice` | `src/features/voice/static` | present |
| `/static/backends/openai_compatible` | `src/backends/openai_compatible/static` | absent |
| `/static/backends/antigravity` | `src/backends/antigravity/static` | absent |
| `/static/backends/charlie_code` | `src/backends/charlie_code/static` | absent |
| `/static/backends/claude_code` | `src/backends/claude_code/static` | present |
| `/static/backends/codex` | `src/backends/codex/static` | present |
| `/static/backends/gemini` | `src/backends/gemini/static` | absent |
| `/static/backends/anthropic_compatible` | `src/backends/anthropic_compatible/static` | absent |
| `/static/backends/opencode` | `src/backends/opencode/static` | absent |
| `/static/features/files` | `src/features/files/static` | absent |
| `/static` | `web/static` | present |

How the list is built:

- The package mounts come from `templating.page_package_static()` (`src/runtime/templating.py:74`). That is one mount per page package, in the order of the page-render registry.
- A package `src.<kind>.<name>` maps to the mount `/static/<kind>/<name>` and the directory `src/<kind>/<name>/static`.
- The package mounts use `check_dir=False`. The server mounts them even when the directory is absent.
- The `/static` mount exists only when `web/static` exists at import time (`server.py:573`). Its route name is `static`.

## Auth

Every path that starts with `/static/` is public. See [auth.md](auth.md#public-paths).

## Request handling

Each mount is a Starlette `StaticFiles` app with `html=False` and `follow_symlink=False`.

A mount matches `<mount path>/<rest>`. A request for the bare mount path of a package mount (for example `/static/features/cron`) falls through to the `/static` mount. That mount looks for `web/static/features/cron` and answers 404.

| Case | Status | Body |
|---|---|---|
| Method other than `GET` or `HEAD` | 405 | `{"detail":"Method Not Allowed"}`, no `allow` header |
| Regular file found | 200 | the file bytes |
| `If-None-Match` lists the file ETag, or `If-Modified-Since` is at or after the file time | 304 | empty |
| Directory, missing file, or a path that resolves outside the mount directory | 404 | `{"detail":"Not Found"}` |
| Path with a null byte, or a name that is too long | 404 | `{"detail":"Not Found"}` |
| Permission error on the file | 401 | `{"detail":"Unauthorized"}` |
| Package mount whose directory is absent | 500 | `Internal Server Error` (text/plain) |

Path rules:

- The server applies `os.path.normpath` to the rest of the path. Then it removes `.` and `..` segments.
- A symlink is resolved. The resolved path must stay inside the mount directory. Otherwise the result is 404.

### Absent package directory

The first request to a package mount runs a directory check. When the directory is absent, the check raises `RuntimeError`, and the client gets 500. The check runs again on each later request until it passes once. When a later update creates the directory, the mount starts to serve files with no restart.

The `http_request` log line records status 500 with `error=RuntimeError`.

## Response headers

A 200 response is a Starlette `FileResponse`. Its headers:

| Header | Value |
|---|---|
| `content-type` | Python `mimetypes.guess_type` of the file name. `text/plain` when the guess is empty. A `text/*` type gets `; charset=utf-8`. |
| `content-length` | the file size. The gzip middleware replaces or removes it (see below). |
| `last-modified` | the file mtime as an HTTP date (`email.utils.formatdate(mtime, usegmt=True)`) |
| `etag` | `"<md5 hex of '<st_mtime>-<st_size>'>"`. `<st_mtime>` is the Python `str()` of the float mtime. |
| `accept-ranges` | `bytes` |

Types at the baseline:

| Extension | `content-type` |
|---|---|
| `.js` | `text/javascript; charset=utf-8` |
| `.css` | `text/css; charset=utf-8` |

The server sets no `Cache-Control` header on static files. The pages add a version token to static URLs (`?v=<static_asset_version>`, see [pages.md](pages.md#static-asset-version)), so a changed file gets a new URL.

`FileResponse` handles a `Range` header (probed on the file server, which uses the same class):

| Request | Status | Body and headers |
|---|---|---|
| `Range: bytes=0-3` | 206 | 4 bytes, `content-range: bytes 0-3/<size>`, `content-length: 4` |
| Several ranges | 206 | a `multipart/byteranges` body |
| A range past the end | 416 | empty, `content-range: bytes */<size>`, `content-type: text/plain; charset=utf-8` |
| Not a `bytes=` range | 400 | `Only support bytes range` (text/plain) |
| Another malformed range | 400 | `Malformed range header.`, `Range header: range must be requested` or `Range header: start must be less than end` (text/plain) |

An `If-Range` value that matches neither the etag nor the last-modified date makes the server ignore `Range` and send the whole file.

## Gzip

The gzip middleware handles static responses like any other response. See [middleware.md](middleware.md#gzip).

- A file under 64 KiB is one body message. When it is 1000 bytes or larger and the client accepts gzip, the response is compressed with a new `content-length`.
- A file of 64 KiB or more is a streaming body (64 KiB chunks). When the client accepts gzip, the response is compressed and has no `content-length`.
- `HEAD` responses have an empty body and are never compressed.
