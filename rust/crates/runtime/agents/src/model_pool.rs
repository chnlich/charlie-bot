//! The model pool: the one `select` that picks the member a call runs on, under either of two rules.
//!
//! A pool is a list of `backends.options` ids and a rule. The Auto set (`backends.auto`) is a priority pool; the
//! rotation list (`backends.pool`) is a rotate pool. Both rules skip a member that the caller excludes, that
//! [`BackendAvailability`] holds out, or whose usage reading says it is exhausted, so the member one rule recorded
//! out is skipped by the other. The module also owns the `auto_member` session slot and the turn-start decision of a
//! session whose backend is `auto`.

use anyhow::anyhow;
use charliebot_core::config::{AUTO_BACKEND_ID, AutoBilling, AutoConfig, AutoMember, BackendOption, CharlieBotConfig};
use charliebot_core::{event_types, metadata_slot};
use charliebot_registry::backend::MemberReading;
use charliebot_registry::context::FeatureContext;
use charliebot_registry::{Registry, RegistryError};
use charliebot_store::metadata::{SessionMetadata, UtcDateTime};
use serde_json::{Map, Value, json};

use crate::backend_availability::{self, BackendAvailability};

/// The owner of the session metadata slot holding the current member.
pub const OWNER: &str = "auto_backend";

/// The session metadata field holding the member the session's current turn runs on.
pub const AUTO_MEMBER_FIELD: &str = "auto_member";

/// How a pool picks the member a call runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PoolRule {
  /// The highest-priority selectable member: subscriptions below the 90% line, ordered by their weekly reset;
  /// subscriptions at or above the line as the last resort; pay-as-you-go in list order only when no subscription
  /// is selectable.
  Priority,
  /// The first selectable member after the rotation position, which advances past the chosen member.
  Rotate,
}

/// One member of a pool: the `backends.options` id and, for a priority pool, how it is paid for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolMember {
  /// The `backends.options` id, which is also the id [`BackendAvailability`] files "out until" under.
  pub backend: String,
  /// How a priority pool's member is paid for; `None` on a rotate pool, whose order alone decides.
  pub billing: Option<AutoBilling>,
}

/// One pool: the rule that picks its member and the members in their configured order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelPool {
  pub rule: PoolRule,
  pub members: Vec<PoolMember>,
}

/// The priority pool that `backends.auto` configures, or the refusal that names the missing section.
pub fn auto_pool(cfg: &CharlieBotConfig) -> anyhow::Result<ModelPool> {
  let auto = require_auto(cfg)?;
  Ok(ModelPool {
    rule: PoolRule::Priority,
    members: auto
      .members
      .iter()
      .map(|member| PoolMember { backend: member.backend.clone(), billing: Some(member.billing) })
      .collect(),
  })
}

/// The rotate pool that `backends.pool` configures.
pub fn rotation_pool(cfg: &CharlieBotConfig) -> ModelPool {
  ordered_pool(&cfg.backends.pool)
}

/// A rotate pool over `backends` in the caller's order, for a fixed route order that keeps no position of its own.
pub(crate) fn ordered_pool(backends: &[String]) -> ModelPool {
  ModelPool {
    rule: PoolRule::Rotate,
    members: backends.iter().map(|backend| PoolMember { backend: backend.clone(), billing: None }).collect(),
  }
}

/// Select the member a call runs on, or `None` when the pool has no selectable member.
///
/// A selectable member is one `excluded` does not name, that is not out per [`BackendAvailability`], and whose
/// usage reading does not say it is exhausted. A member whose backend type exposes no reading is selectable. A
/// [`PoolRule::Priority`] pool orders its selectable members; a [`PoolRule::Rotate`] pool walks from `position` in
/// member order and advances `position` past the member it selects, so `position` is required for a rotate pool.
pub fn select(
  cfg: &CharlieBotConfig,
  registry: &Registry,
  availability: &BackendAvailability,
  pool: &ModelPool,
  position: Option<&mut usize>,
  excluded: &[String],
  now: UtcDateTime,
) -> anyhow::Result<Option<PoolMember>> {
  match pool.rule {
    PoolRule::Priority => select_priority(cfg, registry, availability, pool, excluded, now),
    PoolRule::Rotate => {
      let position = position.ok_or_else(|| anyhow!("a rotate pool needs the rotation position to advance"))?;
      select_rotate(cfg, registry, availability, pool, position, excluded, now)
    }
  }
}

/// One selectable member with the reading that ordered it.
struct Candidate {
  member: PoolMember,
  reading: Option<MemberReading>,
}

/// The member as a candidate, or `None` when it cannot run now.
fn candidate(
  cfg: &CharlieBotConfig,
  registry: &Registry,
  availability: &BackendAvailability,
  member: &PoolMember,
  excluded: &[String],
  now: UtcDateTime,
) -> anyhow::Result<Option<Candidate>> {
  if excluded.iter().any(|backend_id| backend_id == &member.backend) {
    tracing::info!(target: "charliebot_agents", member = %member.backend, reason = "excluded by caller", "model_pool_skipped");
    return Ok(None);
  }
  if is_out(availability, &member.backend, now) {
    tracing::info!(
      target: "charliebot_agents",
      member = %member.backend,
      reason = %format!("cooling until {}", availability.out_until_minutes(&member.backend).unwrap_or_default()),
      "model_pool_skipped"
    );
    return Ok(None);
  }
  let reading = member_reading(cfg, registry, &member.backend, now)?;
  if reading.as_ref().is_some_and(|reading| reading.exhausted) {
    tracing::info!(target: "charliebot_agents", member = %member.backend, reason = "usage reading exhausted", "model_pool_skipped");
    return Ok(None);
  }
  Ok(Some(Candidate { member: member.clone(), reading }))
}

