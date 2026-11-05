# Local work guard

The server sets the variables in this table on every agent child process. Each variable points at a script in this
directory, and a refused run reaches the agent as the refusal text. The server's own environment, the web terminal and
the one-shot calls carry none of them. The variables are set in `rust/crates/runtime/agents/src/local_work_guard.rs`,
and the switch is `server.enable_local_work_guard` in `config.yaml`. `refuse.sh` reads the build host from
`remote_run.host` in `config.yaml` at each refusal, so the refusal names the host that the runner uses.

| Toolchain | Variables | Scripts |
|---|---|---|
| cargo, rustc | `RUSTUP_TOOLCHAIN` names a placeholder toolchain; `RUSTC_WRAPPER` refuses a compile that skips it, such as `cargo +stable` | `rustup/bin/cargo`, `rustup/bin/rustc`, `rustup/rustc-wrapper` |
| pytest, in every entry form | `PYTEST_PLUGINS` loads a refusing plugin | `python/refuse_local_tests.py` |
| unittest, run as a module | `PYTHONPATH` puts a refusing `sitecustomize` first | `python/sitecustomize.py` |
| node --test, npm test, npm run build | `NODE_OPTIONS` preloads a refusing module | `node/refuse-local-tests.cjs` |
| make | `MAKEFILES` names a refusing makefile | `make/refuse.mk` |
| go build, go test, go run | `GOFLAGS` sets `-toolexec` to a refusing program | `go/toolexec.sh` |
| cc, gcc, g++, c++ | `PATH` contains `bin/`; `CC` and `CXX` name its scripts | `bin/cc` (the other three are links to it) |

## Prescreen entry point

`prescreen-run.sh` is the only local compiler exception. It lets a configured session tree run a prescreen command
with `cc`, `gcc`, `g++`, `c++`, `CC` and `CXX` available, while every other guard refusal remains active.

Pass the command and its arguments directly to the entry point:

```sh
dev/local-work-guard/prescreen-run.sh <prescreen command> [arguments...]
```

The host stores allowed session roots in `$CHARLIEBOT_HOME/local-work-guard/prescreen-scope.txt`, one session ID per
line. The entry point follows each caller's `task_parent_id` in `sessions/<id>/metadata.json`; a worker under an
allowed root therefore uses the same scope. The file must exist, be readable, and contain at least one valid session
ID. The entry point reads session metadata with `jq` and refuses a caller outside the configured trees.

The entry point removes every occurrence of the local guard's compiler directory from `PATH`, preserving the order of
the other entries, and sets `CC=cc` and `CXX=c++` for the command. It leaves `PYTEST_PLUGINS`, `PYTHONPATH`,
`NODE_OPTIONS`, `MAKEFILES`, `GOFLAGS`, `RUSTUP_TOOLCHAIN` and `RUSTC_WRAPPER` in place, so tests and every other
refused toolchain stay refused.

One lock at `$CHARLIEBOT_HOME/local-work-guard/prescreen-entry.lock` covers the full command. A later command prints
that it is waiting, then starts when the first command exits. The entry lock is separate from locks the prescreen
command takes itself.

Read-only and non-build commands run as usual: the cargo subcommands `fmt`, `metadata` and `tree`, and the
`--version` of each tool.

`refuse.sh` owns the refusal text. Each script calls it with the name of the tool that was run, then exits 97; pytest
exits 4 instead.

The list of refused toolchains is in `dev/remote-run/README.md`. Keep this table in step with that list.

The scripts in `python/` load into an interpreter by path, so they are Python modules. `dev/check-no-python.sh`
exempts this directory for that reason.
