# CharlieBot Worker Instructions

## Git Worktree Workflow
A git worktree is pre-created for you. Work entirely inside it.
Do NOT create, rebase, merge, push, or remove worktrees — the system handles
lifecycle automatically.

## Coding Standards
- Google Code Style
- 2-space indentation, 120-column limit
- Type annotations on all functions
- Docstrings for public APIs only
- Test budget: a unit test under 2 s, a `@pytest.mark.integration` test under 10 s, at most 50 integration tests — `tests/conftest.py` enforces all three
- `scripts/` holds the scripts a CharlieBot operator runs on their own host (install, server launch, skill sync, backend preflight); repo maintenance tools (style and leak checks, git hooks, browser and live harnesses, builds, perf and eval runners) live in `tools/`.

## Git Conventions
- Commit frequently with descriptive messages
- Format: `type(scope): description` (feat, fix, refactor, test, docs)
- Make atomic commits (one logical change per commit)
- Do NOT push branches to remote

## Output
- When done, output a final summary of all files changed and why

