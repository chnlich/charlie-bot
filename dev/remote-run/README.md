# Remote runner

The runner compiles and tests a local tree on a separate build host. It syncs the tree, runs the
command there, and copies the results back to the original directory.

## Invoke

`setup.sh` installs the build host's state once: uv, Rust 1.99.0 with clippy and rustfmt, sccache,
Node 20 with npm, four build slots, and `remote-exec.pl`. It runs from a project directory; a
project with `pyproject.toml` needs its `.venv` present, because setup installs the CPython patch
release it pins. A directory without a Python project gets none.

```sh
cd <project directory>
dev/remote-run/setup.sh
```

`run.sh` syncs and runs one command; `cargo-slot.sh` sends Cargo commands through `run.sh`. Both
run from the directory to run in:

```sh
cd <project directory>/rust
../dev/remote-run/cargo-slot.sh test --workspace --locked

cd <project directory>
dev/remote-run/run.sh node --test

cd /tmp/<any project>
~/workspace/charlie-bot/dev/remote-run/run.sh cargo test
```

## Directory modes

Inside a git worktree, the runner syncs the whole worktree and runs the command in the directory
it was called from. In any other directory (`/tmp`, `~/scripts`, a session directory), it syncs
the current directory. Changes the command leaves behind are copied back to the original
directory in both modes.

In a git tree, the ignore filters protect the remote paths the local tree ignores, so a remote
build output survives the next upload. A directory outside git has no ignore rules: its remote
tree mirrors the directory after every upload, and remote-only files do not survive, so a remote
install or build that a later run needs is rebuilt.

## Configuration

Every host-specific value comes from the host, so no tracked file carries an address or a path
that belongs to one machine. The runner reads the CharlieBot `config.yaml` on each run, through
`charliebot config get`, so an edit to the file applies to the next run with no server restart.

| Value | Where it comes from | Meaning |
|---|---|---|
| `remote_run.host` | the CharlieBot `config.yaml`; no default | The ssh alias of the build host, as `~/.ssh/config` names it. Until it is set, the runner stops with exit 2 and names the key. |
| `remote_run.remote_base` | the CharlieBot `config.yaml`; default `scripts/remote-run` | The runner's state directory on the build host, relative to its `$HOME`. A host with runner state installed elsewhere sets this. |
| `REMOTE_RUN_CONNECT_TIMEOUT` | the environment; default `60` | The connectivity check's time limit in seconds, for tests. |
| `server.public_url` | the CharlieBot `config.yaml`, read with `charliebot config get server.public_url` | The address a person reaches the CharlieBot server at. The stop text links `<public_url>/host-auth`. It has no default; the host sets it. |

The build host's state layout, under its `$HOME` and the remote base: `trees/<tree name>` for the
synced trees, `tree-owners/` for the local root each tree belongs to, `slots/` for the four build
slot locks, `slot-waits.log` for the slot wait record, `cargo/` and `rustup/` for the toolchain,
`bin/` for uv and sccache, `node/` for Node, `python/` and `cache/` for the uv and sccache
caches, and `remote-exec.pl`.

## What one run does

1. The connectivity check: `ssh <host> true`, limited to 60 seconds. See "Exit codes".
2. Preflights: the build host has node, npm, cargo, uv, rsync, git, flock, nice, perl, and
   rustc; a project with `pyproject.toml` has a matching CPython patch release on the build host.
3. The sweep: remote trees whose recorded local root is gone are removed, the tree the run syncs
   into included, so a stale tree under the current name never blocks the sync; a remote tree whose
   recorded root is a different live directory refuses the run, so two local trees never share one
   remote tree.
4. The upload: rsync with the tree's `.gitignore` as filters, excluding `.git`, `.venv`,
   `__pycache__`, and `rust/target`. A remote path the local tree ignores is protected from the
   sync's delete. After a cargo upload, the local `rust/target` is deleted: the build host owns
   the build outputs.
5. The remote preparation, before the command:
   - a synthetic git commit with an `origin/main` ref in the synced tree, refreshed after every
     upload, so tests that read git history pass;
   - a short `TMPDIR` under `/tmp`, so tmux socket paths stay under the 108-byte `sun_path` limit;
   - `node_modules`, when the tree has a `package-lock.json` and no `node_modules`;
   - a `rust-toolchain.toml`'s channel with its components and targets when the channel is
     missing, and its targets otherwise;
   - the environment: the runner's cargo, uv, and node first, then `~/.local/go/bin`,
     `~/.pixi/bin`, and `~/.local/bin`.
6. The command: cargo and cargo-capable uv commands take one of four build slots, get sccache and
   `CARGO_BUILD_JOBS=4`, and log their slot wait to `slot-waits.log`; every command runs under
   `nice -n 10`.
7. The copy-back: rsync of the command's changes to the original directory.

`remote-exec.pl` makes itself a child subreaper on the build host, so daemons the command starts,
tmux servers included, are adopted instead of reparented away. It stops every descendant when the
command ends: SIGTERM, SIGKILL two seconds later, and a stop after eight seconds that names what
outlived it. Its TMPDIR directories under `/tmp` carry the owning process id and start time, and
a later run reclaims the ones no process holds.

## Exit codes

| Code | Meaning |
|---|---|
| command's own code | The connectivity check passed; the exit code comes back unchanged, 255 included. |
| 2 | The runner's own argument or configuration check refused the run: no command, an unset `remote_run.host`, an unsafe `remote_run.remote_base`, an unsafe directory name, or a tree ownership conflict. |
| 86 | The connectivity check saw a Tailscale login link. The runner prints a stop text that tells the agent to end the run and report the blocker, with the full `/host-auth` page URL. |
| 87 | The connectivity check failed any other way, or ran past its 60-second limit. The same stop text is printed. |

## Refused toolchains

When `server.enable_local_work_guard` is true, the agent environment refuses a direct local run of every toolchain
this list names. C and C++ compilers have one scoped local exception for prescreen runs, documented in
`dev/local-work-guard/README.md`; that entry point keeps every other guard refusal active. This list is the single
home of the refused toolchains.

- cargo and rustc
- pytest in all its entry forms: `pytest`, `python -m pytest`, `uv run pytest`, `.venv/bin/pytest`
- `python -m unittest`
- `node --test`, `npm test`, `npm run build`
- make
- `go build`, `go test`, `go run`
- cc, gcc, g++, c++

Pixi tasks are covered through the tools they call.
