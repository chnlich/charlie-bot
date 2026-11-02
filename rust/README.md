# CharlieBot server in Rust

`charliebot-server` serves the chat page `GET /`, `GET /api/auth/status`, `GET /api/internal/version`
and the `/static` mounts. It runs the middleware stack: auth gate, transport gzip and request
log. It holds the home writer fence, so no second server process serves the same home.

## Build

Build the server through `tools/remote-run/cargo-slot.sh` from `rust/`. The build host keeps the output, so the agent host
builds nothing locally.

CI (`.github/workflows/ci.yml`) runs two Rust jobs on `ubuntu-24.04`, whose glibc 2.39 matches the live host. `release-build`
runs `cargo build --release --locked -p charliebot-server` on every pull request and push. On a push to `main` it uploads
the artifact `charliebot-server-<full commit sha>`, which holds `charliebot-server` and `sha256sums.txt` and is kept for 30
days. It builds no tests, so the binary is a plain release build. `rust-test` runs `cargo test --locked` with Rust 1.99.0.

## Run

```sh
CHARLIEBOT_REPO_ROOT=/path/to/charlie-bot \
CHARLIEBOT_HOME=/tmp/charliebot-demo/home \
  rust/target/release/charliebot-server
```

Run the same configuration checks without starting the server or taking the home writer fence:

```sh
CHARLIEBOT_REPO_ROOT=/path/to/charlie-bot \
CHARLIEBOT_HOME=/tmp/charliebot-demo/home \
  rust/target/release/charliebot-server --check-config
```

The command prints `config ok` and exits 0 when the configuration passes. It exits 1 with the startup error on the first
failure. Any other argument prints the usage line and exits 2.

- `CHARLIEBOT_REPO_ROOT` (required) names the charlie-bot checkout whose `web/`, page packages
  (`web/<kind>/<name>/static` and `templates`) and `configs/` the server serves. The binary exits 1 when the variable
  is unset or the directory holds no `web/templates`.
- `CHARLIEBOT_HOME` selects the profile home: unset or blank is `~/.charliebot`, otherwise an absolute or `~` path.
- `config.yaml` in the home sets `server.host` and `server.port`. `credentials.yaml` sets
  `charliebot.access_key`; an empty or absent key turns access-key auth off.

Startup loads `config.yaml` and checks it, then requires a backend option and loads `credentials.yaml`.
Any error exits 1 with a message naming the key. Startup then takes the home writer fence. When
another process holds the fence, the binary logs `server_startup_refused_home_writer_fence` and exits 3.
Next it initializes the home, logs `server_ready`, and binds. SIGTERM and SIGINT stop accepting and give
in-flight requests up to 5 s. Rust stops services, closes the HTTP client and WebSockets, releases the fence,
and logs `charliebot_shutdown`. SIGTERM then restores and raises its default signal action;
it skips let-go steps.

SIGINT runs the `task_execution` and `headless_renderer` let-go steps after the shutdown line, then exits 0.
A failed bind runs those steps after its shutdown line and exits 1.

Logs are text lines on stdout that open with the UTC time, the level, the file and line of the tracing call, and the
event name, then `key=value` fields in call order (the `logline` grammar;
`charliebot_core::logline` writes and reads it). Every HTTP
response logs one `http_request` line with `method`, `path`, `status`, `duration_ms` and `client`. A failed request
adds `error`.

## Test

- Run every compile and test through `tools/remote-run/`: `run.sh` for any command, `cargo-slot.sh` for Cargo.
- The Rust suite is `cargo test --workspace --locked` from `rust/`.
- The frontend JavaScript tests are `node --test` from the checkout root, as CI does.
- The agent environment on the agent host refuses the toolchains that `tools/remote-run/README.md` lists.
- When the runner exits 86 or 87, end the run and report the blocker with the link it prints.

The Rust tests compare the YAML writer, JSON reader, boot marker, body and form handling, and html tokenizer
with recorded outputs. The compatibility and SSH outputs under `rust/fixtures/golden/` are recorded data; other
recorded expectations live beside the tests that read them. The recorded tables name their generator in the table
headers, and the WAV fixtures name theirs in the fixture README.

### Shell-script tests

`charliebot-shell-harness` (`crates/infra/shell-harness`) runs a repository shell script for real, against stub
commands. The sandbox puts the stub directory first on `PATH`; a stub never starts an interpreter. A test:

