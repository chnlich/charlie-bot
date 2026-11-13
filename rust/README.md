# CharlieBot server in Rust

`charliebot-server` serves the chat page `GET /`, `GET /api/auth/status`, `GET /api/internal/version`
and the `/static` mounts. It runs the middleware stack: auth gate, transport gzip and request
log. It holds the home writer fence, so no second server process serves the same home.

## Build

Build the server through `dev/remote-run/cargo-slot.sh` from `rust/`. The build host keeps the output, so the agent host
builds nothing locally.

CI (`.github/workflows/ci.yml`) runs the Rust jobs on `ubuntu-24.04`, whose glibc 2.39 matches the live host; the exact
commands live only in [`.github/workflows/ci.yml`](../.github/workflows/ci.yml). What each job guards:

- `release-build` builds `charliebot-server` and `charliebot` in release mode without tests, so the binary stays a plain
  release build. A push to `main` uploads both binaries and their `sha256sums.txt` as the artifact
  `charliebot-server-<full commit sha>`, kept for 30 days; a manual run keeps its binaries for 1 day.
- `rust-test` runs the Rust suite through `cargo nextest` with Rust 1.99.0, then fails a unit test whose alone time went
  over the 0.1 s gate, and fails when any doc-test runs (doc-tests carry no per-test time, so one that runs dodges the
  gate).
- `rust-lint` runs clippy over all targets with warnings as errors, and the formatter check.

The other CI jobs — the PR-contract check and the frontend suite, which runs the no-Python guard — run in the same
workflow; the integration suites run from `.github/workflows/integration.yml`.

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

- Run every compile and test through `dev/remote-run/`: `run.sh` for any command, `cargo-slot.sh` for Cargo.
- The Rust suite is `cargo test --workspace --locked` from `rust/`; CI runs the same suite through
  `cargo nextest run --locked --profile ci`.
- The frontend JavaScript tests are `node --test` from `web/`, as CI does.
- The agent environment on the agent host refuses the toolchains that `dev/remote-run/README.md` lists.
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
  module allowed to set or remove a process environment variable, and any such change runs before any thread starts.
  The server builds its agent environment as a value rather than rewriting its own process environment. The structure
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

One line per crate; a crate with a README links to it, and the README carries the detail.

```
rust/
  Cargo.toml                      workspace: every crate under `crates/`
  fixtures/golden/                the expected outputs the compat and ssh tests compare with
  crates/
```

`crates/app/` — the two binaries and their composition:

- [`cli`](crates/app/cli/README.md) — the `charliebot` dispatcher, one file per subcommand under `src/commands/`
- `mock-home` — the synthetic session home: `generate` writes it, `check` reads a served copy back against its manifest
- [`packages`](crates/app/packages/README.md) — the composition root both binaries share: the runtime registrations and the package list, in registration order
- `script-tests` — the stand-in servers (`src/bin`) and the switch-server tests (`tests/switch_server/`)
- `server` — `charliebot-server`: startup, the middleware stack, the route families, graceful shutdown
- `structure` — the structure tests: name ownership, the environment rule, private statics, file-kind ownership
- `switch-helper` — the reads `scripts/switch-server.sh` performs on the home, the run records and the API answers (`charliebot switch-helper`)

`crates/backends/` — one crate per LLM CLI backend:

- [`anthropic_compatible`](crates/backends/anthropic_compatible/README.md) — Claude Code against a configured Anthropic-compatible endpoint
- [`antigravity`](crates/backends/antigravity/README.md) — the `agy --print` backend, read from its JSON envelope once the process exits
- [`charlie_code`](crates/backends/charlie_code/README.md) — the CharlieCode CLI against an OpenAI-compatible endpoint
- [`claude_code`](crates/backends/claude_code/README.md) — the cc-claude backend: option, lifecycle, environment, event translation, and the Claude Code usage source
- [`codex`](crates/backends/codex/README.md) — the codex backend type, its usage resolver and the Codex quota accounts
- [`devin`](crates/backends/devin/README.md) — the Devin CLI in print mode, run headless with every tool approved
- [`gemini`](crates/backends/gemini/README.md) — the gemini CLI in stream-json mode, and the legacy credential keys
- [`openai_compatible`](crates/backends/openai_compatible/README.md) — the `cc-openai-compatible` backend and the Anthropic Messages proxy its calls go through
- [`opencode`](crates/backends/opencode/README.md) — the opencode backend's shared server, session backend and usage source

`crates/features/` — one crate per product feature:

