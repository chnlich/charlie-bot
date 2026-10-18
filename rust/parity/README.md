# Parity harness

The parity harness checks a candidate server against the Python server by running both as real processes on
two identical synthetic homes. It replays scripted requests against both and reports every difference in the
normalized responses and in the files each request changed. A route counts as ported when its scenarios pass.

## Shape

One run walks the selected scenarios. For each scenario it:

1. Lays out two temporary trees under `/tmp/charliebot-parity-*/`, one per server (`base/` for the baseline,
   `cand/` for the candidate), and writes a synthetic `config.yaml` and `credentials.yaml` into each home.
2. Runs `fixture.py` in each tree. It builds the same synthetic home through the Python server's own writers.
   The harness then checks that both homes match file for file.
3. Writes the scenario's fake-agent script into both trees before either server starts.
4. Starts one declared fake-service set per side, then starts the baseline Python server and the candidate. A
   handover scenario starts its candidate in the named
   direction and starts the next server after the handover step.
5. Sends HTTP and WebSocket steps to both servers. Fake steps push provider events to both fake services. Wait
   steps observe durable state, and gate steps release fake agents. Handover steps stop and restart servers on
   the same homes.
6. Stops each final server, kills any fake agent that remains alive, stops each fake set, and compares the home
   deltas and fake recordings.
7. Reads every server trace, including each second server, and lists file changes outside its tree.

The modules:

| Module | Role |
|---|---|
| `parity.py` | entry point: the run loop, the requests, the report |
| `fixture.py` | the deterministic synthetic home and its manifest |
| `servers.py` | the trees, the synthetic config, the process environment, starting and stopping servers |
| `fake_agent.py` | scripted fake agents, spawn records and gate markers |
| `sandbox.py` | the Landlock launcher: confines each process under test to its tree and to its TCP connect ports |
| `guard.py` | the outside-write guard: parses the strace logs |
| `compare.py` | tree snapshots and deltas, response and file comparison, the structural JSON diff |
| `normalize.py` | every normalization rule, each with the field it masks and why the value cannot match |
| `scenario_file.py` | the scenario format: loading, validation, references, route templates |
| `scenarios/*.yaml` | the scenarios, each tagged with its port stage |
| `fakes/__init__.py` | readiness, lifecycle, inbound pushes and refused-host reporting |
| `fakes/authority.py` | run-local certificate authority and fake-host leaf certificate |
| `fakes/proxy.py` | loopback-only HTTPS CONNECT proxy, TLS termination, and request parsing |
| `fakes/recorder.py` | each side's append-only fake-service traffic log |
| `fakes/slack.py` | Slack Web API and Socket Mode fake |
| `fakes/discord.py` | Discord REST and gateway fake |
| `fakes/gemini.py` | Gemini Live transcription fake |
| `fakes/muse.py` | Muse realtime transcription fake |
| `fixture_extras/discord_thread.py` | synthetic Discord-origin session for thread reply scenarios |
| `fixture_extras/sample_files.py` | workspace file for the file-route scenario |
| `fixture_extras/trace_files.py` | small Chrome traces for `features-harness-trace-merge` |
| `data/generate_silence.py` | generator for the one-second synthetic voice audio |
| `data/audio/` | generated WAV, PCM and invalid-WAV request bodies |

## Running

From the worktree root (the first `uv run` builds the worktree `.venv` the servers run from):

```sh
uv run python rust/parity/parity.py --python-vs-python
uv run python rust/parity/parity.py --rust-bin rust/target/release/<server-binary>
uv run python rust/parity/parity.py --python-vs-python --stage server-skeleton --stage session-read-api
uv run python rust/parity/parity.py --python-vs-python --stage features
```

- `--python-vs-python` runs the Python server as the candidate too. Every scenario must pass in this mode. A
  failure here is a harness bug or a missing normalization rule, never a port gap.
- A handover scenario runs once per `directions` value against a candidate. Python-to-Python mode runs it once.
- `--rust-bin PATH` runs PATH as the candidate, with the same environment as the Python server plus
  `CHARLIEBOT_REPO_ROOT` naming the worktree and `CHARLIEBOT_VENV_BIN` naming its venv bin directory. The binary
  reads its home from `CHARLIEBOT_HOME`.