/// Pick from a priority pool: subscriptions before pay-as-you-go, under-line subscriptions first.
fn select_priority(
  cfg: &CharlieBotConfig,
  registry: &Registry,
  availability: &BackendAvailability,
  pool: &ModelPool,
  excluded: &[String],
  now: UtcDateTime,
) -> anyhow::Result<Option<PoolMember>> {
  let mut candidates = Vec::new();
  for member in &pool.members {
    if let Some(candidate) = candidate(cfg, registry, availability, member, excluded, now)? {
      candidates.push(candidate);
    }
  }

  let subscriptions = candidates
    .iter()
    .filter(|candidate| candidate.member.billing == Some(AutoBilling::Subscription))
    .collect::<Vec<_>>();
  if !subscriptions.is_empty() {
    let under_line = subscriptions
      .iter()
      .filter(|candidate| !near_exhaustion(candidate.reading.as_ref()))
      .copied()
      .collect::<Vec<_>>();
    let tier = if under_line.is_empty() { subscriptions } else { under_line };
    let mut with_reset = tier
      .iter()
      .filter_map(|candidate| candidate.reading.as_ref()?.weekly_resets_at.map(|reset| (reset, *candidate)))
      .collect::<Vec<_>>();
    with_reset.sort_by_key(|(reset, _)| *reset);
    if let Some((_, candidate)) = with_reset.first() {
      return Ok(Some(candidate.member.clone()));
    }
    return Ok(tier.first().map(|candidate| candidate.member.clone()));
  }

  Ok(
    candidates
      .iter()
      .find(|candidate| candidate.member.billing == Some(AutoBilling::Payg))
      .map(|candidate| candidate.member.clone()),
  )
}

/// Pick from a rotate pool: the first selectable member after `position`, then advance `position` past it.
fn select_rotate(
  cfg: &CharlieBotConfig,
  registry: &Registry,
  availability: &BackendAvailability,
  pool: &ModelPool,
  position: &mut usize,
  excluded: &[String],
  now: UtcDateTime,
) -> anyhow::Result<Option<PoolMember>> {
  let count = pool.members.len();
  if count == 0 {
    return Ok(None);
  }
  for offset in 0..count {
    let index = (*position + offset) % count;
    let Some(candidate) = candidate(cfg, registry, availability, &pool.members[index], excluded, now)? else {
      continue;
    };
    *position = (index + 1) % count;
    return Ok(Some(candidate.member));
  }
  Ok(None)
}

/// One member change a turn start made.
#[derive(Clone, Debug, PartialEq)]
pub struct MemberChange {
  /// The member the session left; empty for its first assignment, which records no change event.
  pub from: String,
  /// The member the session now runs on.
  pub to: String,
  /// Why the member changed: [`event_types::QUOTA_REASON_QUOTA_ERROR`],
  /// [`event_types::QUOTA_REASON_NEAR_EXHAUSTION`], [`event_types::QUOTA_REASON_SUBSCRIPTION_RESET`].
  pub reason: &'static str,
  /// The moment the member left recovers, in the minute-resolution event format.
  pub recovers_at: Option<String>,
  /// The handoff summary state of a change made mid-turn; a turn-start change carries none.
  pub summary: Option<String>,
}

impl MemberChange {
  /// A turn-start change, which builds no retry and so carries no handoff summary.
  fn at_turn_start(from: String, to: String, reason: &'static str, recovers_at: Option<String>) -> Self {
    Self { from, to, reason, recovers_at, summary: None }
  }

  /// The `quota_failover` chat event recording this change.
  pub fn event(&self) -> Value {
    let mut event = Map::from_iter([
      ("type".to_string(), json!(event_types::QUOTA_FAILOVER)),
      ("from".to_string(), json!(self.from)),
      ("to".to_string(), json!(self.to)),
      (event_types::QUOTA_FAILOVER_REASON.to_string(), json!(self.reason)),
      (event_types::QUOTA_FAILOVER_SET.to_string(), json!(event_types::QUOTA_SET_AUTO)),
      ("recovers_at".to_string(), json!(self.recovers_at.clone().unwrap_or_default())),
    ]);
    if let Some(summary) = &self.summary {
      event.insert(event_types::QUOTA_FAILOVER_SUMMARY.to_string(), json!(summary));
    }
    Value::Object(event)
  }
}

/// What a turn start decided for a session about to dispatch.
#[derive(Clone, Debug, PartialEq)]
pub enum TurnStart {
  /// The session keeps its member and dispatches no change event.
  Kept,
  /// The session switched members; the caller appends the change event.
  Changed(MemberChange),
  /// No member is selectable: the turn ends with this error listing every member.
  Exhausted(String),
}

/// Register the `auto_member` session slot.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.metadata_slot_with_validator(
    metadata_slot::MetadataSlot {
      owner: OWNER,
      on: metadata_slot::MetadataFile::Session,
      after: Some("backend"),
      fields: vec![metadata_slot::SlotField { name: AUTO_MEMBER_FIELD, default: Value::Null }],
    },
    validate_auto_member,
  )
}

