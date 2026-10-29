//! The one managed-instruction assembly every agent run draws from (`src/runtime/task_prompts.py`, and the tree
//! assembly of `src/runtime/task_execution.py:279-378`).
//!
//! A run's managed instructions are an ordered list of blocks:
//!
//! 1. the common execution rules, the run-kind rules, the host supplement and the model overlay;
//! 2. the segments of the registered turn contributions, in registration order;
//! 3. each subtree rule from the root down to the node itself;
//! 4. the node's own node rule.
//!
//! Text that repeats is emitted once, at its first position, and each source of every repeat is merged there.
//! Goals, inputs, bindings and worktree facts are not managed instructions: the launch renders them separately.
//!
//! `prompt_hash` covers the blocks (text, sources, delivery and body ref). `char_count` measures the joined text the
//! backend receives. A cold-read run is the one exception: its managed instructions are one block, the cold-reader
//! contract.

mod coherent;
mod render;
mod run_context;
mod snapshot;
mod sources;

use charliebot_core::RepoRoot;
use charliebot_core::config::CharlieBotConfig;
use charliebot_registry::Registry;

pub use crate::spawner_prompt::reviewer_run_follows;
pub use charliebot_registry::turn::{PromptSource, RuleSegment};
pub use coherent::{
  CoherentSnapshot, assemble_coherent_snapshot, assemble_coherent_snapshot_for_launch, capture_prompt_chain,
  resolve_launch_overlay,
};
pub use render::{
  binding_intro, render_session_info, render_task_body, render_worktree_bindings, render_worktree_persistence,
  repo_less_review_task_context, review_task_context,
};
pub use run_context::{ContextReadError, read_run_context};
pub use snapshot::{PromptBlock, PromptSnapshot, assemble_snapshot};
pub use sources::{ChainLink, OverlayError, RuleInputs, build_segments, prompt_task_type, read_local_rule_body};

/// The snapshot file a run directory holds (`SNAPSHOT_FILENAME`).
pub const SNAPSHOT_FILENAME: &str = "prompt_snapshot.json";
/// The launch text file a run directory holds (`LAUNCH_TEXT_FILENAME`).
pub const LAUNCH_TEXT_FILENAME: &str = "launch_prompt.md";

/// Provenance scopes of a block source: the shared rules, the memory selection, a subtree rule, a node rule.
pub const SCOPE_BASE: &str = "base";
pub const SCOPE_MEMORY: &str = "memory";
pub const SCOPE_SUBTREE: &str = "subtree";
pub const SCOPE_NODE: &str = "node";

/// The delivery of a full text block; the only delivery the assembly emits.
pub const DELIVERY_FULL: &str = "full";

/// The run kinds that take the manager contract (`MANAGER_KINDS`).
pub const MANAGER_KINDS: [&str; 1] = ["manager_turn"];
/// The run kinds that take the worker contract (`WORKER_KINDS`).
pub const WORKER_KINDS: [&str; 4] = ["work", "review", "iteration", "scheduled_step"];

/// The bounded number of coherence passes the launch-time recheck runs (`_COHERENCE_PASSES`).
pub const COHERENCE_PASSES: usize = 3;

/// The weighted-character limit of one inter-agent message (`src/infra/constants.py`, `MAX_AGENT_MESSAGE_WEIGHTED_CHARS`),
/// filled into the manager contract's `{{agent_message_limit}}` token.
pub const MAX_AGENT_MESSAGE_WEIGHTED_CHARS: usize = 2000;

/// The host supplement under the home (`CharlieBotConfig.claude_md_file`).
pub const HOST_SUPPLEMENT_FILENAME: &str = "MASTER_AGENT_PROMPT.md";
/// The immutable rule bodies under the home (`prompt_bodies/<sha256>.md`), the directory `Store::prompt_bodies_dir` names.
pub const PROMPT_BODIES_DIR: &str = "prompt_bodies";

/// A managed instruction source is unavailable, malformed, or hash-invalid. The message names the source and the
/// reason. It blocks the launch: the queued run and its inputs stay untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskPromptError {
  message: String,
}

impl TaskPromptError {
  pub fn new(message: impl Into<String>) -> Self {
    Self { message: message.into() }
  }
}

impl std::fmt::Display for TaskPromptError {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter.write_str(&self.message)
  }
}

impl std::error::Error for TaskPromptError {}

/// What one assembly reads: the config for the home, the registry contributions, and the validated checkout root.
pub struct PromptEnv<'a> {
  pub cfg: &'a CharlieBotConfig,
  pub registry: &'a Registry,
  pub checkout: &'a RepoRoot,
}
