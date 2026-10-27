# The `charliebot` CLI

Baseline: the Python tree at `4521a51b2`. Entry point: `[project.scripts] charliebot = "src.app.main:main"`.
Agents reach it through the shim `src/runtime/agent_entry_points/charliebot` (`env.md` section 3). Per
subcommand, this document gives the HTTP routes it calls, the files it reads or writes itself, and its exit codes.
Route payloads belong to the parallel HTTP contract and are not repeated here. **UNVERIFIED** marks a statement
not confirmed from source, with the reason.

## 0. The Rust port

The Rust CLI lives in `rust/crates/app/cli` (binary `charliebot`) with the shared client contract in
`rust/crates/infra/cli-common`. The `session` rows and the `plan` / `artifact check` rows in section 3 marked
*ported to Rust* run on the Rust binary; every other subcommand prints
`{"error": "<name> is not ported to the Rust CLI yet"}` and exits 1, and the six caller-less Python names
(`config`, `disk-sweep`, `router`, `storage`, `usage-ledger`, `voice-replay-eval`) are unknown subcommands
(exit 2). `artifact wrap` prints `{"error": "artifact wrap is not ported to the Rust CLI yet"}` and exits 1, and
`artifact publish` keeps the not-ported error of the `artifact` row in the port to come. Golden cases record the
Python CLI and replay it against the Rust binary; the record and replay commands are in
`rust/crates/app/cli/README.md`. Rust deviations: no port cache — the port comes from `<home>/config.yaml`'s
`server` section on every call; argument-parser help and usage text may differ from argparse (the exit codes
match); the dispatcher's vocabulary is the 14 caller-bearing names, so its help text and
unknown-subcommand line differ from the Python CLI's; the Python CLI's structlog console lines on stdout (a
wall-clock stamp and a pid per line) are not reproduced — the golden harness drops them from both sides.

## 1. Dispatcher (`src/app/main.py`)

- The vocabulary is `_RUNTIME_COMMANDS` (`config`, `delegate`, `schedule-trigger`, `gc-trash`, `session`) plus every
  `wiring.register_command(name, module)`:
  - `artifact`, `plan`, `publish` → `src.features.artifacts`
  - `disk-sweep` → `src.features.disk_sweep`
  - `memory` → `src.features.memory`
  - `usage-ledger`, `storage` → `src.features.usage`
  - `slack`, `discord`, `web` → `src.features.slack`, `src.features.discord`, `src.features.web_reply`
  - `router` → `src.features.router`
  - `improve`, `improve-stop` → `src.features.improve`
  - `voice-replay-eval` → `src.features.voice`
- No argument, `-h` or `--help` prints `usage: <prog> <subcommand> [args...]` and the sorted list, then exits 0.
- An unknown subcommand prints `"<prog>: unknown subcommand '<x>'. Available subcommands: <list>"` to stderr and
  exits 2.
- Otherwise it runs `registrations.register_all()`, sets `sys.argv[0] = "<prog> <subcommand>"`, and calls the
  module's `main()`.
- Every argparse usage error exits 2.

The 20 top-level subcommands that `uv run python -m src.app.main --help` lists at the baseline: `artifact`,
`compaction-context`, `config`, `delegate`, `discord`, `disk-sweep`, `gc-trash`, `improve`, `improve-stop`, `memory`,
`plan`, `publish`, `router`, `schedule-trigger`, `session`, `slack`, `storage`, `usage-ledger`, `voice-replay-eval`,
`web`. Each has a section
below.

## 2. Shared client contract (`src/runtime/cli/common.py`)

