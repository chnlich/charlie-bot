//! The tool names that start a subagent in a Claude Code transcript.
//!
//! The Claude Code CLI names its subagent tool `Agent`, and older transcripts name it `Task`. The names live here
//! because a backend may depend only on infra and runtime: the Claude Code backend writes `Agent` into its
//! `--disallowed-tools` value, the store starts a subagent card on either name, and the CLI's read budget counts
//! either tool's result as the child's conclusion.

/// The subagent tool name of a current Claude Code transcript.
pub const AGENT_TOOL: &str = "Agent";

/// The subagent tool name of a transcript written before the rename.
pub const LEGACY_AGENT_TOOL: &str = "Task";

/// Every tool name that starts a subagent.
pub const SUBAGENT_TOOL_NAMES: [&str; 2] = [AGENT_TOOL, LEGACY_AGENT_TOOL];
