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
3. Starts the baseline (always the Python server) and the candidate, waits until both answer, and waits until
   both trees are still. It compares what each server's startup wrote as the `(startup)` step.
4. Sends each step's request to both servers and compares the responses. Once both trees are still, it
   compares what the step added, removed or changed in each tree.
5. Stops both servers with SIGTERM and compares what shutdown wrote as the `(shutdown)` step.
6. Reads every process's syscall trace and lists each file changed outside its own tree.

The modules:

| Module | Role |
|---|---|
| `parity.py` | entry point: the run loop, the requests, the report |
| `fixture.py` | the deterministic synthetic home and its manifest |
| `servers.py` | the trees, the synthetic config, the process environment, starting and stopping servers |
| `sandbox.py` | the Landlock launcher that confines every process under test to its own tree |
| `guard.py` | the outside-write guard: parses the strace logs |
| `compare.py` | tree snapshots and deltas, response and file comparison, the structural JSON diff |
| `normalize.py` | every normalization rule, each with the field it masks and why the value cannot match |
| `scenario_file.py` | the scenario format: loading, validation, references, route templates |
| `scenarios/*.yaml` | the scenarios, each tagged with its port stage |

## Running

From the worktree root (the first `uv run` builds the worktree `.venv` the servers run from):

```sh
uv run python rust/parity/parity.py --python-vs-python
uv run python rust/parity/parity.py --rust-bin rust/target/release/<server-binary>
uv run python rust/parity/parity.py --python-vs-python --stage server-skeleton --stage session-read-api
```

- `--python-vs-python` runs the Python server as the candidate too. Every scenario must pass in this mode. A
  failure here is a harness bug or a missing normalization rule, never a port gap.
- `--rust-bin PATH` runs PATH as the candidate, with the same environment as the Python server plus
  `CHARLIEBOT_REPO_ROOT` naming the worktree. The binary reads its home from `CHARLIEBOT_HOME`.
- `--stage NAME` (repeatable) runs only the scenarios of that stage. An unknown stage is an error.
- `--tamper` renames one session in the candidate home after the fixture check. The run must fail; use it to
  see the harness catch a difference.
- `--keep` keeps the trees and logs. A run that fails keeps them anyway and prints their path.

Needs `/usr/bin/strace` and a kernel with Landlock (Linux 5.13 or newer, enabled in the LSM list).

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
difference lines. `rust/parity/out/last-report.txt` holds the full report, and a kept run's `logs/` directory
holds each server's log and strace file and both raw responses of every differing step.

## Isolation

Each process under test (the fixture generator and each server) runs with:

- `HOME` set to its tree, `CHARLIEBOT_HOME` to `<tree>/home` and `TMPDIR` to `<tree>/tmp`;
- `PATH` set to `<tree>/fake-bin:/usr/bin:/bin`. `fake-bin` holds stand-ins named `claude`, `codex`,
  `opencode`, `charlie-code`, `gemini` and `agy`; each prints one line and exits 1, so no agent CLI ever starts;
- every proxy variable pointed at a closed loopback port, so no request leaves the host;
- an environment built from scratch, with nothing inherited from the harness;
- `PYTHONDONTWRITEBYTECODE=1`, because bytecode caches would land in the worktree;
- the synthetic config: `127.0.0.1`, a free port in 18600-18699, the fixed test access key, one backend option,
  no integration, the session cgroup limits and the disk-headroom check off, and both path settings inside the
  tree.

The Python server starts as `<worktree>/.venv/bin/python server.py` from the worktree root.

### The outside-write guard

Every process under test runs as `strace -f -y ... -- python sandbox.py --allow-write <tree> -- <argv>`.

- `sandbox.py` applies a Landlock ruleset before it execs the command. The ruleset refuses with EACCES any
  write, create, remove or rename outside the tree (except a few device nodes such as `/dev/null`). It binds
  every child process too.
- `guard.py` reads the strace log. It resolves each file-changing syscall to an absolute path: the descriptor
  path that `-y` prints, a directory descriptor plus a relative path, or the thread's working directory, which
  it tracks through `chdir`, `fchdir` and every clone, fork and vfork.
- A change outside the tree that completed is a violation and fails the run.
- A change the sandbox refused changed nothing. The report lists it as refused. The Python server's startup
  sweep of empty `charliebot-sess-*` cgroups under `/sys/fs/cgroup/user.slice/user-<uid>.slice/.../app.slice`
  shows up there on every run.

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

Step fields:

- `kind`: `http`, the only kind so far. `ws`, `wait` and `handover` are reserved for later stages; a scenario
  that uses one fails to load until a stage implements it.
- `name`, `method` and `path` are required. `path` carries no query string.
- `query` and `headers` map names to scalar values.
- `json` is a request body, sent as `application/json`.
- `auth` is `bearer` (the default), `cookie` or `none`.
- `expect_status` (optional) is the status both servers must return.
- `capture` maps a name to a dotted field of each server's own JSON response. A later step references the value
  as `${capture.name}`, so a cursor a server issued goes back to that server.

Strings reference the manifest as `${sessions.atlas}`, `${runs.notes_r2.run}`, `${runs.notes_r2.session}`,
`${inputs.atlas_u1}`, `${missing_session}` or `${missing_run}`. A step's route template is its method and path
with each reference replaced by a placeholder (`{session_id}`, `{run_id}`, `{input_id}`, `{<capture name>}`).
The report groups steps by template.

To add a scenario, add a YAML file under `scenarios/` with a stage tag and run it in `--python-vs-python`
mode. The new steps must pass there before they mean anything against a candidate. A fixture kind the scenario
needs goes into `fixture.py`, built through a server writer, with its id recorded in the manifest.

## What is compared

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

## Normalization rules

`normalize.py` holds every rule. Each one replaces a value with a named placeholder and keeps the field, so a
field one side omits still shows as a difference:

- the tree path rule rewrites each tree root to `<tree>` in every string, header value and file;
- `FieldRule` masks one JSON field of one route's body (`RESPONSE_RULES`) or of one home file (`FILE_RULES`).
  Its `field` is a dotted path where `[*]` steps into every list element; `when` restricts it to objects that
  hold given values;
- `TextRule` masks the `value` group of a pattern in one route's non-JSON body (`RESPONSE_TEXT_RULES`);
- `IGNORED_HEADERS` names the headers left out, each with its reason.

To add a rule, append it to the matching tuple with a `reason` that names why no implementation can produce
the same value: wall-clock time, a random id, a pid, a duration, a per-server path or port. A value that
differs because of behavior is a parity difference, not a rule. Prove the rule in `--python-vs-python` mode:
the step must fail without the rule and pass with it.