```rust
use charliebot_shell_harness::{self as harness, Sandbox};

let sandbox = Sandbox::new();                        // a temporary root; bin/ first on PATH
let home = sandbox.dir("home");                      // a temporary home, with its parents
sandbox.stub("tmux", "exit 0\n");                    // a stub: the body of a bash script, in bin/
let mut env = sandbox.env();                         // the test's variables, PATH with the stubs first
env.insert("CHARLIEBOT_HOME".to_string(), home.display().to_string());
let script = harness::repo_root().join("scripts/switch-server.sh");
let mut command = harness::command("bash", &env);    // a command that sees exactly env
command.arg(&script).arg("rust").arg(sha);
let run = harness::run(&mut command);                // stdout, stderr and exit code; killed after 120 s
assert_eq!(run.code, 0, "{}", run.describe());
```

`harness::spawn` starts a background process, such as a stand-in server. Stubs are shell scripts or Rust programs; a
stub never starts an interpreter. `sandbox.install_systemd_stubs()` adds the `systemctl` and `systemd-run` stubs for the
launcher, and `sandbox.scope_records()` lists the scopes `systemd-run` started. The switch tests in
`crates/app/script-tests/tests/switch_server/` show the full pattern, including the stand-in servers that the release
roots copy.

## Development rules

- **A name that two crates agree on has one owner.** A file or directory name, an environment variable name, an event
  name or an HTTP path has one owner module, and the other crates reference that module's const or function.
  Environment variable names live in `infra/core` (`home.rs`, `repo_root.rs`, `run_token.rs`). The structure
  test `no_name_is_spelled_outside_its_owner` in `crates/app/structure/tests/structure.rs` protects each kind of name.
  A session layout name has one row each in `OWNER_TABLE`, and the crate that owns a new shared file or directory name
  adds its row in the same commit. An event name needs no table edit: the rule reads `infra/core/src/event_types.rs`
  and takes one row for each whitespace-free value that holds `_`, except the names in `EXTERNAL_EVENT_NAMES`, whose
  spellings also appear in backend output the rule cannot tell apart. A `CHARLIEBOT_` environment variable name or an
  `/api/` route path has no table; the rule fails the name when two crates spell the same one. A route path lives where
  its route registers: that crate exports the path as a `pub const`, and `charliebot_cli_common::route::fill` fills its
  `{}` parameters.
  *Purpose:* a rename in one crate otherwise leaves the other crate compiling, and the break shows only at run time.
- **The process environment changes only at startup.** `runtime/agents/src/agent_environment.rs` is the one production
  place that sets or removes a process environment variable, and it does so before any thread starts. The structure
  test `no_global_state_outside_its_owner` in `crates/app/structure/tests/structure.rs` checks the rule: a production
  call to `set_var`, `remove_var`, `setenv`, `unsetenv`, `putenv` or `clearenv`, and a production `use` of one, fail
  outside that file.
  *Purpose:* a write to the environment races with every thread that reads it, so one owner that runs first keeps that
  race out of the server.
- **A production `static` stays private to its module.** A production `static` has no visibility modifier; its module
  exposes mutable state through functions, and shared data that never changes is a `const` or an access function. The
  structure test `no_global_state_outside_its_owner` in `crates/app/structure/tests/structure.rs` checks the rule, a
  `static` declared inside `thread_local!` included.
  *Purpose:* a public static lets any module change shared state without the owning module's checks, so no reader can
  tell which code changed it, and the access function gives those checks one place to live.
- **The PACKAGES list alone decides registration order.** The `PACKAGES` list in `rust/crates/app/packages/src/lib.rs`
  sets the order in which packages register; `RUNTIME` in the same file registers before it.
  *Purpose:* routes, page parts, metadata keys, template paths and lifespan steps follow registration order
  (`docs/registry.md` in the repository root), so a second list that set that order would change the server's output
  with no diff to `PACKAGES`.
- **Each file kind has one owner.** The owner module holds the file's paths, its record type, its line rules and the
  functions that decode and encode its lines, and other modules read and write the file through those functions, also
  when they only pass the path to a helper: the owner's decoder and encoder are the only ones code outside the owner
  reaches. The format table in the structure test (`FORMAT_TABLE` in `crates/app/structure/tests/structure.rs`) names
  each kind's owner and its functions, and fails a function that names one of them and also decodes or encodes a line
  in its body. A new file kind adds its row there.
  *Purpose:* a change to a file's format, location or bad-line handling then happens in one module, and a function
  that opens a kind's path and decodes its lines in one body fails `cargo test` instead of reading the file wrong at
  run time.

