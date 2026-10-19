# The Python helper child

`charliebot_core::python_helper` runs one Python helper script and reads back one JSON value. A Rust route uses it for
work only Python can do. Today the ncu viewer uses it to read `.ncu-rep` reports through NVIDIA's `ncu_report` module,
which ships only for Python. The voice `qwen3_hf` engine will use the same module later.

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
- **Timeout.** `Option<Duration>`. `None` waits as long as the helper runs. `Some` kills a helper that runs past it.

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

Nothing retries and nothing falls back. A caller decides what the error means for its request.

## Protocol for a script that reports a status

A helper that can fail in a way the caller handles prints a JSON object with a `status` field, and the caller matches
on it. The ncu helper prints one of:

- `{"status": "ok", "report": {...}}`, the dict `_parse_with_module` returns;
- `{"status": "module_unavailable", "python_dir": "..." or null}`, when `import ncu_report` failed;
- `{"status": "parse_error", "error": "..."}`, the `NcuParseError` text.

Any other exception is a crash: it exits nonzero, and the caller reports `Exit`.

## The capped child runner

`python_helper::capture(child, stdin, timeout)` waits for an already spawned child and collects its piped stdout and
stderr. It is the same bounded wait the ncu CSV import uses for its 120 s limit. A child past its timeout is killed and
reported as `Capture::TimedOut`.

## Tests

`charliebot-core`'s `python_helper` tests start the checkout's `.venv` interpreter on scripts written to a temporary
directory. They cover one JSON value with the working directory, `PYTHONPATH` and the environment override, stdin, a
nonzero exit with stderr, stdout that is not JSON, a missing interpreter, an interpreter that cannot start, and a timeout.
The checkout's `.venv` must exist, so run them through `run.sh uv run …` first.
