# CharlieBot server in Rust

`charliebot-server` is the base for porting the Python server (`server.py`) to Rust. It serves the
foundation routes the way the Python server does, byte for byte where the frontend or the CLI can
observe it. Those routes are the chat page `GET /`, `GET /api/auth/status`, `GET /api/internal/version`
and the `/static` mounts. It runs the same middleware stack: auth gate, transport gzip and request
log. It holds the home writer fence, so it and the Python server exclude each other on one home.
`docs/PORT_STATUS.md` lists every Python route and whether it is ported. `PORT_BASELINE` names the
Python commit the port reproduces.

## Build

```sh
cd rust
cargo build --release        # target/release/charliebot-server
```

## Run

```sh
CHARLIEBOT_REPO_ROOT=/path/to/charlie-bot \
CHARLIEBOT_HOME=/tmp/charliebot-demo/home \
  rust/target/release/charliebot-server
```

Run the same configuration checks without starting the server or taking the home writer fence:

```sh
CHARLIEBOT_REPO_ROOT=/path/to/charlie-bot \
CHARLIEBOT_VENV_BIN=/path/to/charlie-bot/.venv/bin \
CHARLIEBOT_HOME=/tmp/charliebot-demo/home \
  rust/target/release/charliebot-server --check-config
```

The command prints `config ok` and exits 0 when the configuration passes. It exits 1 with the startup error on the first
failure. Any other argument prints the usage line and exits 2.

- `CHARLIEBOT_REPO_ROOT` (required) names the charlie-bot checkout whose `web/`, page packages
  (`src/<kind>/<name>/static` and `templates`) and `configs/` the server serves. The Python server
  derives the same root from its own location. The binary exits 1 when the variable is unset or the
  directory holds no `web/templates`.
- `CHARLIEBOT_VENV_BIN` (required) names the server virtualenv's `bin` directory. Startup reads it
  once before the home writer fence. The server exits 1 and names `CHARLIEBOT_VENV_BIN` when it is
  unset, empty, or not valid Unicode.
- `CHARLIEBOT_HOME` selects the profile home exactly as `src/infra/home.py` does: unset or blank is
  `~/.charliebot`, otherwise an absolute or `~` path.
- `config.yaml` in the home sets `server.host` and `server.port`. `credentials.yaml` sets
  `charliebot.access_key`; an empty or absent key turns access-key auth off, as in Python.

Startup loads `config.yaml` and checks it, then requires a backend option and loads `credentials.yaml`.
Any error exits 1 with a message naming the key. Startup then takes the home writer fence. When
another process holds the fence, the binary logs `server_startup_refused_home_writer_fence` and exits 3.
Next it initializes the home, logs `server_ready`, and binds. SIGTERM and SIGINT stop accepting and give
in-flight requests up to 5 s. Rust stops services, closes the HTTP client and WebSockets, releases the fence,
and logs `charliebot_shutdown` (`server.py:293-310`). SIGTERM then restores and raises its default signal action;
it skips let-go steps, as Uvicorn does (`uvicorn/server.py:322-339`).

SIGINT runs the `task_execution` and `headless_renderer` let-go steps after the shutdown line, then exits 0.
A failed bind runs those steps after its shutdown line and exits 1 (`uvicorn/server.py:179-182`). Python's normal exit
cancels pending tasks and joins the default executor (`asyncio/runners.py:63-75`; `concurrent/futures/thread.py:23-31`),
then its `atexit` hook closes the browser
(`src/features/artifacts/headless_render.py:184`). Turn cancellation follows `src/runtime/master_cc_run.py:648-664`.

Logs are JSON lines on stdout. Each line is `{"event": ..., <fields>, "level": ..., "timestamp": ...}`
and uses the Python server's event names and fields. Every HTTP response logs one `http_request` line
with `method`, `path`, `status`, `duration_ms` and `client`. A failed request adds `error`.

## Test

```sh
cd rust
/home/chaoli/scripts/20261018_rustport/cargo-slot.sh build --release && /home/chaoli/scripts/20261018_rustport/cargo-slot.sh test && /home/chaoli/scripts/20261018_rustport/cargo-slot.sh clippy --all-targets -- -D warnings && /home/chaoli/scripts/20261018_rustport/cargo-slot.sh fmt --check
cd ..
uv run python rust/parity/parity.py --rust-bin rust/target/release/charliebot-server --stage server-skeleton
uv run python rust/scripts/fence_interop.py  # the fence excludes in both directions
uv run python rust/scripts/gen_compat_fixtures.py  # rewrites rust/fixtures/python/ from the Python baseline
```

The compat and ssh tests compare with the committed `rust/fixtures/python/` files. After the Python code
they mirror changes, regenerate the fixtures and commit them with that change.

The skeleton parity check runs both servers through `rust/parity/` with synthetic homes, temporary
ports and a Landlock write fence. Run it with
`uv run python rust/parity/parity.py --rust-bin rust/target/release/charliebot-server --stage server-skeleton`.

## Layout

