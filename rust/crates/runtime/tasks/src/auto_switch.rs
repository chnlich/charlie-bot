//! The switch a pool makes when the member a run ran on has spent its quota.
//!
//! Every pool switch goes through [`switch_member`]: the spent member is recorded out, the pool's rule selects the next
//! member, the switch record is persisted in the session that owns the switch, and that session's current member
//! moves. The caller registers the retry run on the chosen member. The retry's switch note is built when it launches,
//! from the same `handoff` assembly every retry uses.

use charliebot_agents::backend_availability::{self, REASON_QUOTA_ERROR};
use charliebot_agents::model_pool::{self, MemberChange, RotationPositions};
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

/// Move the work of `pool_id` off `from`, whose quota ended on `failure`.
///
/// `pool_id` is the `backends.pools` entry the run's member was selected from. `owner` is the session whose member
/// changes and whose chat records the switch. `run_session` is the session the failed run and its retry belong to, and
/// `source` is the failed run: the handoff note reads its history. A manager turn passes the same session twice; a
/// worker or cron child passes its pool parent and itself.
pub async fn switch_member(
  features: &FeatureContext,
  pool_id: &str,
  owner: &SessionMetadata,
  run_session: &SessionMetadata,
  source: HandoffSource<'_>,
  from: &str,
  failure: &QuotaFailure,
) -> anyhow::Result<AutoSwitch> {
  let cfg = features.config.get()?;
  let availability = backend_availability::backend_availability(features)?;
  availability.record(from, failure.resets_at, REASON_QUOTA_ERROR, &failure.message)?;
  let now = UtcDateTime::now();
  let pool = model_pool::pool_by_id(&cfg, pool_id)?;
  let positions = features.startup.require::<RotationPositions>()?;
  let member_loads = model_pool::member_loads(features, &pool)?;
  let Some(member) = positions.select(&cfg, &features.registry, &availability, &pool, &[], now, &member_loads)?
  else {
    return Ok(AutoSwitch::Exhausted(model_pool::all_exhausted_message(
      &cfg,
      &features.registry,
      &availability,
      &pool,
    )?));
  };
  let recovers_at = availability.out_until_minutes(from);
  let reason = format!(
    "pool '{pool_id}' switched from {from} to {}: the member's quota is spent, resets at {}",
    member.backend,
    recovers_at.clone().unwrap_or_default()
  );
  let goal = run_session.task.as_ref().map_or(run_session.name.as_str(), |task| task.goal.as_str());
  let handoff = handoff::handoff(features, run_session, source, &reason, goal);
  let change = MemberChange {
    pool: pool.id.clone(),
    from: from.to_string(),
    to: member.backend.clone(),
    reason: event_types::QUOTA_REASON_QUOTA_ERROR,
    recovers_at,
    summary: Some(handoff.summary),
  };
  features.store.persist_and_broadcast(&owner.id, change.event()).await?;
  model_pool::set_current_member(features, &owner.id, &member.backend).await?;
  Ok(AutoSwitch::Moved { member: member.backend })
}
