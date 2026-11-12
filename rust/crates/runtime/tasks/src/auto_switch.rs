//! The switch a pool makes when the member a run ran on failed: a spent quota, a refused access, or any other member
//! failure the availability owner records.
//!
//! Every pool switch goes through [`switch_member`]. The failed member is recorded out with the shared
//! classification's reason, and the pool's rule selects the next member. The switch record is persisted in the
//! session that owns the switch, and a pool session's current member moves. The caller registers the retry run on the
//! chosen member. The retry's switch note is built when it launches, from the same `handoff` assembly every retry
//! uses.

use charliebot_agents::backend_availability;
use charliebot_agents::model_pool::{self, MemberChange, Selector};
use charliebot_registry::context::FeatureContext;
use charliebot_registry::sequence::QuotaFailure;
use charliebot_store::metadata::SessionMetadata;

use crate::handoff::{self, HandoffSource};

/// The outcome of one worker or cron switch.
#[derive(Debug, PartialEq)]
pub enum MemberSwitch {
  /// The retry runs on `member`, the next selectable member.
  Moved { member: String },
  /// No member is selectable; the message lists every member and why it is out.
  Exhausted(String),
}

/// Move the work of `pool_id` off `from`, whose member failure ended on `failure`.
///
/// `pool_id` is the `backends.pools` entry the run's member was selected from. `session` is the session whose chat
/// records the switch and, when its backend is that pool, whose `pool_member` moves. A verify or cold-read session
/// leaves the initiating session's current member out, as its first selection does. `source` is the failed run: the
/// handoff note reads its history.
pub async fn switch_member(
  features: &FeatureContext,
  pool_id: &str,
  session: &SessionMetadata,
  source: HandoffSource<'_>,
  from: &str,
  failure: &QuotaFailure,
) -> anyhow::Result<MemberSwitch> {
  let cfg = features.config.get()?;
  let availability = backend_availability::backend_availability(features)?;
  availability.record(from, failure.resets_at, &failure.reason, &failure.message)?;
  let selector = Selector::from_features(features, &cfg)?;
  let excluded = crate::spawner_backends::switch_exclusions(&cfg, &features.store, session).await?;
  let Some(member) = selector.select(pool_id, &excluded)? else {
    return Ok(MemberSwitch::Exhausted(selector.exhausted_message(pool_id)?));
  };
  let recovers_at = availability.out_until_minutes(from);
  let reason = format!(
    "pool '{pool_id}' switched from {from} to {}: {}, {} {}",
    member.id,
    backend_availability::switch_cause(&failure.reason),
    backend_availability::switch_recovery_label(&failure.reason),
    recovers_at.clone().unwrap_or_default()
  );
  let handoff = handoff::handoff(features, session, source, &reason)?;
  let change = MemberChange {
    pool: pool_id.to_string(),
    from: from.to_string(),
    to: member.id.clone(),
    reason: failure.reason.clone(),
    recovers_at,
    summary: Some(handoff.summary),
  };
  features.store.persist_and_broadcast(&session.id, change.event()).await?;
  if cfg.backends.pool(&session.backend).is_some() {
    model_pool::set_current_member(features, &session.id, &member.id).await?;
  }
  Ok(MemberSwitch::Moved { member: member.id })
}
