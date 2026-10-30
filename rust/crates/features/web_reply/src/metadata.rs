//! The web reply package's item models (`src/features/web_reply/metadata.py`): the Waiting on you item that the status
//! and the plan links read, and the Remaining milestone that the status lists.

use charliebot_store::metadata::UtcDateTime;
use serde::{Deserialize, Serialize};

/// One Remaining item: a milestone between the session and its Goal, and whether the session completed it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RemainingItem {
  pub(crate) text: String,
  pub(crate) done: bool,
}

/// One Waiting on you item: a decision or an action, its context, and the link to the place where the user answers it
/// (`metadata.WaitingItem`). An absent context stays absent in stored legacy items. `since` and `since_exact` are
/// written by the server: `since` is the time the item first appeared or a lower bound on it, and `since_exact` is
/// false when `since` is that lower bound. `tier` and `rule` are written by the server from the item's ask cold read:
/// the tier the cold read gave (t0, t1, t2, user-act or review) and the rule that names it, null for none. An item
/// stored before the tier existed carries no tier, and its rule reads as null.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WaitingItem {
  pub(crate) text: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub(crate) context: Option<String>,
  pub(crate) link: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub(crate) since: Option<UtcDateTime>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub(crate) since_exact: Option<bool>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub(crate) tier: Option<String>,
  #[serde(default)]
  pub(crate) rule: Option<String>,
}