- `--stage NAME` (repeatable) runs only the scenarios of that stage. An unknown stage is an error.
- `--tamper` renames one session in the candidate home after the fixture check. The run must fail; use it to
  see the harness catch a difference.
- `--voice-models DIR` selects the read-only model source for scenarios with `voice_models: true`. Its default is
  `~/.charliebot/models`. Before starting any server, the harness checks for the VAD file, sherpa archive and
  extracted model directory, then links those entries into each synthetic home.
- `--keep` keeps the trees and logs. A run that fails keeps them anyway and prints their path. Each run's directory
  is named `NN-RR` (scenario index, run number), so the socket path under its tree stays within 107 bytes;
  `runs.json` in the scratch root maps each name to its scenario, direction and the two tree paths.

Needs `/usr/bin/strace` and a kernel with Landlock ABI 4 or newer (Linux 6.7 or newer, enabled in the LSM list).

### Reading the report

Each step prints one line: verdict, route template, the two statuses (baseline/candidate), the step name, and
the baseline's file changes when the step made any (`files +added -removed ~changed`). A differing step lists
its differences under the line:

```
FAIL  GET /api/sessions/{session_id}                             200/200  detail, manager root, starred, grouped
      body $.name: "Atlas planning" != "Atlas planning (tampered)"
```

A summary per route template follows, then the outside-write guard, then the verdict. The exit code is 0 when
every step matches and the guard found no outside change, and 1 otherwise. The console caps each step at 25
difference lines. `rust/parity/out/last-report.txt` holds the full report. Each tree's `logs/` directory holds
that side's process log and strace file. A kept run's `logs/` directory holds both raw responses of every
differing step.

## Isolation

Each process under test (the fixture generator and each server) runs with:

- `HOME` set to its tree, `CHARLIEBOT_HOME` to `<tree>/home` and `TMPDIR` to `<tree>/tmp`;
- `PATH` set to `<tree>/fake-bin:/usr/bin:/bin`. Each fake-bin wrapper starts `fake_agent.py` with
  `/usr/bin/python3 -I` for `claude`, `codex`, `opencode`, `charlie-code`, `gemini` or `agy`;
- every proxy variable pointed at a closed loopback port, except that a scenario declaring `fakes` sends HTTPS
  requests through its own loopback-only CONNECT proxy. That proxy terminates TLS for declared fake hosts and
  refuses every other CONNECT target with 403; it has no outbound connection path. The server trusts the
  run-local CA only in that scenario;
- an environment built from scratch, with nothing inherited from the harness;
- `PYTHONDONTWRITEBYTECODE=1`, because bytecode caches would land in the worktree;
- the synthetic config: `127.0.0.1`, a port in 18600-18699 that the run reserves, the fixed test access key, one
  backend option, no integration, the session cgroup limits and the disk-headroom check off, and both path settings
  inside the tree.
- `TMUX_TMPDIR` is the tree's `tmp/` directory, and the harness kills the `charliebot` tmux server when a server
  stops or is handed over. Tree creation checks that the resulting socket path is at most 107 bytes.

### The TCP connect rule

`sandbox.py` also limits the TCP connects of every process under test to port 9 (the closed proxy,
`servers.CLOSED_PROXY`) and to ports 1024-65535. A connect to any other port fails with EACCES, which Python raises
as `PermissionError`. Port 22, 80 and 443 are refused, so a client that ignores the proxy variables (an ssh child, for
example) cannot reach a real service on its standard port. Bind stays unrestricted. The rule needs Landlock ABI 4; on
an older kernel `sandbox.py` exits with an error naming the ABI, with no fallback. Landlock has no port ranges, so the
ruleset holds 64,513 port rules. Building it took 0.18-0.24 s per launch in five isolated runs on this host. Across
twelve runs under a load average of 4-7, the median was 0.27 s (kernel 7.0, Landlock ABI 8).

What stays open:

- The destination address. Landlock matches the port only, so a connect to any host on a port of 1024 or more (8443,
  for example) still succeeds. Closing that takes another mechanism, such as a network namespace.
- UDP, so DNS lookups still reach the resolver.
- Unix sockets, which carry the tmux server and other local sockets.

The Python server starts as `<worktree>/.venv/bin/python server.py` from the worktree root. Every server receives
`CHARLIEBOT_VENV_BIN=<worktree>/.venv/bin`.

