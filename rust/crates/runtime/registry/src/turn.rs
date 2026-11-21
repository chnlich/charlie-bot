//! The contracts a turn contribution follows.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use crate::registry::{
  DEFAULT_WORKFLOW_RULES_FILE, DELIVERY_FULL, DELIVERY_INDEX, EventRenderer, PromptSource, RuleSegment, SCOPE_BASE,
  SCOPE_GOAL, SCOPE_MEMORY, SCOPE_NODE, SCOPE_SUBTREE, SCOPE_TASTE, TurnContribution,
};

/// The input every turn-end check reads: the session's turn, with its exit and its stored status.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TurnInput {
  pub session_id: String,
  /// `web`, a thread platform's name (`slack`, `discord`, `feishu`), or null for a plain manager session.
  pub kind: Option<String>,
  /// The turn's exit code; a zero-output turn carries 1.
  pub exit_code: i64,
  /// Whether the turn is the continuation of a block and exited with an error. Such a turn misses the checks that
  /// blocked it again, so each of those checks answers its second miss rather than passing the turn.
  pub continuation_failed: bool,
  /// The session's stored status payload (goal, now, updated_at, waiting), or null when it stores none.
  pub status: Value,
  /// The provenance fragment of the last received input's child report (`clone of <id>`), or null when that input is
  /// not a report. The prompt names the report's source with it, so a check quotes the report as the prompt renders it.
  pub last_input_provenance: Option<String>,
  /// The events since the previous `master_done`, in order, up to the call. An input the running run received carries
  /// `received_at`, the moment of its receipt.
  pub turn: Vec<Value>,
}

impl TurnInput {
  /// Whether a check judges this turn's exit: a turn that exited cleanly, or a failed continuation, which a check
  /// judges as a miss. Any other error exit is not judged, so the check passes it.
  pub fn exit_is_judged(&self) -> bool {
    self.exit_code == 0 || self.continuation_failed
  }
}

/// A turn-end check's answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckResult {
  Pass,
  /// The check passes and its notice reaches the user.
  Notice(String),
  /// The check misses; the reason continues the turn.
  Block(String),
}
