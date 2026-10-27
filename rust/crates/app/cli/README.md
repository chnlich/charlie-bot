# charliebot CLI

The Rust `charliebot` binary: the dispatcher over the caller-bearing subcommands plus the
runtime-owned ones (`src/commands/<name>.rs`, one file per subcommand so a later port task
replaces one file). The shared client contract — base URL, auth headers, the retry/readback
split, JSON output, session resolution — lives in `charliebot-cli-common`
(`rust/crates/infra/cli-common`). This is the only CLI codebase; the Python CLI it replaced is
gone.

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
with `parser_generated: true` compare the exit code only. The golden harness filters the Python
backend-pool console lines the cases were recorded with. A stub agent CLI runs from the case's temp
`bin/` directory, so no test starts a real one.

Each CLI process runs with a cleared environment, a temp `CHARLIEBOT_HOME` and a 20 s timeout; the
stub server binds `127.0.0.1` port 0, so no case can reach the live server or home.

### Replay

From `rust/`:

```sh
cargo-slot.sh test -p charliebot-cli --test golden
```

The cases replay against the built binary; the Python CLI that recorded them is deleted, so the
expectations stay as committed history.