### Ports

Concurrent runs share the 18600-18699 range, so each run reserves its ports before it starts a server. `pick_ports`
takes a non-blocking exclusive `flock` on `/tmp/charliebot-parity-ports/<port>.lock` for each port, from a random
start in the range, and skips a port whose lock is held or that does not bind. The run holds both locks until both
servers have stopped. A kill of the harness releases them, because the kernel drops a process's flocks at exit.

Readiness needs more than an answer on the port. `wait_ready` accepts a server only when the listening socket on its
port belongs to the server's process tree (strace, the sandbox launcher, the server and its children). A listener from
any other process fails the step at once, naming the port and the owner pid. The `server startup` step lists each
server's verified owner pid, and a handover step lists the new ones.

A server that exits before the harness stops it fails the step that was running, with its exit status and log path.
The harness checks for this after every step, before a handover stops a server, and before the shutdown step.

Without `fake-agent/script.json`, the fake prints the stand-in line and exits 1. It reads stdin to EOF before
the first scripted step unless stdin is a TTY. A `flock` on `fake-agent/lock` assigns each start the next script
entry and appends one record to `fake-agent/spawns.jsonl`; every start writes `exit-<index>` before it exits.
The record includes argv, cwd, pid and `/proc` start time, stdout target, stdin byte count and digest, nice value,
session ownership, parent-death signal, selected environment, and decoded run-token claims with signature validity.
The scenario end checks that no recorded process remains alive.

### The outside-write guard

Every process under test runs as `strace -f -y ... -- python sandbox.py --allow-write <tree> -- <argv>`.

- `sandbox.py` applies a Landlock ruleset before it execs the command. The ruleset refuses with EACCES any
  write, create, remove or rename outside the tree (except a few device nodes such as `/dev/null`). It binds
  every child process too.
- `guard.py` reads the strace log. It resolves each file-changing syscall to an absolute path: the descriptor
  path that `-y` prints, a directory descriptor plus a relative path, or the thread's working directory, which
  it tracks through `chdir`, `fchdir` and every clone, fork and vfork.
- A change outside the tree that completed is a violation and fails the run.
- A call strace prints as `???` (its syscall number was unreadable) names no path. The guard counts it as undecodable
  and prints the count with each trace file path on the guard line. It is not a violation: the sandbox refuses every
  write outside the tree, so such a call cannot have changed a file there.
- A change the sandbox refused changed nothing. The report lists it as refused. The Python startup sweep targets
  empty `charliebot-sess-*` cgroups for the current uid. Landlock refuses the sweep inside the harness.
- Outside the harness, that sweep removes every empty `charliebot-sess-*` cgroup for the uid, including cgroups
  of a live server. The live server recreates them at its next launch.

Why a syscall trace, not a filesystem scan: the host is shared. A before-and-after scan of `HOME` or `/tmp` sees
every other process's writes (a live server, other worktrees, editors) and cannot tell them apart. inotify cannot
watch a whole filesystem, and fanotify needs CAP_SYS_ADMIN. The trace attributes each change to the process
tree under test and also records refused attempts. The sandbox makes outside writes fail, so a gap in the
parser cannot become damage. Landlock leaves metadata calls (chmod, chown, utimes, xattrs) open; the trace
covers them. A write submitted through io_uring would escape the trace; neither server uses io_uring.

## The fixture

`fixture.py` builds one home through the Python server's writers: the task-tree owner, the run store, the input
dispatcher, the completion owner, the session lifecycle, the fork, the plan registry and the trigger store. It
covers:

| Key | Kind |
|---|---|
| `atlas` | manager root, starred, grouped; uploads, every chat event type, acknowledged and consumed inputs |
| `atlas_survey` | manager child with consumed child reports |
| `atlas_notes` | worker: failed run, retry, successful run, closed |
| `atlas_probe` | worker: interrupted run, failure report, archived |
| `atlas_scout` | worker whose queued run was stopped |
| `beacon`, `beacon_draft` | archived root (starred) and its archived child |
| `cobalt` | root pinned to a retired backend, with a pending input, unread |
| `delta` | chat-thread session (Slack origin) |
| `ember` | recycled history (archive offset > 0), archived then restored |
| `fable` | approved and closed plans; pending, fired and cancelled triggers; unread |
| `garnet` | fork of `atlas` |

