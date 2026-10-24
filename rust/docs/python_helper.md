# The Python helper child

`charliebot_core::python_helper` runs Python helper scripts for work only Python can do. It has two modes: a run that
reads back one JSON value, and a long-lived helper that answers one JSON line per request. The ncu viewer runs the first
mode to read `.ncu-rep` reports through NVIDIA's `ncu_report` module, which ships only for Python.

## The contract

- **Interpreter.** `<repo>/.venv/bin/python`, where `<repo>` is the server's `RepoRoot`. It is the interpreter the
  Python server runs in. No configuration key names it. A missing file fails with `MissingInterpreter`.
- **Script.** The caller's path, relative to the repo root or absolute. The helper's first argument is the script's
  absolute path, and the caller's arguments follow.
- **Working directory.** The repo root.
- **`sys.path`.** `PYTHONPATH` is the repo root, so a helper imports the checkout's packages (`src.…`). An inherited
  `PYTHONPATH` is replaced. The script's own directory is `sys.path[0]`, as Python sets it for any script.
- **Environment.** The server's environment, plus the overrides the caller names with `PythonHelper::env`. The ncu
  viewer passes its `PATH` there, so the helper and the `ncu` binary see the same search path.
- **Stdin.** Null, unless the caller passes bytes. Then the bytes are written and the pipe closes.
- **Stdout.** One JSON value, parsed as a whole. Logs and tracebacks belong on stderr. A helper whose logger writes
  to stdout (structlog's default) must redirect `sys.stdout` before it logs, as `rust/helpers/ncu_report_helper.py`
  does.
- **Timeout.** `Option<Duration>`. `None` waits as long as the helper runs. `Some` kills a helper that runs past it, and
  the helper's process group with it.
- **Process group.** Each helper leads a process group of its own, so the group kill reaches the children the helper
  started. The children hold the pipes open, so without the group kill a timed-out run would wait for them.

## Errors

`HelperError` names the failure. Every variant that ran the helper carries its exit code and stderr, so the caller
shows the Python traceback next to the failure.

| Variant | Cause |
|---|---|
| `MissingInterpreter` | `<repo>/.venv/bin/python` does not exist |
| `Start` | the file exists but the process did not start: a missing working directory, or a file that is not executable |
| `Exit` | a nonzero exit status, or a signal (`code` is `None`) |
| `Output` | exit status zero, but stdout is not one JSON value |
| `TimedOut` | the timeout passed, and the helper was killed |
| `Io` | writing stdin or waiting on the child failed |
| `Unanswered` | a long-lived helper stopped answering; see the long-lived mode below |

Nothing retries and nothing falls back. A caller decides what the error means for its request.

## Protocol for a script that reports a status

A helper that can fail in a way the caller handles prints a JSON object with a `status` field, and the caller matches
on it. The ncu helper prints one of:

- `{"status": "ok", "report": {...}}`, the dict `_parse_with_module` returns;
- `{"status": "module_unavailable", "python_dir": "..." or null}`, when `import ncu_report` failed;
- `{"status": "parse_error", "error": "..."}`, the `NcuParseError` text.

Any other exception is a crash: it exits nonzero, and the caller reports `Exit`.

## The long-lived mode

`PythonHelper::start_long_lived(script, args, label)` starts a helper that stays up between requests. The interpreter,
working directory, `PYTHONPATH` and environment are those of the one-shot run above. The helper's protocol is JSON
lines:

- **Requests and replies.** `request` writes one JSON line to the helper's stdin and reads one line from its stdout.
  `receive` reads a line the helper prints without a request, such as the ready line that a helper prints once its model
  has loaded. The helper's own stdout must carry only these lines, so a helper redirects `sys.stdout` before it loads
  anything, as `rust/helpers/ncu_report_helper.py` does for its logs.
- **One request at a time.** The protocol lock is held from a request's write to its reply, so a second caller waits.
- **Stderr.** Each line goes to the server log as a `python_helper_stderr` event that names the helper in `helper`.
- **Retirement.** A helper that closes its stdout, writes a reply that is not one JSON value, or cannot take a request
  is retired: the child is reaped, the process group is killed, and the reason is recorded. The failing request returns
  `Unanswered` with that reason, and every later request returns the same error. Nothing restarts the helper.
- **Stop.** `stop` kills the process group and reaps the child. A request in flight then fails with the reason
  `the server stopped it`. Dropping the handle stops the helper.

## The capped child runner

`python_helper::capture(child, stdin, timeout)` waits for an already spawned child and collects its piped stdout and
stderr. It is the same bounded wait the ncu CSV import uses for its 120 s limit. A child past its timeout is reported as
`Capture::TimedOut`. When the child leads its own process group, as every `PythonHelper` helper does, the whole group is
killed; any other child is killed alone, as the ncu import's `ncu` process is.

## Tests

`charliebot-core`'s `python_helper` tests start the checkout's `.venv` interpreter on scripts written to a temporary
directory. They cover one JSON value with the working directory, `PYTHONPATH` and the environment override, stdin, a
nonzero exit with stderr, stdout that is not JSON, a missing interpreter, an interpreter that cannot start, and a timeout
that kills the helper's children. For the long-lived mode they cover one reply per request, requests that do not
interleave, an exit that fails the pending and every later request, a reply that is not JSON, and `stop`.
The checkout's `.venv` must exist, so run them through `run.sh uv run …` first.