- **Base URL:** `http://localhost:<server.port>`. The port comes from `<home>/cache/cli_base_url.json` while its
  fingerprint (`(mtime, size)` of `config.yaml` and of the checkout's `src/infra/config.py`) matches; otherwise
  from a full `get_config()`, which rewrites the cache (`files.md` 5.5).
- **Headers:**
  - `Authorization: Bearer <CHARLIEBOT_RUN_TOKEN>` when that variable is set, else
    `Bearer <credentials.charliebot.access_key>` when that is set, else none;
  - `X-CharlieBot-Caller-Session: <CHARLIEBOT_SESSION_ID>` when that is non-empty.
- **Methods:** `post_internal_api` (POST JSON) and `get_api` (GET with query parameters). No CLI path uses another
  method.
- **JSON output:** every CLI JSON print goes through `print_json` (`src/runtime/cli/common.py:433`), which keeps
  non-ASCII characters as written (`ensure_ascii=False`). The escapes that `json.dumps` writes by default do not appear.
- **Connect failures** retry with backoff until `CLI_CONNECT_TOTAL_TIMEOUT` (60 s), then print
  `{"error", "code": "server_unavailable", "effect": "none"}` to stderr and exit 1.
- **Sent but no response:** the call is never re-issued. An optional `readback` runs once; when it finds the effect
  it is printed as the result. Otherwise the CLI prints `{"error", "code": "outcome_unknown", "effect": "unknown"}`
  and exits 1.
- **HTTP status >= 400:** `{"error": <detail>, "code": "server_error", "effect": "none", "hint"?}` on stderr, exit 1
  unless the command maps the status to another code. `hint` is a version-skew note built from
  `GET /api/internal/version` and the local `git rev-parse --short HEAD`.
- **Session resolution** (`resolve_session_id`):
  1. `CHARLIEBOT_SESSION_ID` wins; a disagreeing `--session` exits 2 with
     `"session id mismatch: ...; refusing to use an ambiguous session"`.
  2. Otherwise the session is `--session`, or the cwd's name when the cwd's parent is `<home>/sessions`. If both are
     given they must agree (else exit 2).
  3. With neither, the CLI exits 2 with `"--session required when not running from a CharlieBot session dir"`.
- **Other errors:** `exit_usage_error(msg)` exits 2 and `exit_error(msg)` exits 1, both with a message on stderr.

## 3. Subcommands

| Subcommand | HTTP | Files read or written directly | Exit codes | Rust port |
| --- | --- | --- | --- | --- |
| `artifact check <file> --genre G [--trigger T] [--assertions-only]` | Manager session: `POST /api/internal/delegate` (cold-read child). Worker session or none: no route. | Reads the artifact and `<home>/sessions/<sid>/metadata.json` (its `profile`). Writes and deletes `.page-height-probe-<hex>.html` beside the artifact. Runs `headless_chrome_bin`. A worker or session-less run calls a backend one-shot in the foreground, which starts an agent CLI. | 0 ok; 1 an assertion failed, the probe could not run, or the delegation was refused; 2 usage (for example a missing `--trigger`) | *ported to Rust*: all five genres' assertion sets; the checkout that supplies `prompts/` is `CHARLIEBOT_REPO_ROOT` when it holds `prompts/cold_read.md`, else the tree above the executable; a failed backend attempt prints `RuntimeError: …` (Python's exception-class prefix) and a build failure `ValueError: …`; the probe registers the backend packages itself and reads `<home>/config.yaml`'s `backends` and `headless_chrome_bin` without parsing other packages' sections |
| `artifact wrap <fragment> --genre G --output OUT [--math\|--no-math]` | none | Reads the fragment and the genre template; writes `OUT`; with math on, may fetch `<home>/vendor/katex/katex.min.js` (`files.md` 5.12) and runs `node` | 0; 1 a failed assembly or byte self-check; 2 usage (missing fragment) | not ported (task cli-wrap-publish) |
| `compaction-context --session SID --slot N` | none | Reads `sessions/<sid>/data/chat_events.jsonl` and current skill files from `<home>/skills/` or the repository's `skills/` directory. A named path in a Read or Bash event is tried first. Non-plain path patterns and unresolved skills are skipped and reported. Writes nothing. | 0 prints the SessionStart hook JSON for a non-empty slot; 1 on an unreadable log, invalid event JSON or unreadable skill; 2 usage |  |
| `config get <key>` | none | Reads `config.yaml` | 0 prints the value; 1 the value is null; 2 unknown key |  |
| `delegate --task-spec-file F --keep-worktree {0,1} [--session] [--repo] [--base-branch] [--backend] [--request-id] [--task-type]` | `POST /api/internal/delegate`, with a readback | Reads `F`. Without `--request-id`, the server derives `delegate-<sha256[:24]>` (`files.md` section 3), and the readback uses the same derivation. | 0 prints `{"session_id", "run_id"}` as indented JSON (`src/runtime/cli/delegate.py:199`), not the server's whole answer; 1 client contract; 2 usage |  |
| `discord reply --file F [--session]` | `POST /api/internal/discord/reply` | Reads `F` | 0; 1; 2 |  |
| `discord read [--url] [--limit N] [--session]` | `POST /api/internal/discord/read` | none | 0; 1; 2 (`--limit` out of range) |  |
| `discord check` | `POST /api/internal/discord/check` | none | 0 when the result's `ok` is true; 1 otherwise or on error |  |
| `disk-sweep [--yes]` | none | Reads Run metadata, lifecycle events, `paths.worktree_dir`, `paths.scratch_dir` and `/proc` to select old paths and skip paths in use (`src/features/disk_sweep/sweep.py`). Without `--yes` it prints candidate rows only. With it, it archives changed worktrees to the owner Run, removes worktrees through git and scratch directories with `rmtree`. Prints one row per candidate and a totals line | 0; 1 when a path fails to remove |  |
| `gc-trash [--yes]` | none | Lists `<paths.worktree_dir>/.trash/` with sizes; `--yes` deletes every entry (`rmtree`) | 0; 1 when a deletion fails |  |
| `improve --repo R --goal-file G --base-branch B [--iterations N] [--plan-file P] [--backend] [--work-branch] [--merge-back] [--session]` | `POST /api/internal/improve`, with a readback | Reads `G` and `P` | 0; 1; 2 |  |
| `improve-stop [--session]` | `POST /api/internal/improve/stop` | none | 0 prints `stopped`; 1; 2 |  |
| `memory query --topic T [--audience master\|worker]` | none | Reads `<home>/memory/` (creates the store scaffold on first use, `files.md` 5.10). With `CHARLIEBOT_RUN_TOKEN` set, it verifies the token against `credentials.charliebot.access_key` and reads `sessions/<sid>/metadata.json`, `data/runs/<run>/metadata.json` and `data/chat_events.jsonl` to fix the audience from the run's session profile. | 0; 1 (unknown topic, invalid token, inactive run, store error); 2 usage |  |
| `memory add [--file F]` | none | Writes `<home>/memory/staging/<ts>-<sess8>-<slug>.md` and prints its path | 0; 1; 2 |  |
| `memory lint [--dir D]` | none | Reads the store or `D` | 0, including warnings; 1 on violations; 2 |  |
| `memory proposal open\|status\|commit\|land` | none | Local git only, in `<home>/memory` and `<home>/memory-proposal` (branch `proposal`, worktree add, commit of one store path, fast-forward land). `status` prints `worktree`, `base`, `head`, `ahead`, `opened`, `dirty` and `diff_path` (`/diff?repo=<memory dir>&base=<base>&head=<head>`). `land` prints one `key: value` line per field of its result, and the result includes `diff_path`, the diff link of the landed range (`src/features/memory/memory_proposal.py:508`, `src/features/memory/cli.py:268-273`). | 0; 1 refusal; 2 |  |
| `plan present --file F [--session]`, `plan amend --file F --note N`, `plan approve [--plan]`, `plan close --plan P` | `GET /api/sessions/<sid>/plans`, then `POST /api/internal/plan/<verb>` | none (the server validates and writes `plans.json`) | 0; 1; 2 | *ported to Rust* |
| `plan diff [--plan] [--v]`, `plan list` | `GET /api/sessions/<sid>/plans` | `plan diff` takes the registry from the route and reads the two version files straight from the session directory (`_read_version_file`), then diffs them locally | 0; 1; 2 | *ported to Rust*: `plan_diff::diff_text`, the Rust differ's text form |
| `publish <artifact> [-- lane options]` | none | With `publish.command`: runs it with the artifact's absolute path plus the lane options; success is exit 0 and exactly one `http(s)://` line on stdout. Otherwise: needs `<publish.dir>/index.html`, creates `<publish.dir>/<token_urlsafe(16)>/` (0755), copies the file in (0644), and prints `<public_base_url>/<token>/<basename>`. | 0 prints the URL; 1 any failure (JSON on stderr); 2 usage |  |
| `router` | none | Reads one JSON request from stdin and writes one JSON answer to stdout. Reads `<home>/router.yaml` and the `api_key` in the credentials section named by `jev.credential`; posts the choice request to Jev. | 0; 1 config, credential or request failure; 2 usage |  |
| `schedule-trigger --max-wait S --message M [--watch SPEC...] [--cmd CMD [--host H] [--cwd D]] [--session]` | `POST /api/internal/schedule-trigger` (422 maps to exit 2) | With `--cmd`, it starts `bash -lc "(<cmd>\n); echo $? > <exit file>"` detached, locally or over `ssh <host>` (remote files in `/tmp/charliebot_runs/<launch_id>/`), and writes `<home>/sessions/<sid>/launches/<launch_id>/metadata.json` (`files.md` 6.7). Watch specs: `<pid>`, `<host>:<pid>`, `slurm:<job>`, `<host>:slurm:<job>`. | 0 prints `{trigger_id, fire_at, ...launch fields}`; 2 usage, or a rejected or failed registration (a started process is killed first); 3 the ssh start failed or the remote pid did not parse (nothing registered); 1 client contract |  |
| `session create [--name] [--backend] [--parent P --profile manager\|worker] [--task-file F] [--request-id] [--group G] ...` | `POST /api/sessions/`, then `POST /api/sessions/<id>/group` when `--group` is given | Reads `F` | 0 prints JSON; 1; 2 (for example `"v2 task create requires both --parent and --profile"`) | *ported to Rust*
| `session tree [--root] [--include-archived] [--limit]` | `GET /api/sessions/tree` | none | 0; 1; 2 | *ported to Rust*
| `session retry <sid> --run R [--request-id]` | `POST /api/sessions/<sid>/retry` | none | 0; 1; 2 | *ported to Rust*
| `session archive <sid> [--result-file F] [--request-id]` | `POST /api/sessions/<sid>/archive` | Reads `F` when given; its JSON must be an object. Without `F` the body is `{}`. The help text says an agent must give `F`. | 0; 1; 2 | *ported to Rust*
| `session acknowledge <sid> --input-ids IDS [--note]` | `POST /api/sessions/<sid>/task-inputs/acknowledge` | none | 0; 1; 2 | *ported to Rust*
| `session cancel <sid> --reason R [--request-id]` | `POST /api/sessions/<sid>/cancel`; `GET /api/sessions/<sid>/runs` when a stopped run has no outcome | Reads run rows only for unresolved stops; prints each unresolved run id and pid to stderr. | 0; 1 when a stopped run is still running; 2 usage or refusal | *ported to Rust*
| `session send (--message M \| --file F) ...` | `POST /api/internal/session-message` (422 maps to exit 2) | Reads `F` | 0; 2 refused (`EXIT_SEND_REFUSED`) or usage; 1 | *ported to Rust*
| `session dialog [--session] [--turns N]` | `GET /api/sessions/<sid>/bootstrap`, then pages of `GET /api/sessions/<sid>/events?before=<cursor>&limit=200`; with `--turns` the pages stop once N+1 separator messages are fetched | none. With `--turns N` (N of at least 1) the output is the last N turns plus the turn in progress, under the header `last N turns · from event E · K earlier messages`, or `events` when the bootstrap's `archive_offset` is above 0; a bootstrap without `archive_offset` exits 1 | 0; 1; 2 | *ported to Rust*
| `slack reply --file F [--session]` | `POST /api/internal/slack/reply` | Reads `F` | 0; 1; 2 |  |
| `slack ack --message-id TS [TS ...] [--session]` | `POST /api/internal/slack/ack` | none | 0; 1; 2 |  |
| `storage cool [--dry-run] [--min-idle-days N] [--session ID] [--vacuum] [--force]` | none | Runs `storage_cool.run_cool_sweep` in-process (`files.md` sections 5.8, 5.14 and 7). A real run first captures usage into `<home>/usage/ledger.sqlite3` and aborts on failure. It deletes raw transport files of cold sessions under `data/master_runs/*/` and `threads/*/data/`, and backend transcripts under the Claude login and `/home/<user>/.codex/sessions`. `--vacuum` runs `VACUUM` on `opencode.db` and refuses while an `opencode serve` writer lives, unless `--force`. | 0 prints the sweep table; 1 `ValueError`; 2 |  |
| `usage-ledger capture [--ledger PATH]` | none | Opens the ledger (default `<home>/usage/ledger.sqlite3`) and captures this host's usage logs: Claude login `projects/`, `/home/<user>/.codex/sessions`, `opencode.db`, and this home's run logs | 0 prints `<source>: <n> records written` lines; 1 on an uncaught error |  |
| `voice-replay-eval [--engines] [--terms] [--only] [--limit] [--out DIR] [--dry-run] ...` | none | Reads `<home>/sessions/*/voice/*.wav`, `.txt` and the chat events that hold the sent messages; runs the transcription backends in-process (cloud ones call their APIs). Writes `<out>/results.json` and `<out>/summary.md`, default `<home>/voice_eval/<UTC timestamp>/`. It refuses an `--out` inside the repo. | 0; 1 (`SystemExit` with a message); 2 usage |  |
| `web reply --file F [--keep-language REASON] [--session]` | `POST /api/internal/web/reply` | Reads `F` | 0; 1; 2 |  |
| `web dialog [--session]` | `GET /api/internal/web/dialog` | none | 0; 1; 2 |  |
| `web status set --file F [--session]` | `POST /api/internal/web/status/unread`, then `POST /api/internal/web/ask-check` for each unread item, then `POST /api/internal/web/status` after every item has a current read | Reads `F`; for a linked item, reads the host-served file and runs a light backend one-shot. The server refuses a stale plan link with 409 before any cold read. | 0 stored; 3 after recording cold reads without storing; 1; 2 |  |
| `web status confirm` | `POST /api/internal/web/status/confirm` | none. Prints the route's JSON answer on one line. | 0; 1; 2 |  |
| `web status show [--session]` | `GET /api/internal/web/status` | none. Each waiting item prints its `since` and `since_exact` lines under the item. | 0; 1; 2 |  |

## 4. Acceptance check

```
uv run python -m src.app.main --help | sed -n '/Available subcommands:/,$p' | tail -n +2 | awk '{print $1}' \
  | while read c; do grep -q "\`$c" rust/docs/contract/state/cli.md && echo "ok $c" || echo "MISSING $c"; done
```
