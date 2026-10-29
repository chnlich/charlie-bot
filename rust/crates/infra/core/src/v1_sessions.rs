//! The v1 session conversion directions the store's startup refusal and metadata decoder quote.

/// The deleted v1 session conversion script.
pub const CONVERSION_SCRIPT: &str = "scripts/v1_session_conversion.py";

/// Directions for running the deleted v1 session conversion script from a full worktree at the commit that still
/// carries it. A worktree, not `git show`, because the script imports the repository's Python `src/` tree.
pub const V1_SESSION_CONVERSION_GUIDANCE: &str = "the conversion script was deleted. Run it from a full worktree at the commit that still carries it: `git worktree add <dir> e2f27b133` creates the worktree, `uv sync --frozen` inside it builds the script's environment, and from that directory `uv run python scripts/v1_session_conversion.py dry-run --home HOME` followed by `apply --home HOME` converts HOME's v1 sessions, where HOME names the refused home.";
