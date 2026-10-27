# Persisted state: every file the Python server reads or writes

Baseline: the Python tree at `4521a51b2` on `rust-backend`. A Rust server that shares a `CHARLIEBOT_HOME` with
this baseline must read every file below as the baseline writes it and must write it so the baseline reads it
back. Paths use `<home>` for the resolved `CHARLIEBOT_HOME`, `<sid>` for a session id, `<run>` for a run id and
`/home/<user>` for the operator's home directory. Module paths are repo-relative. A statement marked
**UNVERIFIED** names why it was not confirmed from source.

Related contracts in this directory: `config.md` (the `config.yaml`, `config.d/` and `credentials.yaml` schema),
`services.md` (lifespan and background services), `env.md`, `cli.md`, `processes.md`, and `ledger_schema.sql`
(the usage ledger's exact SQL).

## 1. Home resolution

- `src/infra/home.py::charliebot_home_dir` is the only reader of `CHARLIEBOT_HOME`. The value is stripped. Empty
  or unset gives `/home/<user>/.charliebot` (`Path.home() / ".charliebot"`, not resolved). A set value must start
  with `/` or `~`, else `ValueError("CHARLIEBOT_HOME must be an absolute path or start with '~'; got '<raw>'")`.
  A set value is `expanduser().resolve()`d, so symlinks in it are resolved. The result is cached per
  `(raw value, $HOME)` pair.
- `config.yaml` may not choose the home: a `charliebot_home` key raises (see `config.md`).
- `CharlieBotConfig.charliebot_home` holds the same path; every derived path below hangs off it.

## 2. Write disciplines

Each file kind below names one of these disciplines.

| Name | Mechanism | Source |
| --- | --- | --- |
| **ATOMIC** | Write to a sibling temp file named `.<name>.<uuid4 hex>.tmp` in the same directory, then `os.replace` onto the target. `private=True` creates the temp file with mode 0600; otherwise the mode follows the umask. No fsync of the file or the directory. A failed write unlinks the temp file. | `src/infra/json_utils.py::atomic_write_stream`, `atomic_write_text`, `write_json_atomically`, `write_model_json_atomically` |
| **ATOMIC-JSON** | ATOMIC with `json.dumps(value, ensure_ascii=False)`: compact separators `(",", ":")` when `indent` is None, else `indent=N`. `newline=True` appends `"\n"`. | `json_utils.write_json_atomically` |
| **ATOMIC-MODEL** | ATOMIC with pydantic `model_dump_json(indent=2)`. The parent directory is created first. | `json_utils.write_model_json_atomically` |
| **NDJSON-APPEND** | Per call: `os.open(path, O_WRONLY\|O_APPEND\|O_CREAT, 0o666)`, write the whole line, `os.fdatasync`, close. The line is `json.dumps(obj, ensure_ascii=False) + "\n"` with the stdlib default separators `", "` and `": "`. | `src/infra/ndjson.py::_append_ndjson_sync` (`append_ndjson`) |
| **HELD-APPEND** | One `O_WRONLY\|O_APPEND\|O_CREAT` descriptor held open for a run; each event is `orjson.dumps(event) + b"\n"` (compact, no spaces), with no fsync. | `src/runtime/worker.py` (`events.jsonl` of a worker run) |
| **CHILD-FD** | The server opens the file `O_WRONLY\|O_CREAT\|O_APPEND`, mode 0644, hands the descriptor to the child as stdout or stderr, and closes its own copy. Only the child writes. | `src/runtime/agent_process/base.py::AgentBackend.run` |
| **PLAIN** | `Path.write_text` / `write_bytes` / `open(..., "w")`: truncate and write in place, not atomic. | various, named per file |
| **YAML** | `yaml.dump(data, Dumper=<safe dumper>, allow_unicode=True, default_flow_style=False, sort_keys=False)` written with PLAIN `write_text(encoding="utf-8")`: block style, keys in insertion order, not atomic. | `src/infra/yaml_utils.py::save_yaml` |

NDJSON readers (`src/infra/ndjson.py`) parse with orjson, skip blank lines, and skip a line that does not parse.
A line with invalid UTF-8 is retried once with `errors="replace"`. A line-type prefilter (`type_line_filter`)
assumes a line starts with `{"type"`; lines that start with any other key fall through to a full parse. A Rust
writer must keep `"type"` as the first key of event lines that carry one, or the prefilter's fast path misses
them; correctness does not depend on it.

## 3. Shared formats

- **Timestamps.** Pydantic models (`metadata.json`, run `metadata.json`, triggers, improve state) serialize
  `UtcDatetime` as ISO 8601 with a `Z` suffix and microseconds when nonzero (`2026-01-01T00:00:00.123456Z`).
  Event logs and hand-built stamps use `datetime.now(UTC).isoformat()`, which ends in `+00:00`. Readers accept
  both: `src/infra/models.py::ensure_utc` parses with `datetime.fromisoformat` and treats a naive value as UTC.
- **Stable ids** (`src/runtime/control_events.py`). `NS = uuid5(NAMESPACE_URL, "charliebot/session-task-tree")`.
  Every id below is `str(uuid5(NS, <key>))`:

  | Id | Key |
  | --- | --- |
  | task (session) id | `task:{task_parent_id or 'root'}:{request_id}` |
  | run id | `run:{session_id}:{request_id}` |
  | close-request event | `close-request:{session_id}:{request_id}` |
  | close-blocked notice | `close-blocked-notice:{session_id}:{request_id}` |
  | task-closed event | `task-closed:{session_id}:{request_id}` |
  | task-reopened event | `task-reopened:{session_id}:{request_id}` |
  | input-ack event | `input-ack:{session_id}:{request_id}` |
  | request-keyed input or delivery event | `request:{session_id}:{request_id}` |
  | child report | `child-report:{child_session_id}:{source_event_id}:{recipient_session_id}` |
  | launch-withheld event | `run-launch-withheld:{run_id}:{reason}` |
  | turn-launch event | `turn-launch:{run_id}:{reason}` (`src/runtime/task_execution.py`) |

  `derived_delegate_request_id(session, task_type, description)` is
  `"delegate-" + sha256_hex("\x00".join([session, task_type, description]))[:24]`. `sha256_hex` hashes the UTF-8
  bytes and returns lowercase hex. Other request-id spellings seen in the baseline: `review:{work_run}:{n}`,
  `{request_id}:work`, `cold-read:work:{n}`, `scheduled-node:{cron name}`, `improve:{loop}`,
  `improve:{loop}:iter:{n}`, `history-copy-{id}`, `operator-create-{uuid4}`, `archive-{uuid4}`,
  `unsuccessful:{run_id}`, `auto:{firing_ref}`, and `dispatch:` + sha256 of the sorted pending input ids joined
  by `\x00`. A session that `charliebot session create` makes without `--request-id` uses a fresh uuid4 request
  id.
- **Chat-thread session ids.** Slack: `uuid5(UUID("1b4e28ba-2fa1-4d7a-9f0c-8d5e7a3b6c11"),
  f"slack:{team}:{channel}:{thread_ts}")`. Discord: `uuid5(UUID("13667dae-7551-489f-9cf1-d0819d0978e6"),
  f"discord:{guild}:{thread}")`.
- **Task-spec hash.** `canonical_task_spec_text(task) = orjson.dumps(task.model_dump()).decode()` (compact, field
  order of `TaskSpec`), and the hash is `sha256_hex` of that text (`src/runtime/task_sessions.py`).
- **Absolute paths in records.** `RunRecord.task_spec_ref`, `raw_log_ref`, `events_ref`, `result_ref`,
  `prompt_snapshot_ref`, `ThreadMetadata.worktree_path`, trigger `exit_file`/`log`, and launch `log`/`exit_file`
  hold absolute path strings as the writer resolved them. A home that moves invalidates them. A Rust writer must
  write absolute paths of the same form.

## 4. Home layout

```
<home>/
  config.yaml                      config.md
  credentials.yaml                 config.md
  router.yaml                      charliebot router
  config.d/cron.d/<name>.yaml      config.md, section 9
  MASTER_AGENT_PROMPT.md           read only
  host_auth.json
  state/home_writer.lock
  state/writer_identity.json
  state/cron_alert_fingerprint.json
  state/boot_recovery.json         section 5.19
  cache/cli_base_url.json
  cache/perfetto_merge/<sha256>.json.gz
  usage/ledger.sqlite3             (+ SQLite -journal while a transaction is open)
  prompt_bodies/<sha256>.md
  memory/                          git repository
  memory-proposal/                 git worktree of memory/ on branch "proposal"
  models/                          not created by the server
  vendor/katex/katex.min.js
  voice_eval/<ts>/                 CLI output
  opencode/{data,state,cache}/     opencode server XDG store
  sessions/
    .counter
    .task-<id>-<pid>-<hex>.tmp/    create staging, renamed into place
    <sid>/                         one session (task-tree node)
```

`init_seed.init_charliebot_home` creates `<home>`, `<home>/sessions` and `<home>/config.d` at startup
(`mkdir(parents=True, exist_ok=True)`). Every other directory is created on first write.

Session directory:

```
sessions/<sid>/
  metadata.json
  CLAUDE.md | AGENTS.md            manager-turn instructions file (backend-dependent)
  data/chat_events.jsonl
  data/archives/chat_events.<isoyear>-W<ww>.jsonl
  data/explain_results.json
  data/runs/<run>/...              v2 runs
  data/master_runs/<started_at>/   legacy (read only)
  threads/<thread_id>/...          legacy (read only)
  triggers/<trigger_id>.json
  launches/<launch_id>/{metadata.json,log,exit}
  plans.json
  artifacts/...                    agent-written pages; opaque except where plans.json points
  uploads/<filename>
  voice/<stamp>_<8hex>.wav, .txt
  loops/active.lock, loops/<n>/...
```

A manager turn runs with the session directory as its working directory, so agent CLIs may create further
files there. The server treats every file not listed in this document as opaque.

## 5. Home-level files

### 5.1 `config.yaml`, `credentials.yaml`, `config.d/cron.d/<name>.yaml`

Schema, defaults and errors: `config.md`. Writers:

- `src/runtime/init_seed.py::init_charliebot_home`, at every startup. If `config.yaml` is missing it is copied
  from `configs/config.example.yaml`; when that is absent it is written as YAML
  `{paths: {workspace_dirs: ["~/workspace"], worktree_dir: "~/worktrees"}}`. If `credentials.yaml` is missing it
  is copied from `configs/credentials.example.yaml` and chmod 0600. Existing files are never touched.
- Cron files: `src/features/cron/api.py` (HTTP edits), `src/features/cron/seed.py` (setup step), and
  `src/features/cron/cron_files.py::write_cron_key` (single-key write-back, for example the `session_id`
  auto-bind). All three use YAML.

Readers: `src/infra/config.py::load_config` (hot-reloaded on the file's `(mtime, size)` fingerprint; a reload
that fails keeps the previous config and logs `config_reload_failed`), `src/infra/credentials.py::load_credentials`
(same reload rule), and `src/features/cron/loader.py`.

### 5.2 `MASTER_AGENT_PROMPT.md`

- Format: UTF-8 Markdown, operator-owned. Every occurrence of `YOUR_SESSION_UUID` is replaced with the session
  id when it is read.
- Writer: none in the server.
- Reader: `src/runtime/task_prompts.py::_host_supplement` adds it to the manager rule segments when the file
  exists. An unreadable file raises `TaskPromptError("host supplement is unavailable: <path> (<error>)")`.

### 5.3 Home writer fence: `state/home_writer.lock`, `state/writer_identity.json`

Module: `src/runtime/home_writer_fence.py`. One server per home.

- `state/home_writer.lock`: an empty file. Opened `O_RDWR|O_CREAT|O_NOFOLLOW` mode 0600, then
  `flock(LOCK_EX|LOCK_NB)`. The descriptor is held for the server's life.
- `state/writer_identity.json`: written after the lock is taken, ATOMIC, as
  `json.dumps(identity, indent=2, sort_keys=True)`. Fields:

  | Field | Type | Meaning |
  | --- | --- | --- |
  | `argv` | str | the holder's `sys.argv` joined with single spaces (`" ".join(os.sys.argv)`), not a JSON list |
  | `home` | str | the resolved home |
  | `pid` | int | holder pid |
  | `pid_start` | str | `/proc/<pid>/stat` field 22 of the holder |
  | `purpose` | str | `"server startup"` for the server |
  | `started_at` | str | ISO 8601 UTC |

- The state directory and both files must not be symlinks; a symlink refuses the acquire.
- A held lock raises `HomeWriterActiveError`; server startup logs `server_startup_refused_home_writer_fence` and
  fails.
- Release (at shutdown, in a `finally`) unlinks `writer_identity.json` first, then unlocks and closes.
- `probe` (used by other tools) tries `LOCK_SH|LOCK_NB` and reads the identity file.
- Other holders: `scripts/v1_session_conversion.py` acquires the fence for `apply` (purpose
  `"v1 session conversion apply"`) and `rollback` (`"v1 session conversion rollback"`), and probes it for
  `dry-run`.

A Rust server must take the same `flock` on the same path before any write, so it and a Python server can never
both write one home.

### 5.4 `state/cron_alert_fingerprint.json`

- Format: ATOMIC-JSON with `newline=True`; a JSON list of cron task names (strings).
- Writer and reader: `src/features/cron/loader.py`. It records the set of broken cron files already reported
  through Telegram, so each new breakage alerts once.

### 5.5 `cache/cli_base_url.json`

- Format: ATOMIC-JSON, written with `private=True` (0600).
  `{"fingerprint": [[mtime, size] of config.yaml, [mtime, size] of src/infra/config.py], "port": N}`.
- Writer and reader: `src/runtime/cli/common.py`. The CLI caches the server port so it can skip parsing
  `config.yaml`. A fingerprint mismatch triggers a fresh config parse and a rewrite.
- The server never reads it.

### 5.6 `cache/perfetto_merge/<sha256>.json.gz`

- Format: gzip of merged Perfetto trace JSON. The temp file comes from `tempfile.mkstemp` in the same directory,
  then `os.replace`. Least-recently-used eviction keeps 24 entries.
- Writer and reader: `src/features/trace/trace_merge.py` (through `src/features/trace/api.py`, run in the
  `merge_pool` process pool).
- **UNVERIFIED:** the exact cache-key inputs behind `<sha256>`; the key is a content hash, so a Rust port can
  ignore existing entries.

### 5.7 `host_auth.json`

- Format: ATOMIC with `json.dumps(indent=2) + "\n"`.
- Writer and reader: `src/features/host_auth/host_auth.py`.

| Field | Type | Meaning |
| --- | --- | --- |
| `probed_at` | str or null | ISO time of the last full probe pass |
| `probe_running` | bool | a pass is in progress |
| `interval_sec` | int | 1800 |
| `ttl_sec` | int | 604800 |
| `hosts` | list | one entry per collectable `Host` block in `/home/<user>/.ssh/config`: no wildcard or negation, and a `HostName` line |
| `hosts[].alias` | str | ssh alias |
| `hosts[].hostname` | str | the block's `HostName` |
| `hosts[].status` | str | probe verdict |
| `hosts[].detail` | str | probe output summary |
| `hosts[].last_probe_at`, `last_change_at`, `last_ok_at`, `enrolled_observed_at` | str or null | ISO times |

`hosts[].status` is one of `ok`, `needs-okta`, `needs-interactive-auth`, `unreachable`. A host in
`needs-okta` is re-probed on the longer blocked backoff instead of the 1800 s interval.

### 5.8 Usage ledger: `usage/ledger.sqlite3`

Module: `src/features/usage/usage_ledger.py`. Path: `default_ledger_path() = <home>/usage/ledger.sqlite3`. The
`usage-ledger capture --ledger` CLI flag can point elsewhere.

- **Open sequence:** `mkdir -p` the parent, `sqlite3.connect(path, timeout=30)`, `executescript(_SCHEMA)` (every
  `CREATE ... IF NOT EXISTS`), then `_refresh_agg_triggers`, then commit. The default rollback journal is used: no
  PRAGMA sets WAL.
- **Trigger refresh:** the open compares the stored `sqlite_master.sql` of the five sum triggers
  (`usage_agg_after_insert`, `_after_update`, `_after_delete`, `_after_native_session`, `_after_fallback_session`)
  with the module's own text (`_AGG_TRIGGER_SQLS`). When any differs or is missing, it runs `BEGIN IMMEDIATE`,
  re-checks, drops those five, and re-runs all six `CREATE TRIGGER IF NOT EXISTS` statements, so
  `usage_agg_purge_empty` is created only when absent and never compared. If `ledger_meta.agg_backfilled` exists it
  then wipes `usage_agg` and backfills it from `usage`, then commits.
- **Consequence for a port:** a writer whose trigger text differs by one byte makes every Python open rewrite the
  triggers and re-aggregate, and the Python text then makes the Rust open do the same. Store the triggers with the
  exact text in `ledger_schema.sql`.

Tables (verbatim DDL and triggers: `ledger_schema.sql`):

| Table | Columns | Meaning |
| --- | --- | --- |
| `usage` | `record_id TEXT PK`, `kind TEXT` (`native` or `fallback`), `source`, `model`, `account`, `host`, `ts` (ISO; `SUBSTR(ts,1,10)` is the day), `in_fresh`, `cache_write`, `cache_read`, `in_unsplit` (default 0), `output` (all INTEGER, NOT NULL), `origin TEXT`, `captured_at TEXT` | One model call's tokens. `in_unsplit` holds input whose cache split was never logged. |
| `captured_files` | `host`, `path`, `sig`; PK `(host, path)` | Last ingested signature per source file, so a capture skips unchanged files. |
| `native_sessions` | `session TEXT PK`, `source TEXT` | Sessions that have native usage records. |
| `fallback_sessions` | `record_id`, `session`; PK both | Sessions a fallback record covers; a fallback counts only while none of its sessions is native. |
| `ledger_meta` | `key TEXT PK`, `value TEXT` | `last_capture_at:<host>` (ISO UTC end of that host's last capture) and `agg_backfilled`. |
| `usage_agg` | `source`, `model`, `account`, `kind`, `day`, `calls`, `in_fresh`, `cache_write`, `cache_read`, `in_unsplit` (default 0), `output`; PK `(source, model, account, kind, day)` | Trigger-maintained daily aggregate; a row whose `calls` reaches 0 is deleted. |

Index: `usage_group_cover ON usage(kind, source, model, account, ts, in_fresh, cache_write, cache_read,
in_unsplit, output)`.

- **Writers:** `UsageLedger` from `src/features/usage/token_tally.py::capture_local`. Callers are the
  `usage_ledger` cron handler, `charliebot usage-ledger capture`, and `storage_cool` before it deletes anything.
  A capture runs inside a `SAVEPOINT` batch.
- **Readers:** `src/features/usage/api.py` (the usage page).
- **UNVERIFIED:** the exact `record_id`, `origin` and `sig` spellings per source; they live in each backend's
  `usage_logs.py` and were not read line by line.

### 5.9 `prompt_bodies/<sha256>.md`

- Format: UTF-8 text whose `sha256_hex` is the file name. Immutable: written ATOMIC
  (`src/runtime/task_sessions.py`, `json_utils.atomic_write_text`). Re-writing an existing name with different
  content raises.
- Readers: `src/runtime/task_prompts.py::read_local_rule_body`. A body that does not hash back to its name raises
  `TaskPromptError("... rule of task <owner> is corrupt: prompt_bodies/<ref>.md hashes to <actual>")`.
- Referenced from session metadata `subtree_prompt_ref` and `node_prompt_ref`, and from prompt snapshot sources
  (`prompt_bodies/<ref>.md`).

### 5.10 Memory store: `memory/` and `memory-proposal/`

Modules: `src/features/memory/memory.py`, `memory_proposal.py`, `cli.py`, `store_root.py`.

- `memory/` is a git repository, created by `ensure_store` on first use (`git init`), never at server startup.
  Contents:
  - `topics`: one topic per line, `name` or `name resident`, matching `^[a-z0-9][a-z0-9-]*( resident)?$`.
    Seeded only if missing with `profile resident`, `communication resident`, `workflow resident`,
    `rulings resident`, `host resident`, `charliebot`.
  - `.gitignore`: seeded only if missing with `staging/`.
  - `entries/<topic>/<slug>.md`: format v2. Line 1 is exactly `---`. Header lines match `^([a-z_]+): (.+)$` up
    to the next `---` line; the fields are `scope`, `topic`, `audience` (comma list of `master` and `worker`) and
    `title`. A free Markdown body follows.
  - `staging/<%Y%m%dT%H%M%SZ>-<sess8>-<slug>.md`: free-form captures from `charliebot memory add` (PLAIN).
    `sess8` is the first 8 characters of the cwd's session directory name, or `nosess` when the cwd is not a
    session directory. `slug` is the lowercased title with runs of characters outside `[a-z0-9._-]` collapsed to
    `-`, or `capture` when nothing is left.
- `memory-proposal/` is a `git worktree` of `memory/` on branch `proposal`, created by
  `charliebot memory proposal open`. `land` fast-forwards the live checkout to one approved commit. All git
  operations are local subprocesses.
- Readers: `charliebot memory query` and task prompt assembly (`src/runtime/task_prompts.py` through
  `load_store`). `wiring.register_diff_root` exposes `memory/` to the diff viewer.

### 5.11 `models/`

The server does not create a speech-model directory. Python removed the local speech engine and its provisioning at
`d1f7e3ba6` (`4c3bd3fda`); the Rust voice port removes its matching startup service and model files.

### 5.12 `vendor/katex/katex.min.js`

- Writer: `src/features/artifacts/artifact_wrap.py::ensure_vendored_katex`, run by `charliebot artifact wrap`
  with math on. When the file is absent it fetches
  `https://cdn.jsdelivr.net/npm/katex@<KATEX_VERSION>/dist/katex.min.js` (PLAIN `write_bytes`).
- Reader: the same module's Node pre-render (`node <driver> <fragment> <katex path>`).

### 5.13 `voice_eval/<ts>/results.json`, `summary.md`

- Output of `charliebot voice-replay-eval` (`src/features/voice/replay_eval.py`). It is the CLI's report; no
  server code reads it.

### 5.14 opencode store: `opencode/`

- `opencode/{data,state,cache}` are the `XDG_DATA_HOME`, `XDG_STATE_HOME` and `XDG_CACHE_HOME` of the shared
  `opencode serve` process (`src/infra/home.py::opencode_*_dir`). The directories are created by
  `src/backends/opencode/server.py::_start`.
- The opencode binary owns the database `opencode/data/opencode/opencode.db`. CharlieBot reads it for usage
  (`src/backends/opencode/usage_logs.py`) and runs `VACUUM` on it only through `charliebot storage cool --vacuum`.
- Backups exclude the whole top-level `opencode/` directory.

### 5.15 `sessions/.counter`

- Format: a plain decimal integer with no newline.
- Writer and reader: `src/runtime/task_sessions.py` (PLAIN `write_text(str(n + 1))`). It numbers default session
  names `Session N`.
- Not atomic. A missing, unreadable or non-integer counter falls back to the count of directories in
  `sessions/` (`_count_session_dirs`), and the next write repairs the file.

### 5.16 `sessions/.task-<id>-<pid>-<uuid hex>.tmp/`

- The staging directory for publishing a new task node (`src/runtime/task_sessions.py`). It holds
  `data/chat_events.jsonl` (the `task_created` control event) and `metadata.json`, then `os.replace`s onto
  `sessions/<id>`.
- Every scanner skips names that start with `.task-` and end with `.tmp`
  (`src/runtime/v1_sessions.py::count_v1_sessions`, `task_sessions`).
- The writer removes its staging directory in a `finally`. A process killed between `mkdir` and `os.replace`
  leaves it behind, and no baseline code sweeps such leftovers.
- Losing a create race: `os.replace` onto an existing non-empty `sessions/<id>` fails, and the writer returns the
  existing node's metadata (stable ids make the race idempotent).
- A new node's `backend` is the explicit backend, else the parent's backend, else `backends.options[0].id`.

### 5.17 Usage caches

- Persisted: only the ledger (section 5.8). Its `captured_files` table holds the last ingested signature per source
  file, which a capture uses to skip unchanged files, and `ledger_meta` holds `last_capture_at:<host>`.
- In memory only, lost at restart:
  - the quota panel cache `_cached_usage` in `src/features/usage/ext_usage.py`, keyed `<provider>:<label>`, served by
    `GET /api/ext-usage` and broadcast on the sidebar channel;
  - the stat-signature memos of the NDJSON readers (line counts and tails, `src/infra/ndjson.py`).

### 5.18 `router.yaml`

- Writer: none in the server or CLI. The operator copies and edits this host-owned file.
- Reader: `src/features/router/routing.py::load_config`, on every `charliebot router` invocation.
- Shape: a YAML mapping with `jev: {url, model, credential}`, `instructions`, `routine`, `design` and `strong`.
  `strong` is an ordered list of backend ids. `jev.credential` names the `credentials.yaml` section whose
  `api_key` authenticates the request. Missing or invalid fields stop the router command with an error.

### 5.19 `state/boot_recovery.json`

- Writers: Python `src/runtime/task_recovery.py::write_boot_recovery_marker` (line 77) and Rust
  `runtime/tasks/src/recovery.rs::write_boot_recovery_marker`, each called after a task-tree recovery pass in
  which no node failed. The write is atomic: `boot_recovery.json.tmp`, then a rename. Python fsyncs the tmp file
  before the rename (`task_recovery.py:87`); the Rust write does not sync it (`recovery.rs:70-81`), so a power loss
  can leave an empty marker that Python reads as absent.
- Format: `{"pass_started_at": <pass start, UTC isoformat>}`, one line with Python's JSON spacing and no trailing
  newline. The timestamp is Python `datetime.isoformat()` text, including `+00:00`.
- Readers: Python `read_boot_recovery_marker` (line 58) and Rust `read_boot_recovery_marker` at the start of each
  pass. A missing file reads as no marker. Python also reads a file that is not UTF-8, is not JSON, is not an object,
  has no string `pass_started_at`, or has a timestamp that does not parse as no marker: it logs
  `boot_recovery_marker_unreadable` and the pass checks every node in full (`task_recovery.py:58-75`). Any other read
  error still raises. Rust's reader returns that error for every such file (`runtime/tasks/src/recovery.rs:53-68`),
  so a corrupt marker stops the boot there; `rust/docs/PORT_STATUS.md` lists the gap. The next pass skips a closed node that the marker proves unchanged
  (`services.md` section 3). Python keeps a node when either durable file has an mtime within one second before the
  marker; this slack covers coarse filesystem timestamps.

## 6. Session files

### 6.1 `sessions/<sid>/metadata.json` (session)

- Model: `src/infra/models.py::SessionMetadata`, `extra="allow"`, `schema_version: Literal[2]`.
- No `resource_class` field. The repair mark and its `PUT /api/sessions/{sid}/resource-class` writer are removed (`637df0572`), so no save writes the key. A file written before the removal keeps its `resource_class` key as an extra, because `SessionMetadata` allows extra keys (`src/infra/models.py:356`); nothing reads it.
- Writer: `src/runtime/session_store.py`. It writes ATOMIC with
  `model_dump_json(indent=2, exclude=TRANSIENT_METADATA_FIELDS)` under a per-session asyncio lock. When a write
  does not own the anchor fields, `cc_session_id` and `native_backend` are re-read from disk first, so a stale
  in-memory copy cannot roll them back.
- Readers: `session_store` (30 s TTL cache; an empty file logs `session_metadata_empty`),
  `src/runtime/run_identity.py`, `src/runtime/v1_sessions.py`, `src/features/artifacts/cli.py` (reads `profile`
  directly), `src/features/usage/storage_cool.py`, and every listing and sidebar path.
- Validation (`validate_session_metadata`): a missing `profile`, or `schema_version != 2`, raises a message that
  ends with `run scripts/v1_session_conversion.py`. At startup, `v1_sessions.require_no_v1_sessions` refuses to
  serve when any session directory's `metadata.json` has a null or missing `profile`. That error names the home,
  the count and the two commands
  `uv run python scripts/v1_session_conversion.py dry-run --home <home>` and `... apply --home <home>`.

On-disk key order with every baseline package registered (verified by dumping a model). Package-owned fields come
from `metadata_slot_registration.register_metadata_fields` and are placed after the field they name.

| Key | Type | Default | Owner | Meaning |
| --- | --- | --- | --- | --- |
| `id` | str | a fresh uuid4 | core | session id; task nodes use the stable uuid5 of section 3 |
| `name` | str | required | core | display name |
| `status` | `active`, `archived` | `active` | core | archived hides the session and its subtree |
| `has_unread` | bool | false | core | unread badge |
| `starred` | bool | false | core | star |
| `created_at`, `updated_at` | datetime | now | core | |
| `cc_session_id` | str or null | null | core | backend-native conversation id (anchor) |
| `cc_session_started_at` | datetime or null | null | core | |
| `claude_account` | str or null | null | `src.backends.claude_code` | Claude pool account label |
| `backend` | str | `""` | core | backend option id |
| `scheduled_task` | str or null | null | `src.features.cron` | cron task bound to this session |
| `last_scheduled_run` | datetime or null | null | `src.features.cron` | |
| `last_run_status` | `running`, `success`, `failed`, `skipped`, or null | null | `src.features.cron` | |
| `last_scheduled_cron` | str or null | null | `src.features.cron` | |
| `parent_session_id` | str or null | null | core | fork parent |
| `successor_session_id` | str or null | null | core | clone successor |
| `slack_origin` | `{team_id, channel_id, thread_ts}` or null | null | `src.features.slack` | |
| `slack_watermark_ts` | str or null | null | `src.features.slack` | |
| `discord_origin` | `{guild_id, parent_channel_id, thread_id}` or null | null | `src.features.discord` | |
| `discord_watermark_id` | str or null | null | `src.features.discord` | |
| `schema_version` | int, must be 2 | 2 | core | |
| `task_parent_id` | str or null | null | core | task-tree parent; null is a root |
| `profile` | `manager`, `worker` | required | core | |
| `task` | TaskSpec or null | null | core | see below |
| `created_by_event` | `{session_id: str, event_id: str\|null}` or null | null | core | the control event that created the node |
| `origin_ref` | `{session_id, event_id}` or null | null | core | the event this node originates from |
| `subtree_prompt_ref`, `node_prompt_ref` | str or null | null | core | `prompt_bodies` sha256 names |
| `native_prompt_hash`, `native_backend`, `native_model` | str or null | null | core | identity of the native conversation (`native_backend` is an anchor) |
| `round_ratings` | dict event id to `thumbs_up` or `thumbs_down` | `{}` | core | |
| `group` | str or null | null | core | sidebar group |
| `archive_offset` | int | 0 | core | count of chat event lines moved to archives |
| `backend_from_pool` | bool | false | `src.features.backend_pool` | backend chosen by pool rotation |
| `route` | `{backends: [str], reason: str}` or null | null | `src.features.backend_pool` | router stdout saved on a node that asked the router before selecting a backend |
| `web_status` | `{goal, now, waiting: [{text, context, link, since, since_exact}], updated_at}` or null | null | `src.features.web_reply` | Older items can lack `context` and serialize without it; new status writes require non-blank context. `since` and `since_exact` are written by the server (`src/features/web_reply/metadata.py`); an item stored before them carries neither, and the status read fills them from the status time. |

`TaskSpec` (`extra="forbid"`): `goal: str`, `acceptance: list[str]`, `context_refs: list[str]`,
`repo_path: str|None`, `base_branch: str|None`, `task_type` (`implement`, `quick-edit`, `script-run`, `verify`,
`cold-read`), `keep_worktree: bool`, `cold_read: {page_path, genre, trigger}|None` (all three required strings).
Defaults: `goal: ""`, `acceptance: []`, `context_refs: []`, `repo_path: null`, `base_branch: null`,
`task_type: null`, `keep_worktree: false`, `cold_read: null`.

Never written (`TRANSIENT_METADATA_FIELDS`, computed per read): `has_pending_plan_approval`,
`has_pending_trigger`, `has_running_tasks`, `next_trigger_at`, `pending_trigger_count`, `run_backend`,
`schedule_allow_failure`, `schedule_cron`, `schedule_enabled`, `schedule_next_run`, `schedule_project`,
`schedule_timezone`, `thinking_since`, `work_state`. Unknown keys are kept on read (`extra="allow"`) and written
back.

### 6.2 `sessions/<sid>/data/chat_events.jsonl`

- Module: `src/runtime/chat_events.py`. The live event log of the session.
- Writer: NDJSON-APPEND. `persist` adds `id` (uuid4) and `timestamp` (`+00:00` form) when missing. Lines are any
  event dict with a `type`. Control events (`src/runtime/control_events.py::build_control_event`) start with the
  header keys `id`, `type`, `timestamp`, `actor` (`user`, `agent` or `system`) and `source_session_id`, followed
  by the payload. For example `task_created` carries `request_id`, `task_parent_id` and `task_spec_hash`.
- A chat input request carries its `request_id` into the event. Its `id` is
  `uuid5(NS, "request:<sid>:<request_id>")`; a replay with the same id and payload returns the existing event, while
  reuse with a different event type, content or extra payload conflicts. Agent sends and recorded Slack, Discord and
  web replies can also carry a request id, so a retry does not append another event. `/api/chat/<sid>/message`
  requires a non-empty request id.
- Archive: lines that move out go to `data/archives/chat_events.<isoyear>-W<isoweek:02d>.jsonl` (opened `"a"`).
  The live file is then rewritten ATOMIC with the kept lines, and `metadata.json.archive_offset` grows by the
  moved count. An event's global index is `archive_offset + line index in the live file`.
- Fork and clone (`src/runtime/session_fork.py`): the child's file is written ATOMIC. It holds the parent's raw
  archive lines and live lines, then a `clone_start` marker and the child's creation fact.
- Readers: `src/runtime/session_events.py`, `message_projection.py`, `run_identity.py`, `task_completion.py`,
  `src/features/explain/explain.py` (copies it to a 0444 temp file for a one-shot), `session_fork`, and the
  agents themselves (a manager is told its history is at `data/chat_events.jsonl`).
- **UNVERIFIED:** the full event-type vocabulary; `src/infra/event_types.py` lists the names, and the route and
  WebSocket contract task owns their payloads.
- `web_ask_check` records one cold read of a Waiting on you item: `item_sha256`, `backend` and `answer` accompany
  the content `Cold read of a Waiting on you item`. `POST /api/internal/web/ask-check` appends it; the status set
  uses its digest to decide whether a waiting item has been read as it stands.

### 6.3 `sessions/<sid>/data/explain_results.json`

- Format: ATOMIC-JSON `indent=2`, mapping `str(upto event index)` to an entry
  `{state, backend, answer, error, requested_at, generated_at}`. `state` is `pending`, `ready` or `error`.
- Writer and reader: `src/features/explain/explain.py`. A pending entry older than the one-shot budget is reaped
  to `error` on read.

### 6.4 Runs: `sessions/<sid>/data/runs/<run>/`

Module: `src/runtime/runs.py` (with `worker.py`, `task_execution.py`, `agent_process/base.py`).

| File | Writer and discipline | Content |
| --- | --- | --- |
| `task_spec.md` | `runs.register_run_locked`, ATOMIC, before `metadata.json` | the canonical task-spec text; for a scheduled run, the configured cron prompt or that step's configured prompt before chaining; for a review run, `review of work run <id>`. Its sha256 is `task_spec_hash`. |
| `metadata.json` | `runs`, ATOMIC-MODEL | `RunRecord` (below) |
| `events.jsonl` | `worker.py` HELD-APPEND for streamed events; terminal events through NDJSON-APPEND | translated events of the run |
| `agent.raw.ndjson` | CHILD-FD (stdout) | the agent CLI's raw stdout |
| `agent.stderr.log` | CHILD-FD (stderr) | raw stderr |
| `agent.raw.cursor` | `runs.RawCursorWriter` (held by the tail-follow loop): descriptor opened `O_WRONLY\|O_CREAT` 0o666, each checkpoint one `pwrite` at offset 0 | the byte offset of `agent.raw.ndjson` consumed so far, as exactly 20 zero-padded decimal digits (`CURSOR_FIELD_BYTES = 20`), no newline |
| `agent.raw.ndjson.<N>`, `agent.stderr.log.<N>` | `base._rotate_stale_transport` (rename) | a previous attempt's transport. Before a fresh spawn into a directory whose raw log is non-empty, both files get the next free suffix `N` (1, 2, ...) and `agent.raw.cursor` is deleted |
| `stdout.log`, `stderr.log` | antigravity backend (`O_TRUNC` 0o666) | tee of `agy` stdout and stderr |
| `task.md` | charlie-code backend, PLAIN | the task text handed to `charlie-code --task-file` |
| `harness_instructions.md` | `base._write_instructions_file`, PLAIN | instructions of a worker or review run (kept out of the worktree) |
| `prompt_snapshot.json` | `task_execution`, ATOMIC, `indent=2, ensure_ascii=False` | `{blocks: [{sources: [{scope, source_ref, source_session_id}], body_ref, delivery, text}], prompt_hash, char_count}`; `body_ref = sha256_hex(text)` |
| `launch_prompt.md` | `task_execution`, ATOMIC | the exact launch text handed to the adapter |
| `cold_read.json` | `task_execution` | `{page_path, page_sha256, error?}` evidence of a cold-read attempt |
| `hang_diagnostics.json` | `worker.py`, PLAIN `json.dumps(indent=2)` | diagnostics when a run hangs after its result |
| `worktree-changes.tar` | `disk_sweep._archive_worktree`, temporary tar then ATOMIC rename | `tracked.patch` and unignored untracked worktree files under `untracked/`, written before an old changed worktree is removed |

`RunRecord` (`extra="forbid"`):

| Field | Type | Meaning |
| --- | --- | --- |
| `id` | str | run id (stable uuid5) |
| `session_id` | str | owner node |
| `kind` | `manager_turn`, `work`, `review`, `iteration`, `scheduled_step` | |
| `input_event_ids` | list[str] | chat events this run consumes |
| `task_spec_hash`, `task_spec_ref` | str or null | |
| `prompt_snapshot_ref` | str or null | |
| `retry_of_run_id`, `review_of_run_id` | str or null | |
| `backend`, `model` | str or null | backend option id and model; launch refuses a run with no backend |
| `native_session_id` | str or null | backend conversation id |
| `pid` | int or null | child pid |
| `pid_start` | str or null | `/proc/<pid>/stat` field 22 |
| `started_at`, `ended_at` | datetime or null | |
| `exit_code` | int or null | |
| `repo_path`, `base_branch`, `branch_name`, `worktree_path` | str or null | worker git context |
| `sequence_ref` | `{kind, owner_ref, position}` or null | position in a cron step chain or an improve loop |
| `raw_log_ref`, `events_ref`, `result_ref` | str or null | absolute paths of evidence files |

Order rules: `run_finished` is appended to `chat_events.jsonl` before the run's `metadata.json` mirrors the end.
The chat event is the fact and the run file is its projection.

Liveness of a recorded process: the pid exists, `/proc/<pid>/stat` field 22 equals `pid_start`, the process is not
a zombie, and the record's `started_at` is later than the host boot time (`/proc/stat` `btime`). A restart
re-attaches to a live process by reading `agent.raw.ndjson` from the offset in `agent.raw.cursor`.

### 6.5 Legacy run layouts (read only)

- `data/master_runs/<started_at>/` and `threads/<thread_id>/data/` (with `threads/<thread_id>/metadata.json` as
  `ThreadMetadata`) are read by recovery, `run_identity` and `storage_cool`; no baseline code creates them.
  **UNVERIFIED:** that no baseline path still writes them; the grep found only readers and the sweep.
- `ThreadMetadata` (`extra="allow"`), key order: `id`, `session_id`, `description`, `status` (`idle`, `running`,
  `completed`, `failed`, `cancelled`), `created_at`, `started_at`, `completed_at`, `pid`, `pid_start`,
  `exit_code`, `claude_session_id` (Claude package slot), `branch_name`, `base_branch`, `repo_path`,
  `worktree_path`, `context`, `backend`, `model`, `keep_worktree`, `tried_backends`, `task_type`.

### 6.6 `sessions/<sid>/triggers/<trigger_id>.json`

- Model: `PendingTrigger`. Writer: `src/runtime/triggers.py`, ATOMIC-MODEL. Files are never deleted; a fired or
  cancelled trigger keeps its file with the new status.
- Fields: `id` (uuid4), `session_id`, `fire_at`, `message`, `created_at`, `status` (`pending`, `fired`,
  `cancelled`), `fired_at`, `watch_targets` (list of `{kind: "local_pid", pid, exit_file, log}`,
  `{kind: "remote_pid", host, pid, exit_file, log}`, or `{kind: "slurm_job", host, job_id}`; discriminated on
  `kind`), `fire_reason` (`completed`, `timeout`, or a cancel reason).
- At most 5 pending triggers per session (`MAX_PENDING_TRIGGERS`).
- Readers: `TriggerManager.recover_pending` scans `sessions/*/triggers/*.json` at startup;
  `src/runtime/trigger_files.py::iter_trigger_file_stats` lists them for the sidebar.

### 6.7 `sessions/<sid>/launches/<launch_id>/`

- Writer: `charliebot schedule-trigger --cmd` (`src/runtime/cli/schedule_trigger.py`). It runs in the agent's
  process, not the server.
- `launch_id = <%Y%m%dT%H%M%S>-<6 hex>`.
- `metadata.json` is compact JSON plus `"\n"`: `{launch_id, session_id, host, pid, cwd, cmd, started_at (Z form),
  log, exit_file, trigger_id}`.
- `log` holds the command's combined output. `exit` is written by the wrapper
  `bash -lc "(<cmd>\n); echo $? > <exit file>"` (`src/runtime/cli/launcher.py`), so it holds the decimal exit
  code and a newline, and it appears only after the command ends.
- A remote launch (`--host`) keeps its files on the remote host in `/tmp/charliebot_runs/<launch_id>/{log,exit,pid}`.

### 6.8 `sessions/<sid>/plans.json`

- Writer: `src/features/artifacts/plans.py`, ATOMIC-JSON `indent=2`, under a per-session lock.
- Shape: `{"plans": [{id: int, title, versions: [{v: int, file, created_at, trigger: initial|auto_amend|feedback,
  base: {repo, branch, sha}|null, note}], takeoff: {v, at}|null, closed: {as: superseded|abandoned|completed,
  at}|null}]}`. `file` is a POSIX path relative to the session directory, normally `artifacts/<name>.html`.
- Fork: `src/features/artifacts/sidebar.py::_copy_plans_to_child` copies `plans.json` and every referenced file
  into the child and rewrites `file` paths relative to the child.
- Readers: plans API and CLI, and the sidebar probe (`has_pending_plan_approval`).

### 6.9 `sessions/<sid>/uploads/<filename>`

- Writer: `POST /api/sessions/<sid>/upload` (`src/runtime/api/chat.py`). The name is the client's file name
  basename (default `upload`), written in 1 MB chunks with `aiofiles` (PLAIN). A same-named upload overwrites the
  file.

### 6.10 `sessions/<sid>/voice/<stamp>_<8hex>.wav` and `.txt`

- Writer: `src/features/voice/api.py`. The wav is written to `.wav.partial`, then `os.replace`d; the transcript
  `.txt` is PLAIN.

### 6.11 Improve loops: `sessions/<sid>/loops/`

- `loops/active.lock`: PLAIN text `<loop_id>\n`. Its existence marks the session's single active loop; it is
  unlinked when the loop ends or its start is rejected. It is a marker file, not a `flock`.
- `loops/<n>/state.json`: `ImproveState` (ATOMIC) with fields `loop_id`, `goal`, `status`, `work_branch`,
  `base_branch`, `repo_path`, `merge_back`, `backend`, `model`, `backend_from_pool`, `created_at`, `server_pid`,
  `end_error`, `blocked_iteration`, `blocked_reason`.
- `loops/<n>/goal.md`, `plan.md`, `iter_<NNNN>.md`: the loop's goal, plan, and per-iteration reports. A runner
  without a report gets a fallback file that starts with `RUNNER_FALLBACK_REPORT_MARKER`.
- Writers: `src/features/improve/improve_command.py`, `improve_sequence.py`. Startup recovery marks interrupted
  loops through `sequence_controllers.reconcile_interrupted`.

### 6.12 Instruction files in the session directory

A manager turn writes its instructions into the session directory before each spawn
(`base.AgentBackend._prepare_cwd`, PLAIN): `CLAUDE.md` for cc-claude, cc-anthropic-compatible and cc-openai-compatible, and
`AGENTS.md` for codex, charlie-code and opencode. gemini and antigravity frame the instructions into the prompt
instead.

## 7. Files outside the home

| Path | Written by | What |
| --- | --- | --- |
| `/sys/fs/cgroup/user.slice/user-<uid>.slice/user@<uid>.service/charliebot.slice/charliebot-agents.slice/charliebot-sess-<sid[:8]>/` | `src/infra/process.py` | agent session cgroups; see `services.md` |
| `/sys/fs/cgroup/user.slice/user-<uid>.slice/user@<uid>.service/app.slice/charliebot-sess-<sid[:8]>/` | `src/infra/process.py` | retired session cgroups that the startup sweep may remove; see `services.md` |
| `<paths.scratch_dir>/<top-level entry>` | `src/features/disk_sweep/sweep.py` | eligible scratch directories are removed after 14 quiet days; paths in use are skipped |
| `<login dir>/.credentials.json` (`/home/<user>/.claude` or each `accounts.claude[].config_dir`) | `src/backends/claude_code/usage_quota.py` (from the `ext_usage` service) | rewritten ATOMIC-JSON `indent=2, private=True` when an OAuth token refresh succeeds |
| `<login dir>/projects/<cwd slug>/<cc_session_id>.jsonl` and sidecar `<cc_session_id>/` | `src/backends/claude_code/claude_accounts.py::move_transcript`, `retire_transcript_copies` | copies a Claude transcript between pool logins (staged copy, then `os.replace`), and later deletes all but the newest 2 copies |
| `<login dir>/projects/...`, `/home/<user>/.codex/sessions/...` | `usage_sweep.py` of the claude and codex backends, through `storage cool` | deletes backend transcripts of cold or orphaned sessions |
| `<paths.worktree_dir>/<branch with / replaced by ->` | `src/runtime/task_execution.py`, `src/infra/git.py` | `git worktree add` for worker runs (branch default `charliebot/task-<epoch>-<run id[:8]>`); `git worktree remove --force` and `prune` |
| `<paths.worktree_dir>/.trash/` | none in the baseline | listed and purged only by `charliebot gc-trash --yes`; the docstring's "startup sweep" that moves worktrees here has no code |
| `<home name>_backup/charliebot-<%Y%m%d-%H%M%S>.tar.gz` (a sibling of the home) | `src/features/backup/backup.py` (cron handler `backup`) | see 7.1 |
| `<publish.dir>/<token>/<basename>` | `charliebot publish` | see `cli.md` |
| `/tmp/charliebot-oneshot-*`, `/tmp/charliebot-run-*`, `/tmp/charliebot-opencode-oneshot-*` | one-shot backends | temp working and transport directories, removed after the call |
| `/tmp/charliebot-explain-*/` | `src/features/explain/explain.py` | the 0444 copy of the chat log that a one-shot reads; removed after the call |
| `/tmp/headless-render-*/` | `src/features/artifacts/headless_render.py` | Chromium user-data directory of the headless height probe; removed when the renderer closes |
| `/tmp/merge-chunks-*/`, `/tmp/merge-members-*/` | `src/features/trace/trace_merge.py` | fragment directories of a Perfetto merge; removed after the merge |
| `<system tmp>/<TemporaryDirectory>` | `src/features/memory/memory_proposal.py` | a `git archive` extraction of one commit, linted and removed |
| `/home/<user>/.ssh/controlmasters/` | `src/infra/ssh.py::_control_dir` (every `ssh_cmd` argv) | ssh ControlMaster sockets named `%C`, with `ControlPersist=1200`. The directory is created 0700; ssh creates each socket 0600. Used by host_auth probes, trigger probes and remote launches. |
| `.page-height-probe-<hex>.html` beside an artifact | `src/features/artifacts/artifact_check.py` | temporary probe page for the headless height check |
| tmux socket `charliebot` (default tmux socket directory) | `src/runtime/agent_process/pty_common.py` | terminal sessions `charliebot-<id>` |

### 7.1 Backup archive

- `create_backup` writes a tar stream through isal gzip level 1. It excludes, by path part: `.git`, `.claude`,
  `credentials.yaml`, `__pycache__`, and `*.pyc`. It also excludes the top-level `opencode/` and
  `sessions/*/threads`.
- Retention (`apply_retention`, `backup.py:122`): keep every archive younger than 3 days (`_DAILY_THRESHOLD`,
  `backup.py:42`), Sundays from 3 to 15 days (`_WEEKLY_THRESHOLD`, `backup.py:43`), and delete the rest. No
  monthly tier remains.
- Archive names use local time (`datetime.now()`), not UTC.

## 8. Reader-only inputs from the repo

- `configs/config.example.yaml`, `configs/credentials.example.yaml`: seeds (5.1).
- `configs/cron.default.yaml`: setup-step seed for `config.d/cron.d/` (`memory-curator` and `storage-cool`).
- `configs/code-server.yaml`: default `code_server.config`; its `bind-addr` gives the code-server port.
- `prompts/`: prompt sources, read fresh per launch (out of scope here).
