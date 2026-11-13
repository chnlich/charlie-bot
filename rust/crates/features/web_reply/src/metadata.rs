//! The web reply package's item model: the Waiting on you item that the status
//! and the plan links read.

use charliebot_core::approval_policy::ApprovalRuleRef;
use charliebot_store::metadata::UtcDateTime;
use serde::{Deserialize, Serialize};

/// One Waiting on you item: a decision or an action, its context, and the link to the place where the user answers it.
/// An absent context stays absent in stored legacy items. `since` and `since_exact` are
/// written by the server: `since` is the time the item first appeared or a lower bound on it, and `since_exact` is
/// false when `since` is that lower bound. The server writes the ask cold read's tier, policy rule, executor, exact
/// current session-rule citations, backend and raw tier line. An item stored before those fields existed carries no
/// assessment.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WaitingItem {
  pub text: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub context: Option<String>,
  pub link: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub since: Option<UtcDateTime>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub since_exact: Option<bool>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub tier: Option<String>,
  #[serde(default)]
  pub rule: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub executor: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub approval_rule_refs: Option<Vec<ApprovalRuleRef>>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub backend: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub raw_tier_line: Option<String>,
}
