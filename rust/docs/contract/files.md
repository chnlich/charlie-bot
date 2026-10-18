# File routes

This file covers two routes that read files from the host file system:

- `/absolute_filepath/{path:path}`: the file server, which serves any file or directory the server process can read;
- `/perfetto/merged`: the merged Perfetto trace.

Both routes need the access key. Neither route limits the paths it reads.

## File server

- Methods: `GET` and `HEAD`.
- Path: `/absolute_filepath/{path:path}`. `path` is the absolute file system path without its first `/`.
- Auth: access key.
- Handler: `src/features/files/api.py:116`.

The mount prefix list is `file_urls.FILE_SERVER_MOUNTS` (`src/runtime/file_urls.py:11`). At the baseline it holds only `/absolute_filepath`. The `/files` and `/file` prefixes give the router 404 `{"detail":"Not Found"}`.

`GET /absolute_filepath` (no trailing slash) gives 307 to `/absolute_filepath/`.

### Order of handling

1. File views. Each registered file view gets the request first. The artifacts package registers the only one (see [Artifact pages](#artifact-pages)). A view that returns a response ends the request.
2. Resolve `/` + `path` with `pathlib.Path.resolve()`. This follows symbolic links and removes `..`.
3. Missing path: 404 `{"detail":"Not found"}` (lowercase `found`).
4. Directory: the listing page.
5. File: a `FileResponse`.

`HEAD` gives the same status and headers as `GET`. Uvicorn sends no body for `HEAD`. The chat page uses `HEAD` to check whether a linked path still exists.

### Directory listing

200, `text/html; charset=utf-8`, built from `_DIR_LISTING_TEMPLATE` (`src/features/files/api.py:24`):

- `<title>` and `<h2>`: `Index of <resolved path>` (HTML-escaped).
- A table with the header row `<tr><th></th><th>Name</th><th>Size</th><th>Modified</th></tr>`.
- First row, except at `/absolute_filepath/`: a `..` link to the request URL without its last segment, or to `/absolute_filepath` for a top-level directory.
- One row per child that `stat` can read. Directories come first, then files. Each group is sorted by lowercase name.

A child row:

```html
<tr><td>📄</td><td><a href="/absolute_filepath/tmp/example/a%20b.txt">a b.txt</a></td><td style="text-align:right">1 B</td><td>2026-01-01 12:00</td></tr>
```

- Icon: `📁` for a directory, `📄` for a file.
- `href`: the request URL path (trailing `/` removed) + `/` + the name, percent-encoded with no safe characters. The `href` is HTML-escaped.
- Name: the child name, with a trailing `/` for a directory. The name is HTML-escaped.
- Size: `human_size.format_size(st_size)` for a file, empty for a directory.
- Modified: the UTC mtime as `%Y-%m-%d %H:%M`.

A directory the process cannot list gives 403 `{"detail":"Permission denied"}`.

### File response

200, a Starlette `FileResponse`:

| Header | Value |
|---|---|
| `content-type` | `mimetypes.guess_type` of the resolved path, else `text/plain`. A `text/*` type gets `; charset=utf-8`. Example: a `.bin` file gives `application/octet-stream`. |
| `content-length` | the file size |
| `last-modified` | the mtime as an HTTP date |
| `etag` | as for static files, see [static.md](static.md#response-headers) |
| `accept-ranges` | `bytes` |

- The route sets no `Cache-Control`.
- `Range` requests follow the rules in [static.md](static.md#response-headers).
- The gzip middleware compresses the response by the usual rules. A compressed response keeps `etag` and `accept-ranges`.

Failures (probed):

| Case | Status | Body |
|---|---|---|
| A regular file the process cannot read | 500 | `Internal Server Error` (text/plain). The route's `PermissionError` branch never runs, because `FileResponse` opens the file only when it sends. |
| A path that exists and is neither a regular file nor a directory, for example a FIFO | 500 | `Internal Server Error`. `FileResponse` raises `RuntimeError`. |

### Artifact pages

The artifacts package registers `serve_artifact_path` (`src/features/artifacts/artifact_view.py:121`) as a file view.

An artifact page is a resolved path that meets all of these rules:

- it ends in `.html` (any case);
- it is inside `<sessions_dir>/<session id>/`;
- its parent directory is named `artifacts`.

The view does not check that the session exists.

Without a `diff` query parameter:

- A path that does not end in `.html` passes to the file server at once.
- An artifact page that is a regular file gives 200 `text/html; charset=utf-8` with `Cache-Control: no-store`. The body is the file text with the UI tags inserted (see below).
- Any other path passes to the file server.
- An artifact page that cannot be read as UTF-8 gives 500.

The UI tags, inserted before the last `</body>`, or appended after a newline when the page has no `</body>`:

```html
<script>window.__cbcServerSessionId="<session id>";</script>
<script src=/static/js/comment_post.js?v=<static asset version>></script>
<script src=/static/features/artifacts/artifact-comments.js?v=<static asset version>></script>
```

A newline follows the last tag. The session id is JSON-encoded. The version token is not URL-encoded here.

With a `diff` query parameter (the plan compare page):

| Case | Status | `detail` |
|---|---|---|
| The path does not exist | 404 | `Not found` (the file server answers) |
| The path is not an artifact page, is a directory, or cannot be stat-ed | 400 | `diff target is not a session artifact page: <resolved path>` |
| `diff` does not resolve to an artifact page. It is relative to `<sessions_dir>/<session id>/`, for example `artifacts/plan_01_v1.html`. | 400 | `diff base is not a session artifact page: <diff>` |
| The base page cannot be read | 404 | `diff base not found: <resolved base path>` |

200: `text/html; charset=utf-8` with `Cache-Control: no-store`. The body is the target page with word-level change marks against the base page (`plan_diff.annotate`), plus the UI tags.

UNVERIFIED: the mark markup comes from `src/features/artifacts/plan_diff.py` and is not documented here.

## `GET /perfetto/merged`

- Auth: access key. The public path list does not include it.
- Handler: `src/features/trace/api.py:340`.
- The Perfetto viewer page (`GET /perfetto`, see [pages.md](pages.md)) builds links to this route.

| Name | In | Type | Required | Default |
|---|---|---|---|---|
| `trace` | query, repeatable | string (file path) | no | `[]` |
| `dir` | query | string (directory path) | no | null |
| `pattern` | query | string (`fnmatch` pattern) | no | `*.json` |
| `slim` | query | bool | no | false |

Steps:

1. No `trace` and no `dir`: 400 `Provide at least one trace path with 'trace' or 'dir'.`.
2. `dir`: add the regular files in that directory whose names match `pattern`, sorted by name. The scan is not recursive. A `dir` that is not a directory logs `perfetto_dir_not_found` and adds nothing. No match: 400 `Provide at least one trace path; 'dir' matched no files.`.
3. For each path, `trace` values first:
   - not a regular file: 404 `Trace path not found: <path>`;
   - the first non-whitespace byte in the first 64 bytes is not `{` or `[`: 400 `Trace is not JSON: <path>`.
4. Build or reuse the cached gzip file (see below). A build error gives 500 with `detail` = the error text and logs `perfetto_merge_failed`. Example: `Not a Chrome-JSON trace (no traceEvents array): <path>`.

A relative path is relative to the server working directory.

### Build kinds

| Inputs | Kind | Output |
|---|---|---|
| One trace, `slim` false | direct pass | The original file bytes, gzip-compressed. A child process first checks that the file parses as a Chrome JSON trace. |
| One trace, `slim` true | single-trace merge | A merged trace, built in a child process. |
| Several traces | multi-trace merge | A merged trace. A process pool (spawn, at most 4 workers) parses the members. |

A merged trace is compact JSON `{"traceEvents":[…]}`. Each event's `pid` becomes the source file stem. Each thread gets a `thread_name` metadata event named `<stem>/<tid>`. Thread ids of later files are offset by multiples of 2^24 so they stay distinct (the second file's thread 1 becomes 16777217). The merge drops metadata events (`ph` `M`) whose name starts with `process_`. With `slim`, it also drops events of category `cpu_instant_event`. It also replaces an object `args` with `{"stream": …}` when `args` has `stream`, and removes `args` otherwise.

UNVERIFIED: the full merge and slim rules live in `src/features/trace/trace_merge.py`. The statements above come from one probe.

### Cache

- Directory: `<home>/cache/perfetto_merge/`.
- File: `<key>.json.gz`. The key is the SHA-256 hex of compact JSON `{"paths":[[<resolved path>,<size>,<mtime ns>],…],"slim":<bool>,"mode":"gzip"|"merge"}`.
- Hit: the server updates the file mtime and serves it.
- Miss: the server builds into a temporary file (`*.tmp`) in the same directory and renames it into place. Then it deletes the oldest entries so that at most 24 remain.
- Concurrent requests for one key share one build. A client that disconnects does not stop the build.

### Response

200, a `FileResponse` of the cache file:

- `content-type: application/gzip`. The body is the gzip file; there is no `Content-Encoding` header. The transport gzip middleware skips this type.
- `etag` and `last-modified` come from the cache file. A cache hit updates the mtime, so the `etag` changes on every request.
