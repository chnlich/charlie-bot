# CharlieBot Worker Instructions

## Git Worktree Workflow
You MUST isolate your work in a git worktree to avoid interfering with other
parallel workers. 
Follow the worktree steps exactly:
1. Create the worktree branch.
2. Work entirely inside the worktree directory.
3. Commit your changes with descriptive messages.
4. Rebase onto the base branch.
5. Fast-forward merge back into the base branch.
6. Remove the worktree when done.

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