Two runs produce byte-identical homes up to the tree path. uuid4 draws from a seeded generator. The model clock
steps from a fixed epoch. The last step pins every file time in the home to the clock's final instant, because
the server derives ETags and memo revisions from file times. The home is a fixed point of the server's
startup recovery: the generator runs the real reconcile pass at the end and fails if that pass would write or
launch anything.

The generator writes `fixture-manifest.json` at the tree root: the ids of `sessions`, `runs` (each with its
`session` and `run` id), `inputs`, plus `missing_session` and `missing_run`, two ids no fixture entry has.
Scenarios reference these ids.

`fixture.py --extras NAME[,NAME...]` runs each extra after the base fixture's final write and before coverage,
recovery and time pinning. Extras use the server writers for server-format files and plain tree writes for files
with no server writer. When extras run, the fixture also pins tree entries outside the home except harness-owned
entries. With no extras, the base fixture layout and manifest stay unchanged.

## Scenarios

A scenario is a YAML file under `scenarios/`:

```yaml
stage: session-read-api        # the port stage the routes belong to; --stage selects on it
name: session-runs             # names the scenario in the report
steps:
  - name: runs, first page of one, manager root
    method: GET
    path: /api/sessions/${sessions.atlas}/runs
    query: {limit: 1}
    capture: {cursor: next_cursor}
    expect_status: 200
  - name: runs, second page of one, manager root
    method: GET
    path: /api/sessions/${sessions.atlas}/runs
    query: {limit: 1, cursor: "${capture.cursor}"}
    expect_status: 200
```

Scenario fields:

- `fake_agent` gives the starts each fake agent consumes in order. `expect_starts` gives the required record count
  in each tree at scenario end.
- `directions` names a non-empty subset of `python-to-candidate` and `candidate-to-python`. Add it when a scenario
  has a `handover` step. A scenario without handover refuses this key.
- `config` deep-merges into the synthetic `config.yaml`; nested mappings merge, while lists and scalar values
  replace the earlier value. `credentials` deep-merges into `credentials.yaml`; the harness refuses a
  `charliebot` key so the access key stays fixed. Strings in either overlay may use `${tree.root}` and
  `${tree.home}`.
- `fakes` maps `slack`, `discord`, `gemini` or `muse` to that fake's seed mapping. Empty seeds select the
  documented synthetic defaults. Seed keys are validated by each module: Slack accepts `channel_name`,
  `thread_messages` and `failures`; Discord accepts `bot_user_id`, `application_flags`, `guilds`, `channels`,
  `messages` and `failures`; Gemini and Muse accept `transcript` and `handshake_error`.
- `fixture_extras` is an ordered list of module names under `fixture_extras/`. Each module's `async build(b)`
  returns tree-relative values recorded at `extras.<name>` in the manifest. Sessions created by an extra use
  `Builder.node` and appear under `sessions`.
- `voice_models: true` links the three sherpa entries from `--voice-models` into `<tree>/home/models/`. The
  source directory is read only.

Each fake-agent start names its executable, lists its steps and gives its exit code. A start without a matching
entry prints the stand-in line and records `scripted: false`.

```yaml
fake_agent:
  starts:
    - name: claude
      steps:
        - emit: {type: system, subtype: init, session_id: synthetic-session}
        - emit:
            type: assistant
            message: {role: assistant, model: claude-sonnet-5, content: [{type: text, text: Done.}]}
        - gate: manager
        - emit: {type: result, result: "", usage: {input_tokens: 1, output_tokens: 1}}
      exit_code: 0
expect_starts: 1
directions: [python-to-candidate, candidate-to-python]
```

Each `emit` writes one JSON line to stdout and flushes it. A string equal to `${argv:<flag>}` reads the value
after `<flag>`. A missing flag writes an error to stderr and exits 2. A `gate` writes `at-<name>` and polls for
`open-<name>` every 20 ms. Each start records its argv, cwd, pid, process start time, stdio, environment and
verified run-token claims.

Step fields:

- `kind`: `http` (the default), `ws`, `fake`, `wait`, `gate` or `handover`.
- `name` is required for every step. HTTP steps also require `method` and `path`. `path` carries no query string.
- `query` and `headers` map names to scalar values.
- `json` is a request body, sent as `application/json`.
- `body_file` sends a raw file from `data/`. `multipart` maps form fields to scalar values or to
  `{file: PATH, content_type: TYPE}` entries. Both sides use the same fixed multipart boundary. A step may
  contain at most one of `json`, `body_file` and `multipart`.
