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

- `CHARLIEBOT_REPO_ROOT` (required) names the charlie-bot checkout whose `web/`, page packages
  (`src/<kind>/<name>/static` and `templates`) and `configs/` the server serves. The Python server
  derives the same root from its own location. The binary exits 1 when the variable is unset or the
  directory holds no `web/templates`.
- `CHARLIEBOT_HOME` selects the profile home exactly as `src/infra/home.py` does: unset or blank is
  `~/.charliebot`, otherwise an absolute or `~` path.
- `config.yaml` in the home sets `server.host` and `server.port`. `credentials.yaml` sets
  `charliebot.access_key`; an empty or absent key turns access-key auth off, as in Python.

Startup loads `config.yaml` and checks it, then requires a backend option and loads `credentials.yaml`.
Any error exits 1 with a message naming the key. Startup then takes the home writer fence. When
another process holds the fence, the binary logs `server_startup_refused_home_writer_fence` and exits 3.
Next it initializes the home, logs `server_ready`, and binds. SIGTERM or SIGINT stops accepting, gives
in-flight requests up to 5 s, releases the fence and logs `charliebot_shutdown`. The process then ends
killed by the same signal, as uvicorn's does.

Logs are JSON lines on stdout. Each line is `{"event": ..., <fields>, "level": ..., "timestamp": ...}`
and uses the Python server's event names and fields. Every HTTP response logs one `http_request` line
with `method`, `path`, `status`, `duration_ms` and `client`. A failed request adds `error`.

## Test

```sh
cd rust
cargo build --release && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check
cd ..
uv run python rust/scripts/compare_live.py   # Python and Rust side by side on copies of one temp home
uv run python rust/scripts/fence_interop.py  # the fence excludes in both directions
```

`compare_live.py` starts both servers on a fresh home under `/tmp`, with ports in 18600-18699. It
replaces the Python server's startup cgroup sweep with a no-op, because the sweep removes empty
session cgroups host-wide. It compares status, headers and decoded bodies for each check, then the
`http_request` log events. Both scripts exit 1 on any difference.

## Layout

```
rust/
  Cargo.toml                      workspace: charliebot-core, charliebot-server
  PORT_BASELINE                   the Python commit the port reproduces
  docs/PORT_STATUS.md             every Python route: method, path, stage, ported or todo
  scripts/compare_live.py         live response and log comparison against server.py
  scripts/fence_interop.py        cross-language fence exclusion check
  scripts/gen_mimetypes.py        regenerates mimetypes_table.rs from CPython's mimetypes
  crates/charliebot-core/src/
    config.rs                     config.yaml: typed sections, legacy and unknown key refusals
    credentials.rs                credentials.yaml
    hot_reload.rs                 reload a file when its (mtime, size) moves
    home.rs, paths.rs             CHARLIEBOT_HOME, expanduser, realpath
    fence.rs, fsutil.rs           the home writer fence and its identity record
    run_token.rs                  signed run tokens
    registry.rs                   package, backend type and config section registrations
    logging.rs, timefmt.rs        JSON log lines, Python time formats
    pyfmt.rs                      CPython repr, str and json.dumps renderings
  crates/charliebot-server/src/
    main.rs                       startup, bind, graceful shutdown
    app.rs                        the middleware stack
    routes/mod.rs                 the one list of route families, 405 and 404/307 answers
    routes/{pages,system,static_mounts,websockets}.rs
    auth.rs, cookies.rs           the access-key gate
    gzip.rs                       transport gzip
    request_log.rs, request_path.rs
    static_files.rs, mimetypes.rs StaticFiles and FileResponse
    templating.rs                 Jinja2-compatible page rendering with minijinja
    home_init.rs, buildinfo.rs, git.rs, responses.rs, state.rs
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
  `voice.engine` and `voice.default_backend`.
- A run token whose base64 holds characters outside the base64url alphabet fails here. CPython's
  decoder skips such characters.
- The diff page's `code_server_enabled` template global is not registered, because the diff page is
  not ported.