```
rust/
  Cargo.toml                      workspace: every crate under `crates/`
  PORT_BASELINE                   the Python commit the port reproduces
  docs/PORT_STATUS.md             every Python route: method, path, stage, and status (`ported`, `todo` or `not ported`)
  docs/registry.md                the plug-in seam: each hook kind, its Rust signature and Python source
  parity/parity.py                skeleton check: `uv run python rust/parity/parity.py --rust-bin rust/target/debug/charliebot-server --stage server-skeleton`
  scripts/fence_interop.py        cross-language fence exclusion check
  scripts/gen_mimetypes.py        regenerates mimetypes_table.rs from CPython's mimetypes
  scripts/gen_compat_fixtures.py  writes fixtures/python/ from the Python baseline
  fixtures/python/                the expected outputs the compat and ssh tests compare with
  crates/infra/core/src/
    config.rs                     config.yaml: typed sections, legacy and unknown key refusals
    credentials.rs                credentials.yaml
    hot_reload.rs                 reload a file when its (mtime, size) moves
    home.rs, paths.rs             CHARLIEBOT_HOME, expanduser, realpath
    fence.rs, fsutil.rs           the home writer fence and its identity record
    run_token.rs                  signed run tokens
    logging.rs, timefmt.rs        JSON log lines, Python time formats
    pyfmt.rs                      CPython repr, str and json.dumps renderings
    ssh.rs                        the batch-mode ssh argv and its 0700 control directory
  crates/infra/compat/src/   the Python-compatibility pieces the server and later crates share
    json.rs                       orjson and Starlette JSON bytes, floats included
    validation.rs                 FastAPI's 422 bodies and pydantic's lax parsing of query and path values
    jinja.rs                      minijinja environment with Jinja2's tojson and urlencode, autoescape on
    mimetypes.rs, mimetypes_table.rs  content types; the table is generated by scripts/gen_mimetypes.py
    static_files.rs               StaticFiles answers: ranges, conditional requests, content types
    responses.rs, headers.rs      response builders and the latin-1 header text
    routing.rs                    method refusals: GET-only, GET and HEAD, POST-only, and Starlette's `allow` list
    http_client.rs                outbound client: HTTPS_PROXY, NO_PROXY and SSL_CERT_FILE over rustls
  crates/runtime/registry/src/
    registry.rs                   the Registry: one ledger per hook kind, duplicate refusals, package order
    wiring.rs, pages.rs, turn.rs, sidebar.rs, usage.rs, sequence.rs
                                  the typed registration methods of each hook kind
    backend.rs                    backend types, traits, lifecycles, child env, reading limits, usage resolvers
    lifespan.rs                   the startup and shutdown steps around the crash-recovery task
    context.rs                    FeatureContext and the typed startup state
    store_hooks.rs                the store's hook traits, answered by the registered lifecycles and controllers
    templating.rs                 the page engine: Jinja2-compatible rendering with minijinja and `render_page`
  crates/app/server/src/
    main.rs                       startup, lifespan, bind, graceful shutdown
    app.rs                        the middleware stack and the feature routers
    packages/                     the composition root: the package list, with a stub for each unported package
    routes/mod.rs                 the one list of route families, 405 and 404/307 answers
    routes/{pages,system,static_mounts,websockets}.rs
    auth.rs, cookies.rs           the access-key gate
    gzip.rs                       transport gzip
    request_log.rs, request_path.rs
    templating.rs                 Jinja2-compatible page rendering with minijinja
    home_init.rs, buildinfo.rs, state.rs
```

## Differences from the Python server

The routes marked `ported` in `docs/PORT_STATUS.md` answer as the Python server does, except where
listed here:

- `GET /` renders the context of a home without sessions. The session sidebar, the redirect to the
  first session and the `?session=` bootstrap port with the sessions stage. Until then, a home whose
  `sessions/` directory holds anything answers 500 rather than render a sidebar that omits them.
- WebSocket upgrades on `/ws/sessions/{id}`, `/ws/terminal` and `/ws/voice/{id}` run the `token`
  check, but the streams are not ported, so every upgrade answers 403. An authorized one also logs
  `websocket_route_not_ported`.
- `GET /api/sessions/{session_id}/effective-prompt` leaves out the reply segments that the web, Slack and Discord
  turn contributions add to a manager run, because those contributions do not yet render their segments. The
  preview of a manager session with one of those origins differs from the Python server until they do.
- Transport gzip uses zlib at level 1, and the Python server uses ISA-L at level 1. The headers and the
  decoded bytes match; the compressed bytes and their `content-length` differ.
- Logs are JSON lines; the Python server renders structlog's console format. Event names and fields
  match. The Rust server adds `websocket_route_not_ported`, `graceful_shutdown_timeout_exceeded` and
  `server_failed`.
- YAML loads as YAML 1.2 (serde_yaml), and the Python server loads YAML 1.1 (PyYAML). Unquoted `yes`,
  `no`, `on` and `off` are strings here and booleans there.
- Startup also loads `credentials.yaml` and the `voice:` keys the page reads, and it fails on an
  invalid file. The Python server reads credentials at the first request. Startup skips the Python
  server's v1-sessions and package startup checks, session cgroups, task recovery and background
  services; those port with their stages.
- Package-registered config sections stay raw YAML until their packages port. The page reads only
  `voice.default_backend`.
- A run token whose base64 holds characters outside the base64url alphabet fails here. CPython's
  decoder skips such characters.
- The diff page's `code_server_enabled` template global is not registered, because the diff page is
  not ported.
