# charlie-bot Repository Guide

## Coding Standards
- Google Code Style
- 2-space indentation, 120-column limit
- Type annotations on all functions
- Docstrings for public APIs only
- Test budget: a unit test under 2 s, a `@pytest.mark.integration` test under 10 s, at most 50 integration tests — `tests/conftest.py` enforces all three
- `scripts/` holds the scripts a CharlieBot operator runs on their own host (install, server launch, skill sync, backend preflight); repo maintenance tools (style and leak checks, git hooks, browser and live harnesses, builds, eval runners) live in `tools/`.

- Imports name modules (Google Python Style Guide 2.2): `from src.infra import models`, then
  `models.SessionMetadata`; symbols from `typing`, `typing_extensions` and `collections.abc`
  import directly. New imports take this form; convert a file's existing imports when a change touches it.
- `uv run yapf -i <file>` formats each Python file a change touches.

### Module Boundaries

- Keep features behind clean module boundaries, including features used only by other installs.
- Simplify shared modules by removing import cycles and backend-specific branches.
- Estimate deletion cost from references outside a feature's module; source-file line counts understate that cost.

## Test and Verification

- Run the full suite with `uv run pytest -q` from any checkout.
- Run `uv sync` in a fresh worktree before testing. In a worktree, `uv run <cmd>` builds its local `.venv`; include
  `uv run` commands in task specs.
- Reproduce a load-sensitive failure by forcing interleaving.
- Remove a `noqa` directive only when `ruff check --extend-select RUF100` reports it unused. `--extend-select` keeps
  the project's rule selection.
- Gate YAPF on a single-file exit code or empty diff output.
- Every pytest run through the project venv self-restricts before collection (CHARLIEBOT_USE_TEST_SANDBOX, default 1): it can signal only processes it started, and its default tmux and `systemctl --user` calls cannot reach the live server.

### HTTP Clients and Test Isolation

- Build a per-call `httpx.AsyncClient` with a deferred import in each new low-rate HTTP consumer.
- Patching a consumer module's `httpx.AsyncClient` name changes the shared module attribute.
- Build one client per call so the fake stays scoped to that call.
- Starlette `TestClient` serves each request on a fresh event loop.
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