fn validate_auto_member(values: &mut Map<String, Value>) -> Option<String> {
  values
    .get(AUTO_MEMBER_FIELD)
    .is_some_and(|value| !value.is_null() && !value.is_string())
    .then(|| format!("{AUTO_MEMBER_FIELD} must be a string or null"))
}

/// The configured Auto set, or the refusal that names the missing section.
pub fn require_auto(cfg: &CharlieBotConfig) -> anyhow::Result<&AutoConfig> {
  cfg.backends.auto.as_ref().ok_or_else(|| {
    anyhow!("session backend '{AUTO_BACKEND_ID}' requires config.yaml backends.auto, which is not configured")
  })
}

/// The backend option a member of the Auto set names, or the refusal that it names no `backends.options` id.
pub fn member_option<'a>(cfg: &'a CharlieBotConfig, member: &AutoMember) -> anyhow::Result<&'a BackendOption> {
  cfg
    .get_backend_option(&member.backend)
    .ok_or_else(|| anyhow!("backends.auto names backend '{}', which is not a backends.options id", member.backend))
}

/// Read one backend option's quota reading through its backend type's registered reader, or `None` when the type
/// exposes none.
pub(crate) fn member_reading(
  cfg: &CharlieBotConfig,
  registry: &Registry,
  backend_id: &str,
  now: UtcDateTime,
) -> anyhow::Result<Option<MemberReading>> {
  let option = cfg
    .get_backend_option(backend_id)
    .ok_or_else(|| anyhow!("pool member '{backend_id}' is not a backends.options id"))?;
  match registry.member_reading_for(&option.backend_type) {
    Some(read) => read(cfg, option, now),
    None => Ok(None),
  }
}

/// Whether a member is near exhaustion, as its own reading states it.
fn near_exhaustion(reading: Option<&MemberReading>) -> bool {
  reading.is_some_and(|reading| reading.near_exhaustion)
}

fn is_out(availability: &BackendAvailability, backend_id: &str, now: UtcDateTime) -> bool {
  availability.out_until(backend_id).is_some_and(|until| until > now.into_inner())
}

/// Select the member a turn of a session on `auto` runs on, or `None` when no member is selectable.
///
/// The Auto set is the priority pool that `backends.auto` configures, with no exclusion and no rotation position.
pub fn select_member(
  cfg: &CharlieBotConfig,
  registry: &Registry,
  availability: &BackendAvailability,
  excluded: &[String],
  now: UtcDateTime,
) -> anyhow::Result<Option<PoolMember>> {
  select(cfg, registry, availability, &auto_pool(cfg)?, None, excluded, now)
}

/// What a turn start decides for a session, before it records anything.
enum TurnDecision {
  /// The session keeps the member it records.
  Keep(String),
  /// The session moves to `member`, which the turn start records. `change` is the event to log; a first assignment
  /// logs none.
  Move { member: String, change: Option<MemberChange> },
  /// No member is selectable: the turn ends with this error.
  Exhausted(String),
}

/// Choose the member for a turn that is about to dispatch, record it, and return the change to log.
///
/// A session that is not on `auto` is left alone. A session with no recorded member, or whose member is out, gets a
/// fresh selection; the first assignment records the member without a change event. Otherwise the session keeps its
/// member unless it is on pay-as-you-go and a subscription is selectable (a subscription reset), or it is near
/// exhaustion and an under-line subscription is selectable. When no member is selectable the turn ends with the
/// error that lists every member.
pub async fn start_turn(
  features: &FeatureContext,
  cfg: &CharlieBotConfig,
  meta: &mut SessionMetadata,
) -> anyhow::Result<TurnStart> {
  if meta.backend != AUTO_BACKEND_ID {
    return Ok(TurnStart::Kept);
  }
  match decide_turn(features, cfg, meta)? {
    TurnDecision::Keep(_) => Ok(TurnStart::Kept),
    TurnDecision::Move { member, change } => {
      record_member(features, meta, &member).await?;
      Ok(change.map_or(TurnStart::Kept, TurnStart::Changed))
    }
    TurnDecision::Exhausted(message) => Ok(TurnStart::Exhausted(message)),
  }
}

/// The member a turn of the session runs on, as the turn start decides it, read without recording anything. The cron
/// host gate asks it before a fire, so the gate judges the member the fire then runs on. `None` for a session not on
/// `auto`, and when no member is selectable: the turn then ends with the exhausted error and runs nothing.
pub fn turn_member(
  features: &FeatureContext,
  cfg: &CharlieBotConfig,
  meta: &SessionMetadata,
) -> anyhow::Result<Option<String>> {
  if meta.backend != AUTO_BACKEND_ID {
    return Ok(None);
  }
  Ok(match decide_turn(features, cfg, meta)? {
    TurnDecision::Keep(member) | TurnDecision::Move { member, .. } => Some(member),
    TurnDecision::Exhausted(_) => None,
  })
}

