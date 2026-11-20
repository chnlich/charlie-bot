//! The web reply package's item model: the Waiting on you item that the status
//! and the plan links read.

use charliebot_store::metadata::UtcDateTime;
use serde::{Deserialize, Serialize};

/// One Waiting on you item: a decision or an action, its context, and the link to the place where the user answers it.
/// An absent context stays absent in stored legacy items. `since` and `since_exact` are
/// written by the server: `since` is the time the item first appeared or a lower bound on it, and `since_exact` is
/// false when `since` is that lower bound. `judged_kind` and `judgement_basis` are written by the server's judge; a
/// stored record from before the judge leaves both absent.
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
  /// The user-only kind the judge matched; absent when none fits.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub judged_kind: Option<JudgedKind>,
  /// The judge's basis; a stored item holds the empty string, because the judge stores only accepted items.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub judgement_basis: Option<String>,
}

/// The four kinds of step that only the user decides, as `prompts/master.md` lists them under Waiting on you.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JudgedKind {
  /// The take off of a high-consequence step.
  TakeOff,
  /// An explicit user approval that a rule requires.
  ExplicitApproval,
  /// An action or a fact that only the user has.
  ActionOrFact,
  /// The review of a delivered product.
  DeliveryReview,
}

#[cfg(test)]
mod tests {
  use serde_json::{Value, json};

  use super::*;

  #[test]
  fn a_record_without_the_judge_fields_loads_and_an_accepted_item_writes_an_empty_basis() {
    let legacy: WaitingItem =
      serde_json::from_value(json!({"text": "Choose", "link": null})).expect("a legacy item loads");
    assert_eq!(legacy.judged_kind, None);
    assert_eq!(legacy.judgement_basis, None);
    assert!(serde_json::to_value(&legacy).expect("the item serializes").get("judgement_basis").is_none());

    let accepted =
      WaitingItem { judged_kind: Some(JudgedKind::ActionOrFact), judgement_basis: Some(String::new()), ..legacy };
    let written: Value = serde_json::to_value(&accepted).expect("the item serializes");
    assert_eq!(written["judged_kind"], "action_or_fact");
    assert_eq!(written["judgement_basis"], "");
  }
}
