# Framework routes

FastAPI adds four documentation routes to every app (`FastAPI.setup` in `fastapi/applications.py`). The app keeps the defaults: `openapi_url`, `docs_url`, `swagger_ui_oauth2_redirect_url` and `redoc_url` are not changed.

Shared rules:

- Methods: `GET` and `HEAD`. `HEAD` gives the same status and headers with no body.
- Auth: access key. The routes are not on the public path list, so a request without the key gets the 401 of [auth.md](auth.md#rejection).
- Parameters: none.
- Side effects: none.
- The routes are not in the OpenAPI document.

The app metadata (`server.py:326`):

| Field | Value |
|---|---|
| `title` | `CharlieBot` |
| `description` | `Multi-agent Claude Code orchestration system` |
| `version` | `0.1.0` |

## `GET /openapi.json`

- Handler: `fastapi/applications.py:1104`.
- 200, `application/json` (Starlette `JSONResponse`, compact).
- Body: the OpenAPI 3.1.0 document. Top-level keys in order: `openapi`, `info`, `paths`, `components`.
- `info` is `{"title","description","version"}` from the table above.
- The document has 87 paths at the baseline. FastAPI builds it once from the route signatures and pydantic models, then keeps it in memory.
- The document leaves out static mounts, WebSocket routes and routes with `include_in_schema=False`.

## `GET /docs`

- Handler: `fastapi/applications.py:1119`.
- 200, `text/html; charset=utf-8`.
- Body: FastAPI's Swagger UI page (`get_swagger_ui_html`) with the title `CharlieBot - Swagger UI`, `openapi_url` `/openapi.json` and the OAuth2 redirect URL `/docs/oauth2-redirect`. The page loads the Swagger UI script and stylesheet from `cdn.jsdelivr.net`.

## `GET /docs/oauth2-redirect`

- Handler: `fastapi/applications.py:1137`.
- 200, `text/html; charset=utf-8`.
- Body: FastAPI's fixed OAuth2 redirect page (`get_swagger_ui_oauth2_redirect_html`, title `Swagger UI: OAuth2 Redirect`).

## `GET /redoc`

- Handler: `fastapi/applications.py:1147`.
- 200, `text/html; charset=utf-8`.
- Body: FastAPI's ReDoc page (`get_redoc_html`) with the title `CharlieBot - ReDoc` and `openapi_url` `/openapi.json`. The page loads ReDoc from `cdn.jsdelivr.net`.

The HTML bodies depend on the FastAPI version (0.135.2 at the baseline). No CharlieBot page or script links to these routes.

UNVERIFIED: the last statement comes from a search of `web/` and `src/` for `/docs`, `/redoc` and `/openapi.json`, which found no link. A tool outside the repository may still call them.