/// The turn start's decision for `meta`, computed without recording it. [`start_turn`] applies it and [`turn_member`]
/// reads it, so both answer from the one selection path.
fn decide_turn(
  features: &FeatureContext,
  cfg: &CharlieBotConfig,
  meta: &SessionMetadata,
) -> anyhow::Result<TurnDecision> {
  let auto = require_auto(cfg)?;
  let availability = backend_availability::backend_availability(features)?;
  let now = UtcDateTime::now();
  let current = meta
    .extra(AUTO_MEMBER_FIELD)
    .and_then(Value::as_str)
    .filter(|backend_id| auto.member(backend_id).is_some())
    .map(str::to_string);

  if let Some(current) = &current
    && !is_out(&availability, current, now)
  {
    return decide_keep_or_switch(features, cfg, auto, &availability, current, now);
  }

  let Some(selected) = select_member(cfg, &features.registry, &availability, &[], now)? else {
    return Ok(TurnDecision::Exhausted(all_exhausted_message(features, cfg)?));
  };
  // The first assignment is not a change between members, so it leaves no failover event. A current member that is out
  // is never selected, so the selection always differs from it.
  let change = current.map(|from| {
    let recovers_at = availability.out_until_minutes(&from);
    MemberChange::at_turn_start(from, selected.backend.clone(), event_types::QUOTA_REASON_QUOTA_ERROR, recovers_at)
  });
  Ok(TurnDecision::Move { member: selected.backend, change })
}

/// Apply the turn-start rules 2 and 3 to a session whose current member is selectable.
fn decide_keep_or_switch(
  features: &FeatureContext,
  cfg: &CharlieBotConfig,
  auto: &AutoConfig,
  availability: &BackendAvailability,
  current: &str,
  now: UtcDateTime,
) -> anyhow::Result<TurnDecision> {
  let Some(selected) = select_member(cfg, &features.registry, availability, &[], now)? else {
    // The member is selectable by its out record but spent by its reading, and no other member is left.
    return Ok(TurnDecision::Exhausted(all_exhausted_message(features, cfg)?));
  };
  if selected.backend == current {
    return Ok(TurnDecision::Keep(selected.backend));
  }
  let current_member = auto.member(current).expect("the current member is in the configured set");
  let current_reading = member_reading(cfg, &features.registry, &current_member.backend, now)?;
  let selected_reading = member_reading(cfg, &features.registry, &selected.backend, now)?;
  let (reason, recovers_at) =
    if current_member.billing == AutoBilling::Payg && selected.billing == Some(AutoBilling::Subscription) {
      (event_types::QUOTA_REASON_SUBSCRIPTION_RESET, None)
    } else if current_reading.as_ref().is_some_and(|reading| reading.exhausted) {
      // The reading says the member has no capacity left; the turn start leaves it, as an out record would.
      let reset = current_reading.as_ref().and_then(MemberReading::earliest_reset).map(format_minutes);
      (event_types::QUOTA_REASON_QUOTA_ERROR, reset)
    } else if near_exhaustion(current_reading.as_ref())
      && selected.billing == Some(AutoBilling::Subscription)
      && !near_exhaustion(selected_reading.as_ref())
    {
      let reset = current_reading.as_ref().and_then(MemberReading::earliest_reset).map(format_minutes);
      (event_types::QUOTA_REASON_NEAR_EXHAUSTION, reset)
    } else {
      return Ok(TurnDecision::Keep(current.to_string()));
    };
  Ok(TurnDecision::Move {
    member: selected.backend.clone(),
    change: Some(MemberChange::at_turn_start(current.to_string(), selected.backend, reason, recovers_at)),
  })
}

/// The error that ends a turn when no member of the Auto set is selectable: one line per member, in config order. A
/// member's line names its billing, the reason it is out and when it recovers.
pub fn all_exhausted_message(features: &FeatureContext, cfg: &CharlieBotConfig) -> anyhow::Result<String> {
  let auto = require_auto(cfg)?;
  let availability = backend_availability::backend_availability(features)?;
  let now = UtcDateTime::now();
  let mut lines = Vec::new();
  for member in &auto.members {
    let state = match availability.row(&member.backend).filter(|row| row.out_until.into_inner() > now.into_inner()) {
      Some(row) => {
        let detail = row.detail.lines().next().unwrap_or_default().trim();
        let recovery = format!("{}, recovers at {}", row.reason, format_minutes(row.out_until));
        if detail.is_empty() { recovery } else { format!("{recovery} ({detail})") }
      }
      None => match member_reading(cfg, &features.registry, &member.backend, now)? {
        Some(reading) if reading.exhausted => reading.earliest_reset().map_or_else(
          || "quota exhausted".to_string(),
          |reset| format!("quota exhausted, resets at {}", format_minutes(reset)),
        ),
        _ => "not selectable".to_string(),
      },
    };
    lines.push(format!("- {} ({}): {state}", member.backend, billing_label(member.billing)));
  }
  Ok(format!("Auto set exhausted; no member is selectable:\n{}", lines.join("\n")))
}

fn billing_label(billing: AutoBilling) -> &'static str {
  match billing {
    AutoBilling::Subscription => "subscription",
    AutoBilling::Payg => "pay-as-you-go",
  }
}

/// Persist `member` as the session's current member and mirror it into the in-memory metadata the resolver reads.
async fn record_member(features: &FeatureContext, meta: &mut SessionMetadata, member: &str) -> anyhow::Result<()> {
  set_current_member(features, &meta.id, member).await?;
  meta.set_extra(AUTO_MEMBER_FIELD, json!(member));
  Ok(())
}

/// Persist `member` as the session's current member for a caller that holds no mutable metadata, as a mid-turn
/// switch does.
pub async fn set_current_member(features: &FeatureContext, session_id: &str, member: &str) -> anyhow::Result<()> {
  let values = Map::from_iter([(AUTO_MEMBER_FIELD.to_string(), json!(member))]);
  let updated = features.store.update_slot_fields(session_id, OWNER, values).await?;
  if updated.is_none() {
    return Err(anyhow!("session '{session_id}' disappeared while recording {AUTO_MEMBER_FIELD}"));
  }
  tracing::info!(target: "charliebot_agents", session_id, member, "auto_member_recorded");
  Ok(())
}