- `auth` is `bearer` (the default), `cookie` or `none`.
- `expect_status` (optional) is the status both servers must return.
- `capture` maps a name to a dotted field of each server's own JSON response. A later step references the value
  as `${capture.name}`, so a cursor a server issued goes back to that server.
- `defer_files: true` skips this HTTP step's file comparison. The next step compares changes since before the request.
- `wait` requires `timeout_seconds` and one `until` condition. Both trees must meet it before the timeout.
- `until.event` matches chat events by session, type and fields. `count` requires that many matches and defaults to 1.
- `until.file_field` reads a dotted field from a home-relative JSON file and compares it with `equals`.
- `until.at_gate` waits for the gate marker and for `agent.raw.cursor` to equal the raw stdout file size.
- `gate` requires `open`; the harness writes `open-<name>` in both fake-agent directories.
- `handover` requires `agent_after_stop`: `alive`, `exited` or `terminated`. It checks every scripted start that
  had no exit marker before the server stopped.
- `ws` targets any `/ws/...` path. It accepts `query`, `auth` (`token` or `none`), an ordered `send` list of
  `{json: ...}`, `{text: ...}` or `{binary_file: PATH}` frames, an optional `collect` rule, a required `until`
  condition and a required `timeout_seconds`. `wait_for_fake_handshake: FAKE` holds outgoing frames until the proxy
  records a new upstream WebSocket handshake with that declared fake. `collect` selects JSON frames by exact `where`
  fields, reads a dotted `field`, and concatenates it as text or decoded base64. `until` is `{frames: N}`, `{close: true}` or
  `{collected_contains: TEXT}`. The default `compare: frames` compares received frames in order; JSON frames
  compare structurally. `compare: {collected_contains: [TEXT, ...]}` checks each string in both collected results
  for endpoints whose frame timing varies. `expect_reject: STATUS` expects an HTTP rejection of the upgrade and
  sends no frames. The route template is `WS <path>`.
- `fake` pushes one inbound event through a declared provider. `slack/socket_event` wraps `payload` in an
  `events_api` Socket Mode envelope. `discord/dispatch` sends an op 0 dispatch whose payload contains `t` and
  `d`. Both fake services must be ready on both sides before the step runs. Its route template is
  `FAKE <fake> <action>`.

The harness waits for each declared Slack fake to send `hello` and each Discord gateway fake to receive IDENTIFY
and send READY before the first step. Gemini and Muse become ready with their fake set. The proxy records HTTP
requests, WebSocket handshakes and client text or binary messages. HTTP requests compare as a multiset within a
step because listener and boot-backfill requests interleave; WebSocket messages compare in order within each
connection. Each recorded HTTP request and WebSocket handshake keeps only the headers that its fake's feature
code sets explicitly (`normalize.EXPLICIT_REQUEST_HEADERS`), so library defaults such as the `python-httpx` or
`websockets` user agent do not count as behavior. A request's `content-type` compares as its media type
(`normalize.MEDIA_TYPE_REQUEST_HEADERS`), so a form body sent where Python sends JSON is a difference. Discord op 1 heartbeats are excluded because the client's
first beat includes an unseeded random delay. Refused proxy hosts are written to each tree's `logs/fakes.jsonl` and listed in the report, outside step
comparison.

`features-harness-trace-merge` sends two synthetic traces through `/perfetto/merged`. Python's spawned process
pool creates POSIX semaphores under `/dev/shm/sem.*`; the sandbox permits the shared-memory directory for those
files, and the guard exempts only those semaphore paths. Other `/dev/shm` writes remain outside-write findings.

The baseline always hands over from Python to Python. The candidate starts the direction's first server and hands
over to the other server. A handover delta covers the stop and second startup.

Strings reference the manifest as `${sessions.atlas}`, `${runs.notes_r2.run}`, `${runs.notes_r2.session}`,
`${inputs.atlas_u1}`, `${missing_session}` or `${missing_run}`. A step's route template is its method and path
with each reference replaced by a placeholder (`{session_id}`, `{run_id}`, `{input_id}`, `{<capture name>}`).
Extras use `${extras.<name>.<key>}` and route placeholders `{<name>.<key>}`. `${tree.root}` and `${tree.home}`
resolve to each side's own paths and become `{tree}` and `{home}` in route templates. The report groups steps by
template.

