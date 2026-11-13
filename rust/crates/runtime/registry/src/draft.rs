//! The draft-platform contract: one feature per platform validates that platform's target and sends a draft's body
//! from the operator's own account.
//!
//! The drafts package owns the card, the approval endpoint, the send lock and the three session events. It reaches a
//! platform only through this trait, so a new platform adds only its own module and its own registry entry.

use std::sync::Arc;

use charliebot_core::config::CharlieBotConfig;
use charliebot_core::credentials::Credentials;
use charliebot_core::hot_reload::HotReload;
use futures_util::future::BoxFuture;
use serde_json::Value;

/// The live inputs a platform sender reads when it is built: the host config and the credentials as they are now.
pub struct DraftContext {
  pub config: Arc<HotReload<CharlieBotConfig>>,
  pub credentials: Arc<HotReload<Credentials>>,
}

impl DraftContext {
  /// The context built from the two live inputs.
  pub fn new(config: Arc<HotReload<CharlieBotConfig>>, credentials: Arc<HotReload<Credentials>>) -> Self {
    Self { config, credentials }
  }
}

/// What one successful send returned: the sending account and the message's URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftSent {
  /// The Slack user ID or the GitHub login the platform reports as the message's author.
  pub sender: String,
  /// The platform's URL for the sent message.
  pub url: String,
}

/// How a failed send left the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftFailureOutcome {
  /// The platform answered with an error, so nothing was sent.
  Rejected,
  /// The request timed out or its response was lost, so the message may have gone out.
  Unknown,
}

impl DraftFailureOutcome {
  /// The event's `outcome` value.
  pub fn as_str(self) -> &'static str {
    match self {
      DraftFailureOutcome::Rejected => "rejected",
      DraftFailureOutcome::Unknown => "unknown",
    }
  }
}

/// Why one send failed: the text the card shows and whether the target may hold the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftSendFailure {
  pub detail: String,
  pub outcome: DraftFailureOutcome,
}

impl DraftSendFailure {
  /// The send the platform refused: nothing reached the target.
  pub fn rejected(detail: impl Into<String>) -> Self {
    Self { detail: detail.into(), outcome: DraftFailureOutcome::Rejected }
  }

  /// The send whose response was lost: the target may hold the message.
  pub fn unknown(detail: impl Into<String>) -> Self {
    Self { detail: detail.into(), outcome: DraftFailureOutcome::Unknown }
  }
}

/// One platform's draft support: its target validation, the target's card label, and the sender.
///
/// The drafts package builds an instance from the live [`DraftContext`] on each request, so the sender reads the
/// platform's credentials and host config as they are now.
pub trait DraftPlatform: Send + Sync {
  /// The platform name its drafts carry.
  fn name(&self) -> &'static str;

  /// One target in canonical form, or an error sentence the caller prints.
  fn validate_target(&self, target: &Value) -> Result<Value, String>;

  /// The target's one-line label for the draft card.
  fn target_label(&self, target: &Value) -> String;

  /// The send of `body` exactly as registered. A transport failure after the request was sent reports `Unknown`; a
  /// platform error response reports `Rejected`.
  fn send<'a>(&'a self, target: &'a Value, body: &'a str) -> BoxFuture<'a, Result<DraftSent, DraftSendFailure>>;
}

/// The constructor of one platform's [`DraftPlatform`] from the live [`DraftContext`].
pub type DraftPlatformFactory = Arc<dyn Fn(&DraftContext) -> anyhow::Result<Arc<dyn DraftPlatform>> + Send + Sync>;
