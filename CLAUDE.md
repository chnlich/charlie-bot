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

## Git Conventions
- Commit frequently with descriptive messages
- Format: `type(scope): description` (feat, fix, refactor, test, docs)
- Make atomic commits (one logical change per commit)
- Do NOT push branches to remote

## Output
- When done, output a final summary of all files changed and why

