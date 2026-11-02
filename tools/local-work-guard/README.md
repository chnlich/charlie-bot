# Local work guard

The server sets the variables in this table on every agent child process. Each variable points at a script in this
directory, and a refused run reaches the agent as the refusal text. The server's own environment, the web terminal and
the one-shot calls carry none of them. The variables are set in `rust/crates/runtime/agents/src/local_work_guard.rs`,
and the switch and the runner keys are `server.enable_local_work_guard`, `server.remote_run_host` and
`server.remote_run_remote_base` in `config.yaml`.

| Toolchain | Variables | Scripts |
|---|---|---|
| cargo, rustc | `RUSTUP_TOOLCHAIN` names a placeholder toolchain; `RUSTC_WRAPPER` refuses a compile that skips it, such as `cargo +stable` | `rustup/bin/cargo`, `rustup/bin/rustc`, `rustup/rustc-wrapper` |
| pytest, in every entry form | `PYTEST_PLUGINS` loads a refusing plugin | `python/refuse_local_tests.py` |
| unittest, run as a module | `PYTHONPATH` puts a refusing `sitecustomize` first | `python/sitecustomize.py` |
| node --test, npm test, npm run build | `NODE_OPTIONS` preloads a refusing module | `node/refuse-local-tests.cjs` |
| make | `MAKEFILES` names a refusing makefile | `make/refuse.mk` |
| go build, go test, go run | `GOFLAGS` sets `-toolexec` to a refusing program | `go/toolexec.sh` |
| cc, gcc, g++, c++ | `PATH` starts with `bin/`; `CC` and `CXX` name its scripts | `bin/cc` (the other three are links to it) |

Read-only and non-build commands run as usual: the cargo subcommands `fmt`, `metadata` and `tree`, and the
`--version` of each tool.

`refuse.sh` owns the refusal text. Each script calls it with the name of the tool that was run, then exits 97; pytest
exits 4 instead.

The list of refused toolchains is in `tools/remote-run/README.md`. Keep this table in step with that list.

The scripts in `python/` load into an interpreter by path, so they are Python modules. `tools/check-no-python.sh`
exempts this directory for that reason.