To add a scenario, add a YAML file under `scenarios/` with a stage tag and run it in `--python-vs-python`
mode. The new steps must pass there before they mean anything against a candidate. A fixture kind the scenario
needs goes into `fixture.py`, built through a server writer, with its id recorded in the manifest.

## What is compared

Fake-agent starts compare structurally after normalization. `expect_starts` checks each tree's record count.

- Status code.
- Headers: all except `date`, `server`, `content-length`, `transfer-encoding`, `connection` and `keep-alive`
  (see `normalize.IGNORED_HEADERS` for each reason). A gzip `content-encoding` is compared, and the body is
  compared decoded.
- Body: JSON (`application/json` or `+json`) structurally, with key order free and value types strict. Every
  other body as bytes.
- Files: after each step, the set of paths each server added, removed and changed must match, and so must the
  new content of each. `.json`, `.jsonl` and `.yaml` files compare structurally; other files compare as bytes.
  A step's file changes count once both trees have been still for one quiet interval
  (`compare.SETTLE_QUIET_SECONDS`).
- Fake traffic compares after every step, including startup and shutdown. Each step compares HTTP requests as a
  multiset and compares each WebSocket connection's handshakes and messages in order. Refused proxy hosts stay
  in `logs/fakes.jsonl` and the report but are excluded from parity differences.

## Normalization rules

`normalize.py` holds every rule. Each one replaces a value with a named placeholder and keeps the field, so a
field one side omits still shows as a difference:

- the tree path rule rewrites each tree root to `<tree>` in every string, header value and file;
- the minted UUID rule leaves fixture UUIDs intact. Other UUIDs read as `<minted-N>` by first appearance in sorted
  paths and document order. It applies to paths, file contents, response bodies and spawn records;
- runtime field rules mask process ids and `/proc` start times, wall-clock timestamps in run, chat-event, session
  and trigger records, and measured `master_done` durations. Their reasons are recorded beside each rule in
  `normalize.py`;
- `FieldRule` masks one JSON field of one route's body (`RESPONSE_RULES`) or of one home file (`FILE_RULES`).
  Its `field` is a dotted path where `[*]` steps into every list element; `when` restricts it to objects that
  hold given values;
- `TextRule` masks the `value` group of a pattern in one route's non-JSON body (`RESPONSE_TEXT_RULES`);
- `IGNORED_HEADERS` names the headers left out, each with its reason.
- `RESPONSE_HEADER_RULES` replaces only the named header value on one route. The merged-trace ETag and
  `Last-Modified` come from a cache artifact created at each server's wall-clock time; their body bytes still
  compare. `MERGED_TRACE_CACHE_PATH_RULE` masks the SHA-256 key derived from each tree's absolute trace paths and
  file times, while the cache artifact bytes still compare.
- `EXPLICIT_REQUEST_HEADERS` lists the headers each fake's feature code sets on its REST requests (Slack and
  Discord `authorization`, Discord `user-agent`); WebSocket handshakes compare no header. The fake recorder
  keeps every header in `logs/fakes.jsonl`. `MEDIA_TYPE_REQUEST_HEADERS` names `content-type`, which compares as
  its media type: lowercased, without parameters. `IGNORED_REQUEST_HEADERS` names each other header that comes from
  a library or the transport, with its reason. A header that is neither listed fails the comparison.
- the voice archive stem rule masks `YYYY-MM-DDTHHMMSS.mmmZ_<8 hex>` in paths because the write clock and uuid4
  draw are independent on each side;
- the named Discord heartbeat rule omits only op 1 messages the gateway client sends, because its first interval
  includes an unseeded random delay;
- `WS_FRAME_RULES` selects any frame-specific normalization by WebSocket route template.

To add a rule, append it to the matching tuple with a `reason` that names why no implementation can produce
the same value: wall-clock time, a random id, a pid, a duration, a per-server path or port. A value that
differs because of behavior is a parity difference, not a rule. Prove the rule in `--python-vs-python` mode:
the step must fail without the rule and pass with it.