- `artifacts` — the session plan registry with its routes and verbs, the publish config, and the `artifact`/`publish` CLI
- `backup` — the scheduled home backup, pruned by a tiered retention policy
- `chat_threads` — the Slack/Discord/Feishu thread sessions and their turn contribution
- `code_server` — the code-server config section, the open route and the template global
- [`cron`](crates/features/cron/README.md) — scheduled tasks in `config.d/cron.d`: loading, scheduling, manual runs and sequence recovery
- `diag` — the write-only client timing telemetry route
- `diff_view` — the `/diff` page and the `/api/git` routes it calls
- `discord` — the Discord gateway service, internal routes and thread reply verbs
- `disk_sweep` — the task-worktree and scratch-directory cleanup
- `drafts` — the approval card of a message or publication in the operator's name, and the operator-only send route
- `explain` — the explain button's routes and their background generation
- `failure_triage` — the failure-triage scan: failed runs and error logs grouped by signature, judged in code and by one model one-shot
- `feishu` — the Feishu long-connection service, internal routes and thread verbs
- `files` — the file server at `/absolute_filepath/{path}`
- `github` — the `charliebot github` command group, its draft-target validation and the draft sender
- `host_auth` — the host login panel at `/host-auth` and its browser-login page
- [`improve`](crates/features/improve/README.md) — the improve loop: `charliebot improve start` runs an iterative worker loop on the task tree as one sequence
- `memory` — the labeled-entry memory store's server half and its turn contribution
- `run_log_compaction` — the cron handler that stores a finished run's log files as `.zst` once a day old
- `slack` — the Slack Socket Mode summon listener, thread reply routes and turn hooks
- `taste` — the user's choice precedents: the append-only store, the `taste` CLI and the turn contribution
- `terminal` — the `/ws/terminal` route and its per-profile tmux and PTY relay
- `trace` — the Perfetto trace viewer and the merged-trace cache
- `usage` — the quota panel, the usage ledger capture, the token usage page and the cold-storage sweep
- `voice` — voice input: the live preview relay, the online upload transcription and the turn contribution
- `web_reply` — the internal routes behind `charliebot web` and the page's web-reply surface
- [`web_search`](crates/features/web_search/README.md) — the MCP web-search endpoint at `POST /api/web-search/mcp`

`crates/infra/` — the pieces every other crate builds on:

- `cli-common` — the shared subcommand plumbing: argument parsing, session resolution, the internal-API client and the exit codes
- `compat` — the request and response pieces every crate shares: JSON bodies, validation errors, routing and static files
- [`core`](crates/infra/core/README.md) — the foundation: profile home, configuration, credentials, the home writer fence, log lines and run tokens
- [`html`](crates/infra/html/README.md) — the HTML tokenizer, character references and page-open fold primitives
- `improve-sandbox` — the sandbox that runs one improve round's command under a write-only Landlock boundary
- `shell-harness` — the harness that runs the repository's shell scripts for real, against stub commands

`crates/runtime/` — the machinery a session runs on:

- [`agents`](crates/runtime/agents/README.md) — agent process transport, backend launch planning and environment setup
- `registry` — the plug-in seam: the `Registry` every feature and backend crate fills through its `register`
- `session-api` — the session listing routes and the shared listing helpers
- [`store`](crates/runtime/store/README.md) — the single writer for session metadata and chat event logs
- [`tasks`](crates/runtime/tasks/README.md) — manager turns and task execution: delegation, worker runs, the worktree lifecycle and review
- [`triggers`](crates/runtime/triggers/README.md) — delayed and watched triggers that admit scheduled task inputs
- `usage` — the session's usage projection: backend resolver precedence, event-fold tiers and context state
- `user_text_checks` — the shared checks for user-facing page and reply text

## Behavior notes

- Transport gzip compresses a response body with gzip at level 1, and the compressed bytes and their `content-length`
  are the gzip output. A body a memo serves on every repeat request is compressed once at level 9 and served from the
  memo.
- Logs use the `logline` grammar, with UTC time and a source-location column. The server adds
  `graceful_shutdown_timeout_exceeded`, `server_failed` and the startup failure events in
  `crates/app/server/src/main.rs`.
- YAML loads as YAML 1.2 (serde_yaml): unquoted `yes`, `no`, `on` and `off` are strings.
- Startup also loads `credentials.yaml` and the `voice:` keys the page reads, and it fails on an
  invalid file.
- Package-registered config sections stay raw YAML until their packages register their sections. The page reads only
  `voice.default_backend`.
- A run token whose base64 holds characters outside the base64url alphabet is refused.
