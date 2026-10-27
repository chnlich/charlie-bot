# charliebot CLI

The Rust `charliebot` binary: the dispatcher over the 14 caller-bearing subcommands plus the
runtime-owned ones (`src/commands/<name>.rs`, one file per subcommand so a later port task
replaces one file). The shared client contract — base URL, auth headers, the retry/readback
split, JSON output, session resolution — lives in `charliebot-cli-common`
(`rust/crates/infra/cli-common`). The Python CLI stays the installed `charliebot` until the
switch task lands.

## Golden cases

`tests/golden/<subcommand>/<case>.json` records one invocation: `argv`, `env`, `cwd`, `stdin` (piped
to the process when present, otherwise stdin is null), `files` (written under a temp root before the
run; files under `bin/` and entries in `executables` are written executable), `responses` (the stub
server's scripted answers: `status` + `body`, or `refuse_connect`, or `drop_after_request`), and
`expect` (`requests`, `stdout`, `stderr`, `exit`, `files`). `{root}`, `{port}` and `{repo}` in case
strings become the temp root, stub port and canonical checkout root; `{root}` also substitutes in
scripted responses. A file content of the form `{file:<checkout-relative path>}` reads that file from
the checkout, so a case can use a shipped prompt or genre template. Recorded requests replace the
temp root with `{root}`. `{uuid4}` and `{timestamp}` in `expect` match values of those shapes. Cases
with `parser_generated: true` compare the exit code only. Cases with `expect_from: "rust"` record from
the Rust binary when its task contract differs from Python's output; the golden harness filters the
Python backend-pool console lines, and those cases also cover the exception-class prefix difference.
A stub agent CLI runs from the case's temp `bin/` directory, so no test starts a real one.

Each CLI process runs with a cleared environment, a temp `CHARLIEBOT_HOME` and a 20 s timeout; the
stub server binds `127.0.0.1` port 0, so no case can reach the live server or home.

### Replay (the default)

From `rust/`:

```sh
cargo-slot.sh test -p charliebot-cli --test golden
```

### Record

Record mode runs the Python CLI instead and rewrites each case's `expect` fields. It needs the
worktree's Python environment on the cargo host (`run.sh uv sync` from the worktree root creates
`.venv` there); the record command is resolved against the checkout root:

```sh
cargo-slot.sh --config 'env.CHARLIEBOT_CLI_GOLDEN_RECORD=".venv/bin/charliebot"' test -p charliebot-cli --test golden
```

Re-record every case after a change and commit the rewritten files; `git status` stays clean when
the recorded behavior is unchanged.