/// Render a reset in the minute-resolution form the pool events carry.
fn format_minutes(datetime: UtcDateTime) -> String {
  crate::backend_availability::format_minutes(datetime.into_inner())
}

/// The member a session on `auto` runs on now, for the resolver. Every turn records one before it runs, so a session
/// on `auto` with no recorded member is a refusal, and a recorded member the config no longer offers is too.
pub fn current_member_id(cfg: &CharlieBotConfig, meta: &SessionMetadata) -> anyhow::Result<String> {
  let auto = require_auto(cfg)?;
  let member = meta
    .extra(AUTO_MEMBER_FIELD)
    .and_then(Value::as_str)
    .filter(|backend_id| !backend_id.is_empty())
    .ok_or_else(|| {
      anyhow!(
        "session backend '{AUTO_BACKEND_ID}' has no recorded {AUTO_MEMBER_FIELD}; turn start records one before every run"
      )
    })?;
  if auto.member(member).is_none() {
    return Err(anyhow!(
      "session backend '{AUTO_BACKEND_ID}' records {AUTO_MEMBER_FIELD} '{member}', which config.yaml backends.auto no \
       longer lists"
    ));
  }
  Ok(member.to_string())
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use charliebot_core::config::BackendOption;
  use charliebot_core::credentials::load_credentials;
  use charliebot_core::hot_reload::HotReload;
  use charliebot_registry::backend::{
    AgentBackend, BackendFactory, BackendTraits, BackendTypeRegistration, LaunchArgs, MemberWindow, ResumeStyle,
  };
  use charliebot_registry::context::StartupState;
  use charliebot_registry::store_hooks;
  use charliebot_store::{CreateTask, Store, TaskCaller};
  use serde_json::Map;

  use super::*;

  struct NoLaunch;

  impl BackendFactory for NoLaunch {
    fn build(
      &self,
      _option: Option<&BackendOption>,
      _cfg: &CharlieBotConfig,
      _launch: LaunchArgs,
    ) -> anyhow::Result<Box<dyn AgentBackend>> {
      anyhow::bail!("the auto backend test backend is not launched")
    }
  }

  /// The Auto set's reading of a synthetic member: the canned windows the selection tests order.
  fn test_reading(option: &BackendOption, now: UtcDateTime) -> Option<MemberReading> {
    let reset = |minutes: i64| Some(UtcDateTime::from(now.into_inner() + time::Duration::minutes(minutes)));
    let weekly = |utilization: f64, minutes: i64| MemberReading {
      windows: vec![MemberWindow { utilization, window_minutes: Some(10_080), resets_at: reset(minutes) }],
      weekly_resets_at: reset(minutes),
      near_exhaustion: false,
      exhausted: false,
    };
    match option.id.as_str() {
      // 20% left, weekly reset in 6 hours.
      "chatgpt" => Some(weekly(0.80, 360)),
      // 51% left, weekly reset in 2 days.
      "claude-sub" => Some(weekly(0.49, 2_880)),
      // Near exhaustion: 95% used with a reset beyond the 45-minute horizon.
      "near" => Some(MemberReading { near_exhaustion: true, ..weekly(0.95, 180) }),
      // No capacity left, while the out record has not been written.
      "spent" => Some(MemberReading { exhausted: true, ..weekly(1.0, 60) }),
      // No reading: counts as under the line until its first quota error.
      _ => None,
    }
  }

  fn register_test(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.backend_type(BackendTypeRegistration {
      type_name: "synthetic",
      traits: BackendTraits {
        resume: ResumeStyle::CliFlag,
        restart_reattach: false,
        preassigned_session_id: false,
        family_prefix: None,
        manual_compact: false,
        compact_hint: None,
      },
      option_model: |_| Ok(()),
      factory: Arc::new(NoLaunch),
      lifecycle: None,
      translate_fallback: false,
    })?;
    registry.member_reading("synthetic", Arc::new(|_cfg, option, now| Ok(test_reading(option, now))))?;
    register(registry)
  }

  const CONFIG: &str = "\
backends:
  options:
    - {id: chatgpt, label: ChatGPT, type: synthetic, model: model-gpt}
    - {id: claude-sub, label: Claude, type: synthetic, model: model-claude}
    - {id: near, label: Near, type: synthetic, model: model-near}
    - {id: spent, label: Spent, type: synthetic, model: model-spent}
    - {id: devin, label: Devin, type: synthetic, model: model-devin}
    - {id: payg-a, label: Payg A, type: synthetic, model: model-payg}
  pool: [spent, payg-a, chatgpt]
  auto:
    label: Auto
    members:
      - {backend: chatgpt, billing: subscription}
      - {backend: claude-sub, billing: subscription}
      - {backend: near, billing: subscription}
      - {backend: spent, billing: subscription}
      - {backend: devin, billing: subscription}
      - {backend: payg-a, billing: payg}
";

  fn fixture() -> (tempfile::TempDir, Arc<FeatureContext>) {
    fixture_with(CONFIG)
  }

  fn fixture_with(config_body: &str) -> (tempfile::TempDir, Arc<FeatureContext>) {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("config.yaml"), config_body).unwrap();
    std::fs::write(home.path().join("credentials.yaml"), "charliebot: {}\n").unwrap();
    let mut registry = Registry::new();
    registry.register_package("test.model_pool", register_test).unwrap();
    let registry = Arc::new(registry);
    let cfg = registry.load_config(home.path()).unwrap();
    let store = Arc::new(
      Store::new(cfg.clone(), registry.metadata_slots(), store_hooks::store_hooks(Arc::clone(&registry))).unwrap(),
    );
    let config_file = cfg.config_file();
    let config =
      Arc::new(HotReload::new(config_file, "model_pool_test_config_reload_failed", Box::new(move || Ok(cfg.clone()))));
    let credential_home = home.path().to_path_buf();
    let credentials = Arc::new(HotReload::new(
      home.path().join("credentials.yaml"),
      "model_pool_test_credentials_reload_failed",
      Box::new(move || load_credentials(&credential_home)),
    ));
    let startup = Arc::new(StartupState::default());
    startup.insert(BackendAvailability::load(home.path()).unwrap()).unwrap();
    (home, Arc::new(FeatureContext { config, credentials, store, registry, startup }))
  }

  fn loaded_cfg(features: &FeatureContext) -> Arc<CharlieBotConfig> {
    features.config.get().unwrap()
  }

  async fn auto_session(features: &FeatureContext, request_id: &str) -> SessionMetadata {
    features
      .store
      .create_task(
        CreateTask {
          request_id: request_id.to_string(),
          task_parent_id: None,
          profile: charliebot_store::metadata::TaskProfile::Manager,
          task: None,
          name: Some("Auto session".to_string()),
          backend: Some(AUTO_BACKEND_ID.to_string()),
          group: None,
          session_id: None,
        },
        TaskCaller::System,
        Map::new(),
      )
      .await
      .unwrap()
  }

  fn availability(features: &FeatureContext) -> Arc<BackendAvailability> {
    backend_availability::backend_availability(features).unwrap()
  }

  fn changed(start: TurnStart) -> MemberChange {
    match start {
      TurnStart::Changed(change) => change,
      other => panic!("expected a member change, got {other:?}"),
    }
  }

  fn rotate_select(
    cfg: &CharlieBotConfig,
    features: &FeatureContext,
    availability: &BackendAvailability,
    position: &mut usize,
    excluded: &[&str],
    now: UtcDateTime,
  ) -> Option<String> {
    let excluded = excluded.iter().map(|backend_id| (*backend_id).to_string()).collect::<Vec<_>>();
    select(cfg, &features.registry, availability, &rotation_pool(cfg), Some(position), &excluded, now)
      .unwrap()
      .map(|member| member.backend)
  }

  #[tokio::test]
  async fn selection_orders_by_weekly_reset_then_config_order_then_pay_as_you_go() {
    let (_home, features) = fixture();
    let cfg = loaded_cfg(&features);
    let availability = availability(&features);
    let now = UtcDateTime::now();
    let pool = auto_pool(&cfg).unwrap();
    let pick = |excluded: &[&str]| {
      let excluded = excluded.iter().map(|id| (*id).to_string()).collect::<Vec<_>>();
      select(&cfg, &features.registry, &availability, &pool, None, &excluded, now).unwrap().map(|member| member.backend)
    };

    // Trade-off 3's example: ChatGPT (20% left, 6h), Claude (51%, 2 days), Devin (no reading).
    assert_eq!(pick(&[]), Some("chatgpt".to_string()));
    assert_eq!(pick(&["chatgpt"]), Some("claude-sub".to_string()));
    assert_eq!(pick(&["chatgpt", "claude-sub"]), Some("devin".to_string()));
    // A subscription at or above the line is the last resort, before the first pay-as-you-go member.
    assert_eq!(pick(&["chatgpt", "claude-sub", "devin"]), Some("near".to_string()));
    assert_eq!(pick(&["chatgpt", "claude-sub", "devin", "near"]), Some("payg-a".to_string()));
    assert_eq!(pick(&["chatgpt", "claude-sub", "devin", "near", "payg-a"]), None);
  }

  #[tokio::test]
  async fn an_out_member_is_skipped_and_an_exhausted_reading_is_skipped() {
    let (_home, features) = fixture();
    let cfg = loaded_cfg(&features);
    let availability = availability(&features);
    let now = UtcDateTime::now();
    availability.record("chatgpt", None, backend_availability::REASON_QUOTA_ERROR, "spent").unwrap();
    let selected = select_member(&cfg, &features.registry, &availability, &[], now).unwrap().unwrap();
    assert_eq!(selected.backend, "claude-sub");
  }

  #[tokio::test]
  async fn rotate_skips_excluded_out_and_exhausted_members_and_keeps_one_without_a_reading() {
    let (_home, features) = fixture();
    let cfg = loaded_cfg(&features);
    let availability = availability(&features);
    let now = UtcDateTime::now();

    // `spent`'s reading says it is exhausted; `payg-a` exposes no reading and is selectable.
    let mut position = 0;
    assert_eq!(rotate_select(&cfg, &features, &availability, &mut position, &[], now), Some("payg-a".to_string()));
    assert_eq!(position, 2, "the position advances past the chosen member");

    let mut position = 0;
    assert_eq!(
      rotate_select(&cfg, &features, &availability, &mut position, &["payg-a"], now),
      Some("chatgpt".to_string()),
      "a member the caller excludes is skipped"
    );

    availability.record("payg-a", None, backend_availability::REASON_QUOTA_ERROR, "spent").unwrap();
    let mut position = 0;
    assert_eq!(
      rotate_select(&cfg, &features, &availability, &mut position, &[], now),
      Some("chatgpt".to_string()),
      "a member the owner recorded out is skipped"
    );

    availability.record("chatgpt", None, backend_availability::REASON_QUOTA_ERROR, "spent").unwrap();
    let mut position = 0;
    assert_eq!(rotate_select(&cfg, &features, &availability, &mut position, &[], now), None);
  }

  #[tokio::test]
  async fn a_member_both_pools_share_is_skipped_by_both_once_recorded_out() {
    let (_home, features) = fixture();
    let cfg = loaded_cfg(&features);
    let availability = availability(&features);
    let now = UtcDateTime::now();
    // `chatgpt` is a member of the priority set and of the rotate pool; one out record covers both.
    availability.record("chatgpt", None, backend_availability::REASON_QUOTA_ERROR, "spent").unwrap();

    let selected = select_member(&cfg, &features.registry, &availability, &[], now).unwrap().unwrap();
    assert_eq!(selected.backend, "claude-sub");

    let mut position = 2;
    let selected = select(&cfg, &features.registry, &availability, &rotation_pool(&cfg), Some(&mut position), &[], now)
      .unwrap()
      .unwrap();
    assert_eq!(selected.backend, "payg-a", "the rotate rule skips the member the priority rule recorded out");
  }

  #[tokio::test]
  async fn turn_start_records_the_first_member_without_a_change_event_and_keeps_it_below_the_line() {
    let (_home, features) = fixture();
    let cfg = loaded_cfg(&features);
    let mut meta = auto_session(&features, "auto-first-turn").await;

    let change = start_turn(&features, &cfg, &mut meta).await.unwrap();
    assert_eq!(change, TurnStart::Kept, "the first assignment is not a change between members");
    assert_eq!(meta.extra(AUTO_MEMBER_FIELD), Some(&json!("chatgpt")));
    let stored = features.store.read_metadata_fresh(&meta.id).unwrap().unwrap();
    assert_eq!(stored.extra(AUTO_MEMBER_FIELD), Some(&json!("chatgpt")));
    assert!(features.store.load_all_facts(&meta.id).unwrap().iter().all(|event| event["type"] != "quota_failover"));

    // A member below the 90% line is kept at the next turn start.
    let change = start_turn(&features, &cfg, &mut meta).await.unwrap();
    assert_eq!(change, TurnStart::Kept);
    assert_eq!(meta.extra(AUTO_MEMBER_FIELD), Some(&json!("chatgpt")));
  }

  #[tokio::test]
  async fn turn_start_switches_off_an_out_member_with_a_quota_error_change() {
    let (_home, features) = fixture();
    let cfg = loaded_cfg(&features);
    let mut meta = auto_session(&features, "auto-out-member").await;
    start_turn(&features, &cfg, &mut meta).await.unwrap();
    availability(&features).record("chatgpt", None, backend_availability::REASON_QUOTA_ERROR, "spent").unwrap();

    let change = changed(start_turn(&features, &cfg, &mut meta).await.unwrap());
    assert_eq!((change.from.as_str(), change.to.as_str()), ("chatgpt", "claude-sub"));
    assert_eq!(change.reason, event_types::QUOTA_REASON_QUOTA_ERROR);
    assert!(change.recovers_at.is_some());
    let event = change.event();
    assert_eq!(event["type"], "quota_failover");
    assert_eq!(event["from"], "chatgpt");
    assert_eq!(event["to"], "claude-sub");
    assert_eq!(event["reason"], "quota_error");
    assert_eq!(event["set"], "auto");
    assert_eq!(meta.extra(AUTO_MEMBER_FIELD), Some(&json!("claude-sub")));
  }

  #[tokio::test]
  async fn turn_start_returns_to_a_subscription_after_it_resets() {
    let (_home, features) = fixture();
    let cfg = loaded_cfg(&features);
    let mut meta = auto_session(&features, "auto-subscription-reset").await;
    let availability = availability(&features);
    for member in ["chatgpt", "claude-sub", "near", "devin"] {
      availability.record(member, None, backend_availability::REASON_QUOTA_ERROR, "spent").unwrap();
    }
    start_turn(&features, &cfg, &mut meta).await.unwrap();
    assert_eq!(meta.extra(AUTO_MEMBER_FIELD), Some(&json!("payg-a")));

    // The subscriptions reset (an expired row): the next turn start returns to the first one.
    let expired = UtcDateTime::from(time::OffsetDateTime::now_utc() - time::Duration::hours(1));
    for member in ["chatgpt", "claude-sub", "near", "devin"] {
      availability.record(member, Some(expired), backend_availability::REASON_QUOTA_ERROR, "spent").unwrap();
    }
    let change = changed(start_turn(&features, &cfg, &mut meta).await.unwrap());
    assert_eq!((change.from.as_str(), change.to.as_str()), ("payg-a", "chatgpt"));
    assert_eq!(change.reason, event_types::QUOTA_REASON_SUBSCRIPTION_RESET);
    assert_eq!(change.event()["reason"], "subscription_reset");
    assert_eq!(meta.extra(AUTO_MEMBER_FIELD), Some(&json!("chatgpt")));
  }

  #[tokio::test]
  async fn turn_start_leaves_a_near_exhausted_member_for_an_under_line_subscription() {
    let (_home, features) = fixture();
    let cfg = loaded_cfg(&features);
    let mut meta = auto_session(&features, "auto-near-exhaustion").await;
    features
      .store
      .update_slot_fields(&meta.id, OWNER, Map::from_iter([(AUTO_MEMBER_FIELD.to_string(), json!("near"))]))
      .await
      .unwrap();
    meta.set_extra(AUTO_MEMBER_FIELD, json!("near"));

    let change = changed(start_turn(&features, &cfg, &mut meta).await.unwrap());
    assert_eq!((change.from.as_str(), change.to.as_str()), ("near", "chatgpt"));
    assert_eq!(change.reason, event_types::QUOTA_REASON_NEAR_EXHAUSTION);
    assert!(change.recovers_at.is_some());
    assert_eq!(change.event()["reason"], "near_exhaustion");
  }

  #[tokio::test]
  async fn turn_start_leaves_a_member_whose_reading_says_it_is_spent() {
    let (_home, features) = fixture();
    let cfg = loaded_cfg(&features);
    let mut meta = auto_session(&features, "auto-spent-reading").await;
    features
      .store
      .update_slot_fields(&meta.id, OWNER, Map::from_iter([(AUTO_MEMBER_FIELD.to_string(), json!("spent"))]))
      .await
      .unwrap();
    meta.set_extra(AUTO_MEMBER_FIELD, json!("spent"));

    let change = changed(start_turn(&features, &cfg, &mut meta).await.unwrap());
    assert_eq!((change.from.as_str(), change.to.as_str()), ("spent", "chatgpt"));
    assert_eq!(change.reason, event_types::QUOTA_REASON_QUOTA_ERROR);
    assert_eq!(meta.extra(AUTO_MEMBER_FIELD), Some(&json!("chatgpt")));
  }

  #[tokio::test]
  async fn an_exhausted_set_ends_the_turn_with_one_line_per_member() {
    let (_home, features) = fixture();
    let cfg = loaded_cfg(&features);
    let availability = availability(&features);
    let mut meta = auto_session(&features, "auto-exhausted").await;
    let reset = UtcDateTime::parse("2999-01-02T03:04:05Z").unwrap();
    for member in ["chatgpt", "claude-sub", "near", "devin", "payg-a"] {
      availability.record(member, Some(reset), backend_availability::REASON_QUOTA_ERROR, "API Error: 402").unwrap();
    }

    let start = start_turn(&features, &cfg, &mut meta).await.unwrap();
    let TurnStart::Exhausted(message) = start else { panic!("expected the exhausted turn, got {start:?}") };
    for (member, billing) in [
      ("chatgpt", "subscription"),
      ("claude-sub", "subscription"),
      ("near", "subscription"),
      ("devin", "subscription"),
      ("payg-a", "pay-as-you-go"),
    ] {
      assert!(
        message.contains(&format!("- {member} ({billing}): quota_error, recovers at 2999-01-02T03:04+00:00")),
        "{message}"
      );
    }
    assert_eq!(meta.extra(AUTO_MEMBER_FIELD), Some(&Value::Null), "an exhausted turn records no member");
  }

  #[tokio::test]
  async fn the_resolver_reads_the_recorded_member_and_refuses_a_missing_set() {
    let (_home, features) = fixture();
    let cfg = loaded_cfg(&features);
    let mut meta = auto_session(&features, "auto-resolver").await;
    let error = current_member_id(&cfg, &meta).unwrap_err().to_string();
    assert!(error.contains("no recorded auto_member"), "{error}");

    start_turn(&features, &cfg, &mut meta).await.unwrap();
    assert_eq!(current_member_id(&cfg, &meta).unwrap(), "chatgpt");

    let without_auto =
      fixture_with("backends:\n  options:\n    - {id: chatgpt, label: ChatGPT, type: synthetic, model: model-gpt}\n");
    let without_auto_cfg = loaded_cfg(&without_auto.1);
    let error = current_member_id(&without_auto_cfg, &meta).unwrap_err().to_string();
    assert!(error.contains("backends.auto"), "{error}");
  }

  #[tokio::test]
  async fn turn_member_is_the_member_a_turn_start_records_and_a_read_records_nothing() {
    let (_home, features) = fixture();
    let cfg = loaded_cfg(&features);
    let mut meta = auto_session(&features, "auto-turn-member").await;
    assert_eq!(turn_member(&features, &cfg, &meta).unwrap().as_deref(), Some("chatgpt"));
    let stored = features.store.read_metadata_fresh(&meta.id).unwrap().unwrap();
    assert_eq!(stored.extra(AUTO_MEMBER_FIELD).and_then(Value::as_str), None, "a read records no member");

    start_turn(&features, &cfg, &mut meta).await.unwrap();
    assert_eq!(current_member_id(&cfg, &meta).unwrap(), "chatgpt");

    availability(&features).record("chatgpt", None, backend_availability::REASON_QUOTA_ERROR, "spent").unwrap();
    assert_eq!(turn_member(&features, &cfg, &meta).unwrap().as_deref(), Some("claude-sub"));
    let stored = features.store.read_metadata_fresh(&meta.id).unwrap().unwrap();
    assert_eq!(stored.extra(AUTO_MEMBER_FIELD), Some(&json!("chatgpt")), "a read leaves the recorded member in place");

    let change = changed(start_turn(&features, &cfg, &mut meta).await.unwrap());
    assert_eq!((change.from.as_str(), change.to.as_str()), ("chatgpt", "claude-sub"));
    assert_eq!(current_member_id(&cfg, &meta).unwrap(), "claude-sub");
  }
}
