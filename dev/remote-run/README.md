# Remote runner

The runner compiles and tests a local tree on a separate build host. It syncs the tree, runs the
command there, and copies the results back to the original directory.

## Invoke

`setup.sh` installs the build host's state once: uv, Rust 1.99.0 with clippy and rustfmt, sccache,
Node 24 with npm, four build slots, and `remote-exec.pl`. It runs from a project directory; a
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
| `REMOTE_RUN_TREE_RETENTION_DAYS` | the environment; default `3` | The number of days since the runner last synced or ran in a tree, after which the sweep removes the tree. The tree must pass the sweep's checks (see step 3). A positive whole number; the dry run uses the same setting. |
| `server.public_url` | the CharlieBot `config.yaml`, read with `charliebot config get server.public_url` | The address a person reaches the CharlieBot server at. Exit 86 links its `/host-auth` page. It has no default; the host sets it. |

Preview the sweep without running a project command:

```sh
dev/remote-run/run.sh --dry-run
```

The output lists each selected tree with its allocated bytes, then a total count and size. Set the retention age for
one invocation with `REMOTE_RUN_TREE_RETENTION_DAYS`, for example:

```sh
REMOTE_RUN_TREE_RETENTION_DAYS=45 dev/remote-run/run.sh --dry-run
```

The build host's state layout, under its `$HOME` and the remote base: `trees/<tree name>` for the
synced trees, `tree-owners/<tree name>` for `host=<caller hostname>` and `root=<local absolute path>`,
`tree-last-use/<tree name>` for the epoch second of the tree's last sync or run, `tree-owners.lock` for the lock
that serializes every owner record and tree change, `tree-locks/<tree name>.lock` for activity in each tree
(these small lock files remain after tree removal so a waiting process cannot acquire a different lock file for
the same name), `slots/` for the four
build slot locks, `slot-waits.log` for the slot wait record, `cargo/` and
`rustup/` for the toolchain,
`bin/` for uv and sccache, `node/` for Node, `python/` and `cache/` for the uv and sccache
caches, and one `remote-exec-<hash>.pl` for each `remote-exec.pl` content a run has synced. A run
executes the copy named for its own checkout's content hash.

## What one run does

1. The connectivity check: `ssh <host> true`, limited to 60 seconds. See "Exit codes".
2. Preflights: the build host has node, npm, cargo, uv, rsync, git, flock, nice, perl, and
   rustc; a project with `pyproject.toml` has a matching CPython patch release on the build host.
3. The sweep: current-format records are considered only when their caller hostname matches this machine.
   A tree whose current record names a local root that is gone is removed at once, as before. Every other tree
   is removed once its last use is `REMOTE_RUN_TREE_RETENTION_DAYS` days old: a tree whose local root is live, a
   path-only legacy record (which proves no owner, so it never takes the gone-root removal), or no record at all.
   A tree's last use is the epoch second in its `tree-last-use` record. The sync writes that record at its claim,
   and a run writes it at its start and its end. A tree without the record takes its newest entry's modification
   time once, and a real sweep saves that time as the record, so later sweeps read the record and do not walk the
   tree. The sync reads every last-use record in one call and keeps a live-root tree whose record is younger than the
   retention age without a build-host call; every other tree gets its own sweep call. A record whose tree is
   missing is dropped. Before any removal, the sweep checks the tree's activity lock,
   then every live process whose cwd, executable, open fd or command line is inside or names the tree. A held lock
   or such a process keeps the tree. Dirty applies only to a tree that has `.git`. Runner trees are uploaded
   without `.git`, so their source of truth is the local worktree. The sync then creates a synthetic repository in
   each tree for release tests that read history, and the dirty check reads that repository. It therefore reports
   changes that a run made after the last sync, and the next sync commits them. The sweep never stops a process. `--dry-run` runs the
   same checks and prints the same selection with each tree's allocated bytes, and it deletes no tree, record or
   last-use file.
4. The stop and the claim: before a sync replaces an owned tree, `remote-exec.pl` sends TERM to same-owner processes
   whose cwd or executable path is inside the tree, waits two seconds, then sends KILL to survivors. It reports the
   stopped pids. A process inspection, stop or removal error leaves the tree in place and fails the run. The claim
   then writes the tree's owner record and creates the tree under the owner lock, and writes the tree's last-use
   record under its activity lock. A claim whose tree cannot be created removes its record again, so a record and
   its tree are written together and a record outlives a stopped claim only until the next sweep drops it.
5. The upload: rsync with the tree's `.gitignore` as filters, excluding `.git`, `.venv`,
   `__pycache__`, and `rust/target`. A remote path the local tree ignores is protected from the
   sync's delete. After a cargo upload, the local `rust/target` is deleted: the build host owns
   the build outputs.
6. The remote preparation, before the command:
   - a synthetic git commit with an `origin/main` ref in the synced tree, refreshed after every
     upload, so tests that read git history pass;
   - a short `TMPDIR` on the `/dev/shm` tmpfs, so release trial socket paths fit the trial's
     `sun_path` preflight and a test's fsync does not wait on the build host's disk;
   - `node_modules`, when the tree has a `package-lock.json` and no `node_modules`;
   - a `rust-toolchain.toml`'s channel with its components and targets when the channel is
     missing, and its targets otherwise;
   - the environment: the runner's cargo, uv, and node first, then `~/.local/go/bin`,
     `~/.pixi/bin`, and `~/.local/bin`.
7. The command: cargo and cargo-capable uv commands take one of four build slots, get sccache and
   `CARGO_BUILD_JOBS=4`, and log their slot wait to `slot-waits.log`; every command runs under
   `nice -n 10`.
8. The copy-back: rsync of the command's changes to the original directory.

`remote-exec.pl` makes itself a child subreaper on the build host, so daemons the command starts,
tmux servers included, are adopted instead of reparented away. It stops every descendant when the
command exits or receives a signal, and treats a closed SSH output channel as a caller disconnect.
Stopping sends SIGTERM, then SIGKILL two seconds later; any process still alive after eight seconds
is named in the error output. Its tree-stop mode checks process cwd and executable paths before a sync
replaces an owned tree. The sweep stops no process. Its TMPDIR directories under `/dev/shm`
carry the owning process id and start time, and a later run reclaims the ones no process holds.

## Exit codes

| Code | Meaning |
|---|---|
| command's own code | The connectivity check passed; the exit code comes back unchanged, 255 included. |
| 2 | The runner refused the run because an argument, configuration, ownership, process inspection, stop, or removal check failed. |
| 86 | The connectivity check saw a Tailscale login link. The runner prints a stop text that tells the agent to end the run and report the blocker, with the full `/host-auth` page URL. |
| 87 | The connectivity check failed any other way, or ran past its 60-second limit. The runner tells the agent to end the run and report the blocker without retrying; it prints no `/host-auth` URL. |

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
