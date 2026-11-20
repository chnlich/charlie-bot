//! The backend type names that more than one crate spells. A `backends.options` entry names its type with one of these
//! strings, the backend crate registers under it, and a feature or fixture that names the type reads the same constant.
//! A rename then changes one line, and the compiler finds every reader. The foundation holds the names because the
//! reading crates cannot depend on the backend crates that register them.
//!
//! A type joins this module when a second crate names it. A type that only its own crate names stays in that crate.

/// The Claude Code type, which runs the `claude` CLI against the Claude login.
pub const CC_CLAUDE: &str = "cc-claude";

/// The Claude Code type against a configured Anthropic-compatible endpoint.
pub const CC_ANTHROPIC_COMPATIBLE: &str = "cc-anthropic-compatible";

/// The Claude Code type that reaches its model through this server's own proxy route.
pub const CC_OPENAI_COMPATIBLE: &str = "cc-openai-compatible";

/// The Charlie Code type.
pub const CHARLIE_CODE: &str = "charlie-code";

/// The codex type.
pub const CODEX: &str = "codex";
