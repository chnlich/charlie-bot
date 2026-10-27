# Python server contract

These files state the HTTP, WebSocket and page contract of the Python CharlieBot server at the baseline commit below. The Rust server under `rust/crates/` now serves this contract; these files stay frozen as the baseline the parity harness compares against. The parity harness uses `routes.tsv` to track route coverage.

The contract does not cover persisted file formats, the config schema, background services, environment variables or the CLI command map. A separate contract under `state/` covers them.

## Baseline

- Commit: `4521a51b2ced8ed47f375f70e46bc4c11a9007c0` on `rust-backend`.
- Stack: FastAPI 0.135.2, Starlette 1.0.0, uvicorn 0.42.0, pydantic 2.12.5, Jinja2 3.1.6.

A statement that holds at a later commit is valid only after someone checks it again.

## Files

| File | Content |
|---|---|
| `routes.tsv` | The route table: one row per route of the assembled app, static mounts included. |
| [shapes.md](shapes.md) | Shared JSON shapes: encoders, time forms, error bodies, session JSON, run rows, chat messages. |
| [middleware.md](middleware.md) | Middleware order, the `http_request` log event, transport gzip and framework error responses. |
| [auth.md](auth.md) | The access key, public paths, cookie, run tokens, `require_caller`, the login page and rejections. |
| [websocket.md](websocket.md) | The session, terminal and voice WebSocket endpoints. |
| [pages.md](pages.md) | The HTML pages: templates, context values and the static asset version. |
| [static.md](static.md) | Static mounts, file response headers, `Range` handling and gzip. |
| [sessions.md](sessions.md) | `/api/sessions/` routes. |
| [chat.md](chat.md) | `/api/chat/` routes: upload, message and cancel. |
| [internal.md](internal.md) | `/api/internal/` version, delegate, schedule-trigger, trigger cancel and session-message. |
| [plans.md](plans.md) | The plan registry verbs and the plan list. |
| [replies.md](replies.md) | Slack, Discord and web reply routes. |
| [cron.md](cron.md) | Scheduled-task routes. |
| [improve.md](improve.md) | Improve loop start and stop. |
| [git.md](git.md) | The diff viewer's git routes. |
| [explain.md](explain.md) | The divider explain routes. |
| [voice.md](voice.md) | The voice probe and upload routes. |
| [tools.md](tools.md) | code-server, switch telemetry, host login panel and quota panel routes. |
| [proxy.md](proxy.md) | The Anthropic-to-OpenAI proxy route. |
| [files.md](files.md) | The file server, artifact pages and the merged Perfetto trace. |
| [framework.md](framework.md) | FastAPI's documentation routes. |

## Route table

`routes.tsv` is tab-separated with a header row. Columns:

| Column | Value |
|---|---|
| `method` | `GET`, `POST`, `PUT`, `PATCH`, `DELETE`, `GET,HEAD` or `WS`. A static mount row has `GET,HEAD`. |
| `path` | The route path as Starlette holds it, with `{name}` parameters. A mount row has the mount prefix. |
| `family` | The family file that documents the row: `<family>.md`. |
| `auth` | One value from the table in [auth.md](auth.md#auth-values-in-routestsv). |
| `handler` | `path:line` of the endpoint function. See below. |
| `stage` | The port stage that owns the row. See below. |

Handler values:

- A decorated endpoint: the repository path and the line of its first decorator.
- A static mount: the `app.mount` line in `server.py`.
- A FastAPI documentation route: the line in the installed `fastapi/applications.py`.

### Stage rules

Apply the first rule that matches a route:

1. Middleware, auth, static mounts, framework routes, `/home` and `GET /api/internal/version`: `server-skeleton`.
2. Triggers, cron tasks and improve loops: `triggers-cron-improve`. This rule comes before rule 4.
3. Routes that exist for a non-Claude backend: `other-backends`.
4. Routes that start or stop an agent run: `agent-orchestration`.
5. Read-only session routes and the pages that only read sessions: `session-read-api`.
6. Session writes and the session WebSocket: `session-write-streaming`.
7. All other feature routes: `features`.

`rust-cli-cutover` owns no route. That stage moves the CLI from the Python server to the Rust server and adds no endpoint.

## Method

The tables come from four sources:

1. A route dump. A script set `CHARLIEBOT_HOME` to a new directory under `/tmp` with a synthetic `config.yaml`, imported `server`, and walked `server.app.routes`. It did not start the server. The script recorded the type, path, methods and endpoint location of each route.
2. An OpenAPI dump (`server.app.openapi()`) for parameter names, types, defaults and request models.
3. In-process probes with Starlette's `TestClient` on synthetic homes. One home had no access key. One had a synthetic access key. The probes ran no lifespan, so no background service and no agent process started.
4. Source reading for side effects, error texts, ordering and code paths the probes did not reach.

The route dump, in outline:

```python
import inspect
from starlette import routing
import server

for route in server.app.routes:
  if isinstance(route, routing.Mount):
    print("GET,HEAD", route.path, "mount", route.app.directory)
    continue
  endpoint = route.endpoint
  where = f"{inspect.getsourcefile(endpoint)}:{inspect.getsourcelines(endpoint)[1]}"
  methods = ",".join(sorted(getattr(route, "methods", None) or ["WS"]))
  print(methods, route.path, where)
```

To check `routes.tsv` against a new dump, compare the `(method, path)` pairs. A static mount row compares as `GET,HEAD`. A WebSocket route compares as `WS`.

## Conventions

- `<home>` is the CharlieBot home directory (`CHARLIEBOT_HOME`). `<sessions_dir>` is `<home>/sessions`.
- Examples use synthetic names, paths and ids, for example `00000000-0000-4000-8000-000000000001` and `agent-host`.
- `UNVERIFIED` marks a statement that neither a probe nor a full source trace confirmed. The text after the marker gives the reason.
- A handler reference names the decorator line, as the route table does.
- JSON examples show key order. The port must keep it, see [shapes.md](shapes.md#json-encoding).
