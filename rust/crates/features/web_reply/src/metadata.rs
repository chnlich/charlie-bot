//! The web reply package's item model: the Waiting on you item that the status
//! and the plan links read.

use charliebot_store::metadata::UtcDateTime;
use serde::{Deserialize, Serialize};

/// One Waiting on you item: a decision or an action, its context, and the link to the place where the user answers it.
/// An absent context stays absent in stored legacy items. `since` and `since_exact` are
/// written by the server: `since` is the time the item first appeared or a lower bound on it, and `since_exact` is
/// false when `since` is that lower bound.
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
}
