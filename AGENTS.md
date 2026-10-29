# charlie-bot Repository Guide

## Coding Standards
- Google Code Style
- 2-space indentation, 120-column limit
- `rustfmt` and `clippy` gate the Rust code: `cargo fmt --check` and `cargo clippy --workspace --all-targets -- -D warnings` from `rust/`
- `scripts/` holds the scripts a CharlieBot operator runs on their own host (install, server launch, skill sync, backend preflight); repo maintenance tools (leak checks, git hooks, browser and live harnesses, builds, eval runners) and the improve cron prompts (the landing protocol, the five direction prompts and `inflight-files.sh`) live in `tools/`.

### Module Boundaries

- Keep features behind clean module boundaries, including features used only by other installs.
- Simplify shared modules by removing import cycles and backend-specific branches.
- Estimate deletion cost from references outside a feature's module; source-file line counts understate that cost.

## Test and Verification

- Run the Rust suite from `rust/`: `cargo test --workspace --locked`. The frontend JavaScript tests run with
  `node --test` from the checkout root, as CI does.
- Run Cargo on the shared test host through `cargo-slot.sh` when the task names it; build the CLI locally with
  `cargo build --locked -p charliebot-cli --bin charliebot` from `rust/`.
- Reproduce a load-sensitive failure by forcing interleaving.
- Gate a format check on a single-file exit code or empty diff output.

### Script Tests

- `charliebot-shell-harness` (`rust/crates/infra/shell-harness`) runs a repository shell script for real, against
  stub commands on `PATH` (`rust/README.md`, "Shell-script tests").
- A stub never starts an interpreter: stubs are shell scripts or Rust programs.

### HTTP Clients and Test Isolation

- Build a per-call HTTP client in each new low-rate HTTP consumer; patching a shared client name changes the
  shared module attribute.
- Take shared tree locks before a test's first request. Use lock-free reads after that request.

### Live Verification

- Verify changes on a synthetic `CHARLIEBOT_HOME`.
- Use a trial home with `charliebot-server` to test real delayed triggers.
- Copy a session directory with its CLI continuity files when staging it in another home.
- Child agents inherit the server's environment through `agent_environment` (`rust/crates/runtime/agents/src/agent_environment.rs`).
- `child_env` in `rust/crates/runtime/tasks/src/execution.rs` sets `CHARLIEBOT_HOME` in each run to the server's resolved profile home.
  It sets the variable also when the server started without it.
- Assign a fresh path to a scratch home. Delete only that path.
- Remove agent CLIs from `PATH` and assert that a probe launches zero child processes.

### Model-Based Checks

- Gate exit codes on mechanical answer shape. Print model judgments for a human to assess.
- Judge glossary probes against the page's declared reader model. Expect repeated probe rounds on real pages.

## Git Conventions
- Commit frequently with descriptive messages
- Format: `type(scope): description` (feat, fix, refactor, test, docs)
- Make atomic commits (one logical change per commit)
