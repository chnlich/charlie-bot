//! The switch an Auto session makes when the member a run ran on has spent its quota.
//!
//! Every Auto switch goes through [`switch_member`]: the spent member is recorded out, the next member is selected by
//! `auto_backend::select_member`, the switch record is persisted in the Auto session, and the session's current member
//! moves. The caller registers the retry run on the chosen member. The retry's switch note is built when it launches,
//! from the same `handoff` assembly every retry uses.

use charliebot_agents::auto_backend::{self, MemberChange};
use charliebot_agents::backend_availability::{self, REASON_QUOTA_ERROR};
use charliebot_core::event_types;
use charliebot_registry::context::FeatureContext;
use charliebot_registry::sequence::QuotaFailure;
use charliebot_store::metadata::{SessionMetadata, UtcDateTime};

use crate::handoff::{self, HandoffSource};

/// The outcome of one Auto switch.
#[derive(Debug, PartialEq)]
pub enum AutoSwitch {
  /// The retry runs on `member`, the next selectable member.
  Moved { member: String },
  /// No member is selectable; the message lists every member and why it is out.
  Exhausted(String),
}

/// Move an Auto session's work off `from`, whose quota ended on `failure`.
///
/// `auto_session` is the Auto session whose member changes and whose chat records the switch. `run_session` is the
/// session the failed run and its retry belong to, and `source` is the failed run: the handoff note reads its history.
/// A manager turn passes the same session twice; a worker or cron child passes its Auto parent and itself.
pub async fn switch_member(
  features: &FeatureContext,
  auto_session: &SessionMetadata,
  run_session: &SessionMetadata,
  source: HandoffSource<'_>,
  from: &str,
  failure: &QuotaFailure,
) -> anyhow::Result<AutoSwitch> {
  let cfg = features.config.get()?;
  let availability = backend_availability::backend_availability(features)?;
  availability.record(from, failure.resets_at, REASON_QUOTA_ERROR, &failure.message)?;
  let now = UtcDateTime::now();
  let Some(member) = auto_backend::select_member(&cfg, &features.registry, &availability, &[], now)? else {
    return Ok(AutoSwitch::Exhausted(auto_backend::all_exhausted_message(features, &cfg)?));
  };
  let recovers_at = availability.out_until_minutes(from);
  let reason = format!(
    "auto switched from {from} to {}: the member's quota is spent, resets at {}",
    member.backend,
    recovers_at.clone().unwrap_or_default()
  );
  let goal = run_session.task.as_ref().map_or(run_session.name.as_str(), |task| task.goal.as_str());
  let handoff = handoff::handoff(features, run_session, source, &reason, goal);
  let change = MemberChange {
    from: from.to_string(),
    to: member.backend.clone(),
    reason: event_types::QUOTA_REASON_QUOTA_ERROR,
    recovers_at,
    summary: Some(handoff.summary),
  };
  features.store.persist_and_broadcast(&auto_session.id, change.event()).await?;
  auto_backend::set_current_member(features, &auto_session.id, &member.backend).await?;
  Ok(AutoSwitch::Moved { member: member.backend })
}