## Layout

```
rust/
  Cargo.toml                      workspace: every crate under `crates/`
  fixtures/golden/                the expected outputs the compat and ssh tests compare with
  crates/infra/core/src/
    config.rs                     config.yaml: typed sections, legacy and unknown key refusals
    credentials.rs                credentials.yaml
    hot_reload.rs                 reload a file when its (mtime, size) moves
    home.rs, paths.rs             CHARLIEBOT_HOME, expanduser, realpath
    fence.rs, fsutil.rs           the home writer fence and its identity record
    run_token.rs                  signed run tokens
    logging.rs, timefmt.rs        JSON log lines, the stored time formats
    text_repr.rs                  repr, str and JSON renderings
    printable_table.rs            the non-printable character ranges
    ssh.rs                        the batch-mode ssh argv and its 0700 control directory
  crates/infra/compat/src/   the compatibility pieces the server and later crates share
    json.rs                       orjson and Starlette JSON bytes, floats included
    validation.rs                 FastAPI's 422 bodies and pydantic's lax parsing of query and path values
    jinja.rs                      minijinja environment with Jinja2's tojson and urlencode, autoescape on
    mimetypes.rs, mimetypes_table.rs  content types
    static_files.rs               StaticFiles answers: ranges, conditional requests, content types
    responses.rs, headers.rs      response builders and the latin-1 header text
    routing.rs                    method refusals: GET-only, GET and HEAD, POST-only, and Starlette's `allow` list
    http_client.rs                outbound client: HTTPS_PROXY, NO_PROXY and SSL_CERT_FILE over rustls
  crates/infra/shell-harness/src/lib.rs
                                  the shell-script test harness: the stub sandbox, the command runner and spawn
  crates/runtime/registry/src/
    registry.rs                   the Registry: one ledger per hook kind, duplicate refusals, package order
    wiring.rs, pages.rs, turn.rs, sidebar.rs, usage.rs, sequence.rs
                                  the typed registration methods of each hook kind
    backend.rs                    backend types, traits, lifecycles, child env, reading limits, usage resolvers
    lifespan.rs                   the startup and shutdown steps around the crash-recovery task
    context.rs                    FeatureContext and the typed startup state
    store_hooks.rs                the store's hook traits, answered by the registered lifecycles and controllers
    templating.rs                 the page engine: Jinja2-compatible rendering with minijinja and `render_page`
  crates/app/packages/src/lib.rs
                                  the composition root both binaries share: the runtime and package lists, one entry
                                  per package
  crates/app/server/src/
    main.rs                       startup, lifespan, bind, graceful shutdown
    app.rs                        the middleware stack and the feature routers
    packages/                     the server's registry, re-exported from `charliebot-packages`, and its tests
    routes/mod.rs                 the one list of route families, 405 and 404/307 answers
    routes/{pages,system,static_mounts,websockets}.rs
    auth.rs, cookies.rs           the access-key gate
    gzip.rs                       transport gzip
    request_log.rs, request_path.rs
    templating.rs                 Jinja2-compatible page rendering with minijinja
    home_init.rs, buildinfo.rs, state.rs
  crates/app/script-tests/
                                  the stand-in servers (src/bin) and the switch-server tests (tests/switch_server/)
  crates/app/switch-helper/
                                  the switch script's home, run-record and API-answer reads (`charliebot switch-helper`)
```

## Behavior notes

- Transport gzip uses zlib at level 1; the compressed bytes and their `content-length` are the zlib output.
- Logs use the `logline` grammar, with UTC time and a source-location column. The server adds
  `graceful_shutdown_timeout_exceeded`, `server_failed` and the startup failure events in
  `crates/app/server/src/main.rs`.
- YAML loads as YAML 1.2 (serde_yaml): unquoted `yes`, `no`, `on` and `off` are strings.
- Startup also loads `credentials.yaml` and the `voice:` keys the page reads, and it fails on an
  invalid file.
- Package-registered config sections stay raw YAML until their packages register their sections. The page reads only
  `voice.default_backend`.
- A run token whose base64 holds characters outside the base64url alphabet is refused.
