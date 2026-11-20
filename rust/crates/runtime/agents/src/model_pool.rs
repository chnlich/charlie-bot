//! The model pool: the one resolver for a backend id, whether it names an option or a configured pool.
//!
//! A pool names `backends.options` ids and a rule. A priority pool takes its subscription members in turn, and takes
//! its pay-as-you-go members in turn only when no subscription member is selectable. A rotate pool takes its members in
//! turn. Both rules skip a member the caller excludes, that [`BackendAvailability`] holds out, or whose usage reading
//! says it is exhausted. A priority pool also skips a subscription member whose reading says it is fully reserved —
//! every login that has headroom has all of it reserved by the runs placed on it — when a pay-as-you-go member is
//! selectable, so the pool runs those runs on the pay-as-you-go member instead. A member one pool recorded out is
//! skipped by every pool that names it. A session whose backend
//! is a pool id runs each turn on the member this module picks, and records that member in its `pool_member` slot.
//!
//! Terms: a *member* is a `backends.options` id that a pool names. *Out* is the "out until" record that
//! [`BackendAvailability`] keeps per member. A *lane* is one track of a pool's turns: a rotate pool has one, and a
//! priority pool has one per billing tier. A *position* is a lane's place in its pool's member list, kept in process
//! memory, one per pool id and lane.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::anyhow;
use charliebot_core::config::{BackendOption, CharlieBotConfig, PoolBilling, PoolConfig, PoolMember, PoolRule};
use charliebot_core::{event_types, metadata_slot};
use charliebot_registry::backend::MemberReading;
use charliebot_registry::context::FeatureContext;
use charliebot_registry::{Registry, RegistryError};
use charliebot_store::metadata::{SessionMetadata, UtcDateTime};
use serde_json::{Map, Value, json};

use crate::backend_availability::{self, BackendAvailability};

/// The owner of the session metadata slot holding the member a pool session runs on.
pub const OWNER: &str = "model_pool";

/// The session metadata field holding the member a pool session's current turn runs on.
pub const POOL_MEMBER_FIELD: &str = "pool_member";

/// One pool as the selection reads it: its id, its rule and its members in config order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelPool {
  pub id: String,
  pub rule: PoolRule,
  pub members: Vec<PoolMember>,
}

/// What one selection reads: the config, the registry whose usage readings it consults, the availability owner, the
/// pool, the ids the caller excludes, and the moment it reads at.
pub struct Selection<'a> {
  cfg: &'a CharlieBotConfig,
  registry: &'a Registry,
  availability: &'a BackendAvailability,
  pool: &'a ModelPool,
  excluded: &'a [String],
  now: UtcDateTime,
}

impl<'a> Selection<'a> {
  /// The selection of `pool` at `now`, leaving out the members `excluded` names.
  pub fn new(
    cfg: &'a CharlieBotConfig,
    registry: &'a Registry,
    availability: &'a BackendAvailability,
    pool: &'a ModelPool,
    excluded: &'a [String],
    now: UtcDateTime,
  ) -> Self {
    Self { cfg, registry, availability, pool, excluded, now }
  }
}

/// The one selection every caller makes: the process-wide owners a member pick reads and the positions it moves.
///
/// A caller with a [`FeatureContext`] builds one with [`Selector::from_features`]. A caller that holds only the
/// process-wide owners (a CLI one-shot call) builds one with [`Selector::new`].
pub struct Selector<'a> {
  cfg: &'a CharlieBotConfig,
  registry: &'a Registry,
  availability: Arc<BackendAvailability>,
  positions: Arc<RotationPositions>,
}

impl<'a> Selector<'a> {
  /// A selection over the given owners.
  pub fn new(
    cfg: &'a CharlieBotConfig,
    registry: &'a Registry,
    availability: Arc<BackendAvailability>,
    positions: Arc<RotationPositions>,
  ) -> Self {
    Self { cfg, registry, availability, positions }
  }

  /// The selection over the owners a server process holds in its startup state.
  pub fn from_features(features: &'a FeatureContext, cfg: &'a CharlieBotConfig) -> anyhow::Result<Self> {
    Ok(Self {
      cfg,
      registry: &features.registry,
      availability: backend_availability::backend_availability(features)?,
      positions: features.startup.require::<RotationPositions>()?,
    })
  }

  /// The option a call on `backend_id` runs on, or `None` when no member is selectable. A model id selects itself
  /// through the same filters as a one-member rotate pool. The selected member moves its lane's position past it.
  pub fn select(&self, backend_id: &str, excluded: &[String]) -> anyhow::Result<Option<BackendOption>> {
    let pool = selection_pool_for_id(self.cfg, backend_id)?;
    let selection = Selection::new(self.cfg, self.registry, &self.availability, &pool, excluded, UtcDateTime::now());
    Ok(self.option(self.positions.select(&selection)?))
  }

  /// The option the next call on `backend_id` would run on, without moving any position.
  pub fn peek(&self, backend_id: &str, excluded: &[String]) -> anyhow::Result<Option<BackendOption>> {
    let pool = selection_pool_for_id(self.cfg, backend_id)?;
    let selection = Selection::new(self.cfg, self.registry, &self.availability, &pool, excluded, UtcDateTime::now());
    Ok(self.option(self.positions.peek(&selection)?))
  }

  /// The first selectable option in the router's ordered backend ids. Each id may name a model or pool.
  pub fn ordered(&self, backends: &[String], excluded: &[String]) -> anyhow::Result<Option<BackendOption>> {
    for backend_id in backends {
      if let Some(option) = self.select(backend_id, excluded)? {
        return Ok(Some(option));
      }
    }
    Ok(None)
  }

  /// The error that ends a call when no member of `backend_id` is selectable: one line per member.
  pub fn exhausted_message(&self, backend_id: &str) -> anyhow::Result<String> {
    let pool = selection_pool_for_id(self.cfg, backend_id)?;
    all_exhausted_message(self.cfg, self.registry, &self.availability, &pool)
  }

  /// The current availability reason for one concrete backend option.
  pub fn member_state_reason(&self, backend_id: &str) -> anyhow::Result<String> {
    member_state_reason(self.cfg, self.registry, &self.availability, backend_id, UtcDateTime::now())
  }

  /// The option a selected member names. Every member is checked against `backends.options` at selection.
  fn option(&self, member: Option<PoolMember>) -> Option<BackendOption> {
    member.map(|member| {
      self.cfg.get_backend_option(&member.backend).expect("a pool selection returns a backends.options id").clone()
    })
  }
}

impl ModelPool {
  /// The pool a `backends.pools` entry configures.
  pub fn from_config(pool: &PoolConfig) -> Self {
    Self { id: pool.id.clone(), rule: pool.rule, members: pool.members.clone() }
  }

  /// The member naming `backend_id`, or `None` when the pool holds none.
  pub fn member(&self, backend_id: &str) -> Option<&PoolMember> {
    self.members.iter().find(|member| member.backend == backend_id)
  }
}

/// The candidate set for a backend id: its configured pool, or the one model it names.
fn selection_pool_for_id(cfg: &CharlieBotConfig, id: &str) -> anyhow::Result<ModelPool> {
  cfg.backends.check_backend_id(id).map_err(anyhow::Error::msg)?;
  if let Some(pool) = cfg.backends.pool(id) {
    return Ok(ModelPool::from_config(pool));
  }
  let option = cfg.get_backend_option(id).expect("a valid non-pool backend id names a backends.options entry");
  Ok(ModelPool { id: id.to_string(), rule: PoolRule::Rotate, members: vec![PoolMember { backend: option.id.clone() }] })
}

/// One track of a pool's turns, with its own position. A rotate pool has one lane. A priority pool has one per billing
/// tier, so a turn on one tier leaves the other tier's position where it was.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Lane {
  /// A rotate pool's only lane.
  Rotate,
  /// A priority pool's subscription tier.
  Subscription,
  /// A priority pool's pay-as-you-go tier.
  Payg,
}

/// Where a lane's position starts when a process first reads it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LaneStart {
  /// The first member in config order. The server starts here, so its callers share one rotation from the list head.
  #[default]
  Head,
  /// A priority tier's first member. A CLI process starts each priority tier here, so separate processes do not all
  /// begin on the first member.
  Random,
}

/// The member a selection picked, the lane it belongs to, and the position that lane moves to once the pick is taken.
pub(crate) struct Pick {
  member: PoolMember,
  lane: Lane,
  next: usize,
}

/// The rotation positions, one per pool id and lane. A position lives in process memory: a restart or a new CLI process
/// starts each lane again from its [`LaneStart`].
#[derive(Default)]
pub struct RotationPositions {
  positions: Mutex<HashMap<(String, Lane), usize>>,
  start: LaneStart,
}

impl RotationPositions {
  /// Positions whose lanes start from `start` when a process first reads them.
  pub fn new(start: LaneStart) -> Self {
    Self { positions: Mutex::default(), start }
  }

  /// Select the member `selection`'s pool runs a call on and move the lane's position past it. The position is read
  /// under the lock, the selection reads usage without the lock, and the advanced position is written under the lock in
  /// one step.
  pub fn select(&self, selection: &Selection<'_>) -> anyhow::Result<Option<PoolMember>> {
    let pool_id = &selection.pool.id;
    let Some(pick) = select(selection, |lane| self.position(pool_id, lane))? else {
      return Ok(None);
    };
    self.set_position(pool_id, pick.lane, pick.next);
    Ok(Some(pick.member))
  }

  /// The member `select` would pick now, without moving any position. A route check and a turn read use it, so reading
  /// a member leaves the rotation where it was.
  pub fn peek(&self, selection: &Selection<'_>) -> anyhow::Result<Option<PoolMember>> {
    Ok(select(selection, |lane| self.position(&selection.pool.id, lane))?.map(|pick| pick.member))
  }

  fn position(&self, pool_id: &str, lane: Lane) -> usize {
    let mut positions = self.positions.lock().expect("rotation position mutex is not poisoned");
    *positions.entry((pool_id.to_string(), lane)).or_insert_with(|| self.first_position(lane))
  }

  fn first_position(&self, lane: Lane) -> usize {
    match (self.start, lane) {
      (LaneStart::Head, _) | (_, Lane::Rotate) => 0,
      (LaneStart::Random, Lane::Subscription | Lane::Payg) => {
        let mut bytes = [0u8; size_of::<usize>()];
        getrandom::fill(&mut bytes).expect("the OS random source is available");
        usize::from_ne_bytes(bytes)
      }
    }
  }

  fn set_position(&self, pool_id: &str, lane: Lane, position: usize) {
    let mut positions = self.positions.lock().expect("rotation position mutex is not poisoned");
    positions.insert((pool_id.to_string(), lane), position);
  }
}

/// Select the member a call runs on, or `None` when the pool has no selectable member.
///
/// A selectable member is one `excluded` does not name, that is not out per [`BackendAvailability`], and whose usage
/// reading does not say it is exhausted. A member whose backend type exposes no reading is selectable. `position` reads
/// the position of a lane; the pick names the lane it belongs to and the position that lane moves to.
pub(crate) fn select(selection: &Selection<'_>, position: impl FnOnce(Lane) -> usize) -> anyhow::Result<Option<Pick>> {
  let Selection { cfg, registry, availability, pool, excluded, now } = *selection;
  match pool.rule {
    PoolRule::Priority => select_priority(cfg, registry, availability, pool, excluded, now, position),
    PoolRule::Rotate => select_rotate(cfg, registry, availability, pool, position(Lane::Rotate), excluded, now),
  }
}

/// One selectable member and its quota reading.
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

/// Pick from a priority pool. The subscription tier comes first: its members below the 90% line, or every selectable
/// subscription when none is below it. The pay-as-you-go tier is picked from only when no subscription is selectable.
/// The pick walks the chosen tier's lane from that lane's position.
fn select_priority(
  cfg: &CharlieBotConfig,
  registry: &Registry,
  availability: &BackendAvailability,
  pool: &ModelPool,
  excluded: &[String],
  now: UtcDateTime,
  position: impl FnOnce(Lane) -> usize,
) -> anyhow::Result<Option<Pick>> {
  let candidates = pool
    .members
    .iter()
    .map(|member| candidate(cfg, registry, availability, member, excluded, now))
    .collect::<anyhow::Result<Vec<_>>>()?;
  let reading = |index: usize| candidates[index].as_ref().and_then(|candidate| candidate.reading.as_ref());
  let selectable = |billing: PoolBilling| {
    (0..pool.members.len())
      .filter(|&index| candidates[index].is_some() && member_billing(cfg, &pool.members[index]) == Some(billing))
      .collect::<Vec<_>>()
  };

  let subscriptions = selectable(PoolBilling::Subscription);
  let payg = selectable(PoolBilling::Payg);
  // A subscription whose every login with headroom has all of it reserved by the runs placed on it yields to the
  // pay-as-you-go tier, but only while a pay-as-you-go member can take the run: the reservation never refuses one.
  let reserved = subscriptions
    .iter()
    .copied()
    .filter(|&index| reading(index).is_some_and(|member| member.fully_reserved))
    .collect::<Vec<_>>();
  let (tier, lane) = if subscriptions.is_empty() {
    (payg, Lane::Payg)
  } else if !reserved.is_empty() && !payg.is_empty() {
    for index in &reserved {
      tracing::info!(target: "charliebot_agents", member = %pool.members[*index].backend, reason = "headroom reserved", "model_pool_skipped");
    }
    let open = subscriptions.iter().copied().filter(|index| !reserved.contains(index)).collect::<Vec<_>>();
    if open.is_empty() {
      (payg, Lane::Payg)
    } else {
      let under_line = open.iter().copied().filter(|&index| !near_exhaustion(reading(index))).collect::<Vec<_>>();
      (if under_line.is_empty() { open } else { under_line }, Lane::Subscription)
    }
  } else {
    let under_line =
      subscriptions.iter().copied().filter(|&index| !near_exhaustion(reading(index))).collect::<Vec<_>>();
    (if under_line.is_empty() { subscriptions } else { under_line }, Lane::Subscription)
  };
  if tier.is_empty() {
    return Ok(None);
  }
  Ok(pick_in_tier(pool, &tier, lane, position(lane)))
}

/// The first member of `tier` that the walk from `start` reaches. The pick moves the lane past it.
fn pick_in_tier(pool: &ModelPool, tier: &[usize], lane: Lane, start: usize) -> Option<Pick> {
  let count = pool.members.len();
  walk(count, start).find(|index| tier.contains(index)).map(|index| Pick {
    member: pool.members[index].clone(),
    lane,
    next: (index + 1) % count,
  })
}

/// Pick from a rotate pool: the first selectable member the walk from `start` reaches. The pick moves the lane past it.
fn select_rotate(
  cfg: &CharlieBotConfig,
  registry: &Registry,
  availability: &BackendAvailability,
  pool: &ModelPool,
  start: usize,
  excluded: &[String],
  now: UtcDateTime,
) -> anyhow::Result<Option<Pick>> {
  let count = pool.members.len();
  if count == 0 {
    return Ok(None);
  }
  for index in walk(count, start) {
    let Some(candidate) = candidate(cfg, registry, availability, &pool.members[index], excluded, now)? else {
      continue;
    };
    return Ok(Some(Pick { member: candidate.member, lane: Lane::Rotate, next: (index + 1) % count }));
  }
  Ok(None)
}

/// The member indexes a walk visits: `start` first, then each later index in member order, wrapping once around the
/// list. A walk takes a list of at least one member.
fn walk(count: usize, start: usize) -> impl Iterator<Item = usize> {
  let first = start % count;
  (0..count).map(move |offset| (first + offset) % count)
}

/// One member change a turn start made.
#[derive(Clone, Debug, PartialEq)]
pub struct MemberChange {
  /// The pool whose member changed: the `quota_failover` event's `set`.
  pub pool: String,
  /// The member the session left. Empty for its first assignment, which records no change event.
  pub from: String,
  /// The member the session now runs on.
  pub to: String,
  /// Why the member changed: one of [`backend_availability::RECORDED_REASONS`], or a reading's own
  /// near-exhaustion or subscription-reset reason.
  pub reason: String,
  /// The moment the member left recovers, in the minute-resolution event format.
  pub recovers_at: Option<String>,
  /// The handoff summary state of the retry a mid-turn change started. A turn-start change leaves it `None`: its
  /// summary is recorded in its event instead.
  pub summary: Option<String>,
}

impl MemberChange {
  /// A turn-start change, which starts no retry; its handoff summary is recorded in its event, not in this struct.
  fn at_turn_start(pool: &str, from: String, to: String, reason: String, recovers_at: Option<String>) -> Self {
    Self { pool: pool.to_string(), from, to, reason, recovers_at, summary: None }
  }

  /// The `quota_failover` chat event recording this change.
  pub fn event(&self) -> Value {
    let mut event = Map::from_iter([
      ("type".to_string(), json!(event_types::QUOTA_FAILOVER)),
      ("from".to_string(), json!(self.from)),
      ("to".to_string(), json!(self.to)),
      (event_types::QUOTA_FAILOVER_REASON.to_string(), json!(self.reason)),
      (event_types::QUOTA_FAILOVER_SET.to_string(), json!(self.pool)),
      (event_types::QUOTA_FAILOVER_RECOVERS_AT.to_string(), json!(self.recovers_at.clone().unwrap_or_default())),
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
  /// The session switched members. The caller appends the change event.
  Changed(MemberChange),
  /// No member is selectable: the turn ends with this error listing every member.
  Exhausted(String),
}

/// Register the `pool_member` session slot.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.metadata_slot_with_validator(
    metadata_slot::MetadataSlot {
      owner: OWNER,
      on: metadata_slot::MetadataFile::Session,
      after: Some("backend"),
      fields: vec![metadata_slot::SlotField { name: POOL_MEMBER_FIELD, default: Value::Null }],
    },
    validate_pool_member,
  )
}

fn validate_pool_member(values: &mut Map<String, Value>) -> Option<String> {
  values
    .get(POOL_MEMBER_FIELD)
    .is_some_and(|value| !value.is_null() && !value.is_string())
    .then(|| format!("{POOL_MEMBER_FIELD} must be a string or null"))
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

/// What a turn start decides for a session, before it records anything.
enum TurnDecision {
  /// The session keeps the member it records.
  Keep(String),
  /// The session moves to `member`, which the turn start records. `change` is the event to log. A first assignment
  /// logs none.
  Move { member: String, change: Option<MemberChange> },
  /// No member is selectable: the turn ends with this error.
  Exhausted(String),
}

/// Choose the member for a turn that is about to dispatch, record it, and return the change to log.
///
/// A model id resolves to itself. A pool session with no recorded member, or whose member is out, gets a fresh
/// selection. The first assignment records the member without a change event. A rotate pool's session
/// otherwise keeps its member until the member is out or its reading is exhausted. A priority pool's session keeps its
/// member unless it is on pay-as-you-go and a subscription is selectable (a subscription reset). It also switches when
/// it is near exhaustion and an under-line subscription is selectable. When no member is selectable the turn ends
/// with the error that lists every member.
pub async fn start_turn(
  features: &FeatureContext,
  cfg: &CharlieBotConfig,
  meta: &mut SessionMetadata,
) -> anyhow::Result<TurnStart> {
  match decide_turn(features, cfg, meta, true)? {
    TurnDecision::Keep(_) => Ok(TurnStart::Kept),
    TurnDecision::Move { member, change } => {
      if cfg.backends.pool(&meta.backend).is_some() {
        record_member(features, meta, &member).await?;
      }
      Ok(change.map_or(TurnStart::Kept, TurnStart::Changed))
    }
    TurnDecision::Exhausted(message) => Ok(TurnStart::Exhausted(message)),
  }
}

/// The member a session's display names: its recorded `pool_member` when `session_backend` is a configured pool and that
/// member belongs to the pool. A single-option backend, a deleted pool or a member outside the pool gives `None`, with
/// no error. Only display reads it; selection keeps its own read in [`decide_turn`].
pub fn display_pool_member<'a>(
  cfg: &CharlieBotConfig,
  session_backend: &str,
  recorded: Option<&'a str>,
) -> Option<&'a str> {
  let pool = cfg.backends.pool(session_backend)?;
  recorded.filter(|member| pool.members.iter().any(|pool_member| pool_member.backend == *member))
}

/// The member a turn of the session runs on, as the turn start decides it, read without recording anything or moving
/// a position. `None` when no member is selectable: the turn then ends with the exhausted error and runs nothing.
pub fn turn_member(
  features: &FeatureContext,
  cfg: &CharlieBotConfig,
  meta: &SessionMetadata,
) -> anyhow::Result<Option<String>> {
  Ok(match decide_turn(features, cfg, meta, false)? {
    TurnDecision::Keep(member) | TurnDecision::Move { member, .. } => Some(member),
    TurnDecision::Exhausted(_) => None,
  })
}

/// The turn start's decision for `meta`, computed without recording it. [`start_turn`] applies it with `advance` set,
/// and [`turn_member`] reads it with `advance` unset, so both answer from the one selection path.
fn decide_turn(
  features: &FeatureContext,
  cfg: &CharlieBotConfig,
  meta: &SessionMetadata,
  advance: bool,
) -> anyhow::Result<TurnDecision> {
  let pool = selection_pool_for_id(cfg, &meta.backend)?;
  let availability = backend_availability::backend_availability(features)?;
  let positions = features.startup.require::<RotationPositions>()?;
  let now = UtcDateTime::now();
  let selection = Selection::new(cfg, &features.registry, &availability, &pool, &[], now);
  let current = cfg.backends.pool(&meta.backend).and_then(|_| {
    meta
      .extra(POOL_MEMBER_FIELD)
      .and_then(Value::as_str)
      .filter(|backend_id| pool.member(backend_id).is_some())
      .map(str::to_string)
  });

  if let Some(current) = &current
    && !is_out(&availability, current, now)
  {
    return match pool.rule {
      PoolRule::Rotate => {
        if reading_exhausted(cfg, &features.registry, current, now)? {
          decide_fresh(features, &selection, Some(current), advance)
        } else {
          Ok(TurnDecision::Keep(current.clone()))
        }
      }
      PoolRule::Priority => decide_keep_or_switch(features, &selection, &positions, current, advance),
    };
  }
  decide_fresh(features, &selection, current.as_deref(), advance)
}

/// Select a member for a session whose current member is absent, out or spent. The change, when there was a current
/// member, names it and the moment it recovers.
fn decide_fresh(
  features: &FeatureContext,
  selection: &Selection<'_>,
  current: Option<&str>,
  advance: bool,
) -> anyhow::Result<TurnDecision> {
  let positions = features.startup.require::<RotationPositions>()?;
  let selected = if advance { positions.select(selection)? } else { positions.peek(selection)? };
  let Some(selected) = selected else {
    return Ok(TurnDecision::Exhausted(exhausted_message(features, selection.cfg, selection.pool)?));
  };
  // The first assignment is not a change between members, so it leaves no failover event. A current member that is out
  // or spent is never selected, so the selection always differs from it.
  let change = match current {
    None => None,
    Some(from) => {
      let recovers_at = match selection.availability.out_until_minutes(from) {
        Some(until) => Some(until),
        None => reading_reset(selection.cfg, selection.registry, from, selection.now)?,
      };
      // The row's own reason moves to the switch record as it is, so a member an earlier refusal or timeout recorded
      // does not show up as a quota change. A member the reading alone left keeps the quota reason.
      let reason = selection
        .availability
        .row(from)
        .map(|row| row.reason)
        .unwrap_or_else(|| event_types::QUOTA_REASON_QUOTA_ERROR.to_string());
      Some(MemberChange::at_turn_start(
        &selection.pool.id,
        from.to_string(),
        selected.backend.clone(),
        reason,
        recovers_at,
      ))
    }
  };
  Ok(TurnDecision::Move { member: selected.backend, change })
}

/// Apply the priority turn-start rules to a session whose current member is selectable. The rules compare the member
/// `peek` names. A move takes its new member through `select` when the turn advances the rotation, so each session that
/// moves at one turn start takes the next member in its tier.
fn decide_keep_or_switch(
  features: &FeatureContext,
  selection: &Selection<'_>,
  positions: &RotationPositions,
  current: &str,
  advance: bool,
) -> anyhow::Result<TurnDecision> {
  let Some(selected) = positions.peek(selection)? else {
    // The member is selectable by its out record but spent by its reading, and no other member is left.
    return Ok(TurnDecision::Exhausted(exhausted_message(features, selection.cfg, selection.pool)?));
  };
  if selected.backend == current {
    return Ok(TurnDecision::Keep(selected.backend));
  }
  let current_member = selection.pool.member(current).expect("the current member is in the pool");
  let current_reading = member_reading(selection.cfg, selection.registry, &current_member.backend, selection.now)?;
  let selected_reading = member_reading(selection.cfg, selection.registry, &selected.backend, selection.now)?;
  let (reason, recovers_at) = if member_billing(selection.cfg, current_member) == Some(PoolBilling::Payg)
    && member_billing(selection.cfg, &selected) == Some(PoolBilling::Subscription)
  {
    (event_types::QUOTA_REASON_SUBSCRIPTION_RESET.to_string(), None)
  } else if current_reading.as_ref().is_some_and(|reading| reading.exhausted) {
    // The reading says the member has no capacity left. The turn start leaves it, as an out record would.
    let reset = current_reading.as_ref().and_then(MemberReading::earliest_reset).map(format_minutes);
    (event_types::QUOTA_REASON_QUOTA_ERROR.to_string(), reset)
  } else if near_exhaustion(current_reading.as_ref())
    && member_billing(selection.cfg, &selected) == Some(PoolBilling::Subscription)
    && !near_exhaustion(selected_reading.as_ref())
  {
    let reset = current_reading.as_ref().and_then(MemberReading::earliest_reset).map(format_minutes);
    (event_types::QUOTA_REASON_NEAR_EXHAUSTION.to_string(), reset)
  } else {
    return Ok(TurnDecision::Keep(current.to_string()));
  };
  // Another turn can move the lane between `peek` and `select`, so a move records the member `select` takes. `select`
  // finds none only when a member's state changed between the two reads.
  let Some(moved) = (if advance { positions.select(selection)? } else { Some(selected) }) else {
    return Ok(TurnDecision::Exhausted(exhausted_message(features, selection.cfg, selection.pool)?));
  };
  Ok(TurnDecision::Move {
    member: moved.backend.clone(),
    change: Some(MemberChange::at_turn_start(
      &selection.pool.id,
      current.to_string(),
      moved.backend,
      reason,
      recovers_at,
    )),
  })
}

/// Whether the member's usage reading says it has no capacity left.
fn reading_exhausted(
  cfg: &CharlieBotConfig,
  registry: &Registry,
  backend_id: &str,
  now: UtcDateTime,
) -> anyhow::Result<bool> {
  Ok(member_reading(cfg, registry, backend_id, now)?.is_some_and(|reading| reading.exhausted))
}

/// The earliest reset the member's usage reading names, in the minute-resolution event format.
fn reading_reset(
  cfg: &CharlieBotConfig,
  registry: &Registry,
  backend_id: &str,
  now: UtcDateTime,
) -> anyhow::Result<Option<String>> {
  Ok(member_reading(cfg, registry, backend_id, now)?.and_then(|reading| reading.earliest_reset()).map(format_minutes))
}

/// The turn-start form of [`all_exhausted_message`], read from the process-wide owners.
fn exhausted_message(features: &FeatureContext, cfg: &CharlieBotConfig, pool: &ModelPool) -> anyhow::Result<String> {
  let availability = backend_availability::backend_availability(features)?;
  all_exhausted_message(cfg, &features.registry, &availability, pool)
}

/// The error that ends a turn when no member of the pool is selectable: one line per member, in config order. On a
/// priority pool a member's line names its billing, the reason it is out and when it recovers.
pub fn all_exhausted_message(
  cfg: &CharlieBotConfig,
  registry: &Registry,
  availability: &BackendAvailability,
  pool: &ModelPool,
) -> anyhow::Result<String> {
  let now = UtcDateTime::now();
  let mut lines = Vec::new();
  for member in &pool.members {
    let state = member_state_reason(cfg, registry, availability, &member.backend, now)?;
    let label = match (pool.rule, member_billing(cfg, member)) {
      (PoolRule::Priority, Some(billing)) => format!(" ({})", billing_label(billing)),
      _ => String::new(),
    };
    lines.push(format!("- {}{label}: {state}", member.backend));
  }
  Ok(format!("pool '{}' has no selectable member:\n{}", pool.id, lines.join("\n")))
}

fn member_state_reason(
  cfg: &CharlieBotConfig,
  registry: &Registry,
  availability: &BackendAvailability,
  backend_id: &str,
  now: UtcDateTime,
) -> anyhow::Result<String> {
  match availability.row(backend_id).filter(|row| row.out_until.into_inner() > now.into_inner()) {
    Some(row) => {
      let detail = row.detail.lines().next().unwrap_or_default().trim();
      let recovery = format!("{}, recovers at {}", row.reason, format_minutes(row.out_until));
      Ok(if detail.is_empty() { recovery } else { format!("{recovery} ({detail})") })
    }
    None => match member_reading(cfg, registry, backend_id, now)? {
      Some(reading) if reading.exhausted => Ok(reading.earliest_reset().map_or_else(
        || "quota exhausted".to_string(),
        |reset| format!("quota exhausted, resets at {}", format_minutes(reset)),
      )),
      _ => Ok("not selectable".to_string()),
    },
  }
}

/// The billing of the `backends.options` entry a member names: the option owns it, so every pool reads it there.
fn member_billing(cfg: &CharlieBotConfig, member: &PoolMember) -> Option<PoolBilling> {
  cfg.get_backend_option(&member.backend).and_then(|option| option.billing)
}

fn billing_label(billing: PoolBilling) -> &'static str {
  match billing {
    PoolBilling::Subscription => "subscription",
    PoolBilling::Payg => "pay-as-you-go",
  }
}

/// Persist `member` as the session's current member and mirror it into the in-memory metadata the resolver reads.
async fn record_member(features: &FeatureContext, meta: &mut SessionMetadata, member: &str) -> anyhow::Result<()> {
  set_current_member(features, &meta.id, member).await?;
  meta.set_extra(POOL_MEMBER_FIELD, json!(member));
  Ok(())
}

/// Persist `member` as the session's current member for a caller that holds no mutable metadata, as a mid-turn
/// switch does.
pub async fn set_current_member(features: &FeatureContext, session_id: &str, member: &str) -> anyhow::Result<()> {
  let values = Map::from_iter([(POOL_MEMBER_FIELD.to_string(), json!(member))]);
  let updated = features.store.update_slot_fields(session_id, OWNER, values).await?;
  if updated.is_none() {
    return Err(anyhow!("session '{session_id}' disappeared while recording {POOL_MEMBER_FIELD}"));
  }
  tracing::info!(target: "charliebot_agents", session_id, member, "pool_member_recorded");
  Ok(())
}

/// Render a reset in the minute-resolution form the pool events carry.
fn format_minutes(datetime: UtcDateTime) -> String {
  crate::backend_availability::format_minutes(datetime.into_inner())
}

/// The refusal of a session whose backend resolves to no `backends.options` entry.
pub const MISSING_BACKEND_OPTION: &str = "session backend resolution requires a configured backends.options entry";

/// The session's backend id: the one its metadata records, else the config's new-session default.
///
/// A config whose `backends.options` names no entry is a refusal, and so is an id that names no option.
pub fn session_backend_id_or_default(cfg: &CharlieBotConfig, meta: &SessionMetadata) -> anyhow::Result<String> {
  let backend_id = if meta.backend.is_empty() {
    cfg.backends.new_session_backend().ok_or_else(|| anyhow!(MISSING_BACKEND_OPTION))?
  } else {
    meta.backend.clone()
  };
  cfg.backends.check_backend_id(&backend_id).map_err(anyhow::Error::msg)?;
  Ok(backend_id)
}

/// The member a session runs on now, for the resolver. A model session resolves to its backend id. A pool session
/// resolves to its recorded member. A pool with no recorded member or a member it no longer lists is a refusal.
pub fn current_member_id(cfg: &CharlieBotConfig, meta: &SessionMetadata) -> anyhow::Result<String> {
  let backend_id = session_backend_id_or_default(cfg, meta)?;
  let Some(pool) = cfg.backends.pool(&backend_id) else { return Ok(backend_id) };
  let member =
    meta.extra(POOL_MEMBER_FIELD).and_then(Value::as_str).filter(|backend_id| !backend_id.is_empty()).ok_or_else(
      || {
        anyhow!(
          "session backend '{}' has no recorded {POOL_MEMBER_FIELD}; turn start records one before every run",
          backend_id
        )
      },
    )?;
  if pool.members.iter().all(|pool_member| pool_member.backend != member) {
    return Err(anyhow!(
      "session backend '{}' records {POOL_MEMBER_FIELD} '{member}', which config.yaml backends.pools no longer lists",
      backend_id
    ));
  }
  Ok(member.to_string())
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;
  use std::sync::atomic::{AtomicBool, Ordering};

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
      anyhow::bail!("the model pool test backend is not launched")
    }
  }

  /// Whether `claude-res`'s reservations have left its headroom. Only `a_payg_session_keeps_its_member_while_fully_
  /// reserved_and_returns_on_the_reset` sets it.
  static RESERVED_RELEASE: AtomicBool = AtomicBool::new(false);

  /// The reading of a synthetic member: the canned windows the selection tests order.
  fn test_reading(option: &BackendOption, now: UtcDateTime) -> Option<MemberReading> {
    let reset = |minutes: i64| Some(UtcDateTime::from(now.into_inner() + time::Duration::minutes(minutes)));
    let weekly = |utilization: f64, minutes: i64| MemberReading {
      windows: vec![MemberWindow { utilization, window_minutes: Some(10_080), resets_at: reset(minutes) }],
      weekly_resets_at: reset(minutes),
      near_exhaustion: false,
      exhausted: false,
      fully_reserved: false,
    };
    match option.id.as_str() {
      "chatgpt" => Some(weekly(0.80, 360)),
      "claude-sub" => Some(weekly(0.49, 2_880)),
      // Near exhaustion: 95% used with a reset beyond the 45-minute horizon.
      "near" => Some(MemberReading { near_exhaustion: true, ..weekly(0.95, 180) }),
      // No capacity left, while the out record has not been written.
      "spent" => Some(MemberReading { exhausted: true, ..weekly(1.0, 60) }),
      // Headroom left, but the runs placed on it reserved all of it.
      "reserved" => Some(MemberReading { fully_reserved: true, ..weekly(0.49, 2_880) }),
      // The same, until `RESERVED_RELEASE` frees it for the turn-level test.
      "claude-res" => {
        Some(MemberReading { fully_reserved: !RESERVED_RELEASE.load(Ordering::SeqCst), ..weekly(0.49, 2_880) })
      }
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

  /// Two pools share `chatgpt` and `spent`: `auto` is the priority pool, `light` the rotate pool.
  const CONFIG: &str = "\
backends:
  options:
    - {id: chatgpt, label: ChatGPT, type: synthetic, model: model-gpt, billing: subscription}
    - {id: claude-sub, label: Claude, type: synthetic, model: model-claude, billing: subscription}
    - {id: near, label: Near, type: synthetic, model: model-near, billing: subscription}
    - {id: spent, label: Spent, type: synthetic, model: model-spent, billing: subscription}
    - {id: devin, label: Devin, type: synthetic, model: model-devin, billing: subscription}
    - {id: payg-a, label: Payg A, type: synthetic, model: model-payg, billing: payg}
  pools:
    - id: auto
      label: Auto
      rule: priority
      members: [chatgpt, claude-sub, near, spent, devin, payg-a]
    - id: light
      label: Light
      rule: rotate
      members: [spent, payg-a, chatgpt]
    - id: chatgpt-only
      label: ChatGPT only
      rule: rotate
      members: [chatgpt]
";

  /// The priority pool `auto` over five subscriptions, for the tests that take turns across them.
  const ROTATION_CONFIG: &str = "\
backends:
  options:
    - {id: member-a, label: Member A, type: synthetic, model: model-a, billing: subscription}
    - {id: member-b, label: Member B, type: synthetic, model: model-b, billing: subscription}
    - {id: member-c, label: Member C, type: synthetic, model: model-c, billing: subscription}
    - {id: member-d, label: Member D, type: synthetic, model: model-d, billing: subscription}
    - {id: member-e, label: Member E, type: synthetic, model: model-e, billing: subscription}
  pools:
    - id: auto
      label: Auto
      rule: priority
      members: [member-a, member-b, member-c, member-d, member-e]
";

  const ROTATE_CONFIG: &str = "\
backends:
  options:
    - {id: member-a, label: Member A, type: synthetic, model: model-a}
    - {id: member-b, label: Member B, type: synthetic, model: model-b}
  pools:
    - id: rotating
      label: Rotating
      rule: rotate
      members: [member-a, member-b]
";

  /// One priority pool over two subscriptions below the line, one near exhaustion, one spent, and three pay-as-you-go
  /// members. The readings are `test_reading`'s.
  const TIER_CONFIG: &str = "\
backends:
  options:
    - {id: chatgpt, label: ChatGPT, type: synthetic, model: model-gpt, billing: subscription}
    - {id: claude-sub, label: Claude, type: synthetic, model: model-claude, billing: subscription}
    - {id: near, label: Near, type: synthetic, model: model-near, billing: subscription}
    - {id: spent, label: Spent, type: synthetic, model: model-spent, billing: subscription}
    - {id: payg-a, label: Payg A, type: synthetic, model: model-payg-a, billing: payg}
    - {id: payg-b, label: Payg B, type: synthetic, model: model-payg-b, billing: payg}
    - {id: payg-c, label: Payg C, type: synthetic, model: model-payg-c, billing: payg}
  pools:
    - id: cheap
      label: Cheap
      rule: priority
      members: [chatgpt, claude-sub, near, spent, payg-a, payg-b, payg-c]
";

  /// A priority pool over an open subscription, a subscription whose logins' headroom is all reserved (`test_reading`
  /// reads `reserved` fully reserved, `claude-res` reserved until `RESERVED_RELEASE` is set), and one pay-as-you-go
  /// member.
  const RESERVED_CONFIG: &str = "\
backends:
  options:
    - {id: claude-sub, label: Claude, type: synthetic, model: model-claude, billing: subscription}
    - {id: reserved, label: Reserved, type: synthetic, model: model-reserved, billing: subscription}
    - {id: claude-res, label: Claude reserved, type: synthetic, model: model-claude-res, billing: subscription}
    - {id: payg-a, label: Payg A, type: synthetic, model: model-payg, billing: payg}
  pools:
    - id: cheap
      label: Cheap
      rule: priority
      members: [claude-sub, reserved, payg-a]
    - id: reserved-only
      label: Reserved only
      rule: priority
      members: [reserved]
    - id: reserved-pair
      label: Reserved pair
      rule: priority
      members: [claude-res, payg-a]
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
    startup.insert(RotationPositions::default()).unwrap();
    (home, Arc::new(FeatureContext { config, credentials, store, registry, startup }))
  }

  fn loaded_cfg(features: &FeatureContext) -> Arc<CharlieBotConfig> {
    features.config.get().unwrap()
  }

  #[test]
  fn the_display_member_is_shown_only_for_a_member_of_a_configured_pool() {
    let (_home, features) = fixture_with(CONFIG);
    let cfg = loaded_cfg(&features);
    assert_eq!(display_pool_member(&cfg, "auto", Some("claude-sub")), Some("claude-sub"));
    // A member the pool does not name, a single-option backend, a deleted pool and no record all show no member.
    assert_eq!(display_pool_member(&cfg, "chatgpt-only", Some("spent")), None);
    assert_eq!(display_pool_member(&cfg, "claude-sub", Some("claude-sub")), None);
    assert_eq!(display_pool_member(&cfg, "retired-pool", Some("chatgpt")), None);
    assert_eq!(display_pool_member(&cfg, "auto", None), None);
  }

  async fn pool_session(features: &FeatureContext, request_id: &str, pool_id: &str) -> SessionMetadata {
    features
      .store
      .create_task(
        CreateTask {
          request_id: request_id.to_string(),
          task_parent_id: None,
          profile: charliebot_store::metadata::TaskProfile::Manager,
          task: None,
          name: Some("Pool session".to_string()),
          backend: Some(pool_id.to_string()),
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

  fn positions(features: &FeatureContext) -> Arc<RotationPositions> {
    features.startup.require::<RotationPositions>().unwrap()
  }

  fn changed(start: TurnStart) -> MemberChange {
    match start {
      TurnStart::Changed(change) => change,
      other => panic!("expected a member change, got {other:?}"),
    }
  }

  /// Select from the pool `id` the way a call does: the rotate position advances, the priority order is read.
  fn pick(features: &FeatureContext, id: &str, excluded: &[&str]) -> Option<String> {
    let cfg = loaded_cfg(features);
    let excluded = excluded.iter().map(|backend_id| (*backend_id).to_string()).collect::<Vec<_>>();
    let pool = selection_pool_for_id(&cfg, id).unwrap();
    let availability = availability(features);
    let selection = Selection::new(&cfg, &features.registry, &availability, &pool, &excluded, UtcDateTime::now());
    positions(features).select(&selection).unwrap().map(|member| member.backend)
  }

  /// The priority pool picks subscription members first, and the pay-as-you-go member only as the last resort.
  #[tokio::test]
  async fn subscription_tiers_precede_pay_as_you_go() {
    let (_home, features) = fixture();
    // The readings: ChatGPT (20% left, 6h), Claude (51%, 2 days), Devin (no reading).
    assert_eq!(pick(&features, "auto", &[]), Some("chatgpt".to_string()));
    assert_eq!(pick(&features, "auto", &["chatgpt"]), Some("claude-sub".to_string()));
    assert_eq!(pick(&features, "auto", &["chatgpt", "claude-sub"]), Some("devin".to_string()));
    // A subscription at or above the line is the last resort, before the first pay-as-you-go member.
    assert_eq!(pick(&features, "auto", &["chatgpt", "claude-sub", "devin"]), Some("near".to_string()));
    assert_eq!(pick(&features, "auto", &["chatgpt", "claude-sub", "devin", "near"]), Some("payg-a".to_string()));
    assert_eq!(pick(&features, "auto", &["chatgpt", "claude-sub", "devin", "near", "payg-a"]), None);
  }

  #[tokio::test]
  async fn new_auto_sessions_take_the_subscriptions_in_turn() {
    let (_home, features) = fixture_with(ROTATION_CONFIG);
    let cfg = loaded_cfg(&features);
    let expected = ["member-a", "member-b", "member-c", "member-d", "member-e", "member-a"];

    for (index, expected_member) in expected.into_iter().enumerate() {
      let mut meta = pool_session(&features, &format!("pool-rotation-{index}"), "auto").await;
      let read_member = turn_member(&features, &cfg, &meta).unwrap();
      assert_eq!(read_member.as_deref(), Some(expected_member));

      start_turn(&features, &cfg, &mut meta).await.unwrap();
      assert_eq!(current_member_id(&cfg, &meta).unwrap(), expected_member);
    }
  }

  #[tokio::test]
  async fn consecutive_selects_rotate_across_the_subscriptions() {
    let (_home, features) = fixture_with(ROTATION_CONFIG);
    let picks = (0..6).map(|_| pick(&features, "auto", &[]).unwrap()).collect::<Vec<_>>();
    assert_eq!(picks, ["member-a", "member-b", "member-c", "member-d", "member-e", "member-a"]);
  }

  #[tokio::test]
  async fn a_selectable_subscription_wins_over_pay_as_you_go() {
    let (_home, features) = fixture_with(TIER_CONFIG);
    assert_eq!(pick(&features, "cheap", &[]), Some("chatgpt".to_string()));
    assert_eq!(pick(&features, "cheap", &["chatgpt"]), Some("claude-sub".to_string()), "an excluded subscription");
    assert_eq!(pick(&features, "cheap", &["claude-sub"]), Some("chatgpt".to_string()));
    assert_eq!(pick(&features, "cheap", &[]), Some("claude-sub".to_string()));
  }

  #[tokio::test]
  async fn with_no_selectable_subscription_the_pay_as_you_go_members_rotate() {
    let (_home, features) = fixture_with(TIER_CONFIG);
    let availability = availability(&features);
    let cfg = loaded_cfg(&features);
    for subscription in ["chatgpt", "claude-sub", "near"] {
      availability
        .record(&cfg, &features.registry, subscription, None, backend_availability::REASON_QUOTA_ERROR, "spent")
        .unwrap();
    }
    let picks = (0..4).map(|_| pick(&features, "cheap", &[]).unwrap()).collect::<Vec<_>>();
    assert_eq!(picks, ["payg-a", "payg-b", "payg-c", "payg-a"]);
  }

  /// A fully reserved subscription yields to the pay-as-you-go member, and the skipped pick leaves the subscription
  /// lane's position where it was.
  #[tokio::test]
  async fn a_fully_reserved_subscription_yields_to_pay_as_you_go_and_leaves_its_cursor() {
    let (_home, features) = fixture_with(RESERVED_CONFIG);
    // The open subscription contends first; the reserved one is skipped while the payg member is selectable.
    assert_eq!(pick(&features, "cheap", &[]), Some("claude-sub".to_string()));
    assert_eq!(pick(&features, "cheap", &["claude-sub"]), Some("payg-a".to_string()));
    // The payg pick moved no subscription position: with the payg member out, the walk resumes at `reserved`.
    assert_eq!(pick(&features, "cheap", &["payg-a"]), Some("reserved".to_string()));
    assert_eq!(pick(&features, "cheap", &[]), Some("claude-sub".to_string()));
  }

  /// A reservation never refuses a run: with no payg member to yield to, the fully reserved subscription and the
  /// model id that forms its one-member pool still take the run.
  #[tokio::test]
  async fn a_fully_reserved_subscription_takes_the_run_when_no_payg_member_is_selectable() {
    let (_home, features) = fixture_with(RESERVED_CONFIG);
    assert_eq!(pick(&features, "reserved-only", &[]), Some("reserved".to_string()));
    let cfg = loaded_cfg(&features);
    let selector = Selector::from_features(&features, &cfg).unwrap();
    // The model id's one-member pool is the option id here; the same filter picks the reserved member through it.
    assert_eq!(selector.peek("reserved", &[]).unwrap().map(|option| option.id), Some("reserved".to_string()));
  }

  /// A payg session keeps its member while the subscription stays fully reserved, and returns with a subscription
  /// reset once the reservations leave headroom.
  #[tokio::test]
  async fn a_payg_session_keeps_its_member_while_fully_reserved_and_returns_on_the_reset() {
    let (_home, features) = fixture_with(RESERVED_CONFIG);
    let cfg = loaded_cfg(&features);
    RESERVED_RELEASE.store(false, Ordering::SeqCst);
    let mut meta = pool_session(&features, "payg-reserved-keep", "reserved-pair").await;
    assert_eq!(turn_member(&features, &cfg, &meta).unwrap().as_deref(), Some("payg-a"));
    start_turn(&features, &cfg, &mut meta).await.unwrap();
    assert_eq!(current_member_id(&cfg, &meta).unwrap(), "payg-a");

    assert_eq!(turn_member(&features, &cfg, &meta).unwrap().as_deref(), Some("payg-a"));

    RESERVED_RELEASE.store(true, Ordering::SeqCst);
    assert_eq!(turn_member(&features, &cfg, &meta).unwrap().as_deref(), Some("claude-res"));
    let change = changed(start_turn(&features, &cfg, &mut meta).await.unwrap());
    assert_eq!(change.to, "claude-res");
    assert_eq!(change.reason, event_types::QUOTA_REASON_SUBSCRIPTION_RESET);
  }

  #[tokio::test]
  async fn cooling_and_excluded_members_take_no_turn() {
    let (_home, features) = fixture_with(TIER_CONFIG);
    // While an under-line subscription is selectable, `near` (at or above the line) and `spent` (exhausted) take no
    // turn.
    let picks = (0..3).map(|_| pick(&features, "cheap", &[]).unwrap()).collect::<Vec<_>>();
    assert_eq!(picks, ["chatgpt", "claude-sub", "chatgpt"]);

    // With every subscription gone, only the pay-as-you-go member that is neither cooling nor excluded takes turns.
    availability(&features)
      .record(
        &loaded_cfg(&features),
        &features.registry,
        "payg-b",
        None,
        backend_availability::REASON_QUOTA_ERROR,
        "spent",
      )
      .unwrap();
    let excluded = ["chatgpt", "claude-sub", "near", "payg-c"];
    assert_eq!(pick(&features, "cheap", &excluded), Some("payg-a".to_string()));
    assert_eq!(pick(&features, "cheap", &excluded), Some("payg-a".to_string()));
  }

  #[tokio::test]
  async fn peeks_move_no_lane_and_the_two_tiers_turn_apart() {
    let (_home, features) = fixture_with(TIER_CONFIG);
    let cfg = loaded_cfg(&features);
    let meta = pool_session(&features, "tier-peek", "cheap").await;
    // A turn read names the member the next select takes, and reading it twice moves nothing.
    assert_eq!(turn_member(&features, &cfg, &meta).unwrap().as_deref(), Some("chatgpt"));
    assert_eq!(turn_member(&features, &cfg, &meta).unwrap().as_deref(), Some("chatgpt"));
    assert_eq!(pick(&features, "cheap", &[]), Some("chatgpt".to_string()));

    // Normal selects alternate with selects that exclude both subscriptions. The subscription lane keeps its turn
    // order, and the pay-as-you-go lane rotates on its own.
    let subscriptions_excluded = ["chatgpt", "claude-sub", "near"];
    let mut normal = Vec::new();
    let mut pay_as_you_go = Vec::new();
    for _ in 0..3 {
      normal.push(pick(&features, "cheap", &[]).unwrap());
      pay_as_you_go.push(pick(&features, "cheap", &subscriptions_excluded).unwrap());
    }
    assert_eq!(normal, ["claude-sub", "chatgpt", "claude-sub"]);
    assert_eq!(pay_as_you_go, ["payg-a", "payg-b", "payg-c"]);
  }

  #[tokio::test]
  async fn sessions_moved_at_one_turn_start_take_different_subscriptions() {
    let (_home, features) = fixture_with(TIER_CONFIG);
    let cfg = loaded_cfg(&features);
    let mut changes = Vec::new();
    for index in 0..2 {
      let mut meta = pool_session(&features, &format!("tier-moved-{index}"), "cheap").await;
      features
        .store
        .update_slot_fields(&meta.id, OWNER, Map::from_iter([(POOL_MEMBER_FIELD.to_string(), json!("payg-a"))]))
        .await
        .unwrap();
      meta.set_extra(POOL_MEMBER_FIELD, json!("payg-a"));
      changes.push(changed(start_turn(&features, &cfg, &mut meta).await.unwrap()));
    }
    // Each session runs on pay-as-you-go while a subscription is selectable, so each one moves at its turn start.
    assert_eq!(changes.iter().map(|change| change.to.as_str()).collect::<Vec<_>>(), ["chatgpt", "claude-sub"]);
    assert!(changes.iter().all(|change| change.reason == event_types::QUOTA_REASON_SUBSCRIPTION_RESET));
  }

  #[tokio::test]
  async fn a_random_start_holds_across_reads() {
    let (_home, features) = fixture_with(TIER_CONFIG);
    let cfg = loaded_cfg(&features);
    let availability = availability(&features);
    let positions = RotationPositions::new(LaneStart::Random);
    let cheap = selection_pool_for_id(&cfg, "cheap").unwrap();
    let selection = Selection::new(&cfg, &features.registry, &availability, &cheap, &[], UtcDateTime::now());
    let peeked = positions.peek(&selection).unwrap();
    assert_eq!(positions.peek(&selection).unwrap(), peeked, "a second read draws no new start");
    assert_eq!(positions.select(&selection).unwrap(), peeked, "the select takes the member the peek named");
  }

  #[tokio::test]
  async fn random_cli_start_does_not_change_rotate_pool_order() {
    let (_home, features) = fixture_with(ROTATE_CONFIG);
    let cfg = loaded_cfg(&features);
    let availability = availability(&features);
    let positions = RotationPositions::new(LaneStart::Random);
    let rotate = selection_pool_for_id(&cfg, "rotating").unwrap();
    let selection = Selection::new(&cfg, &features.registry, &availability, &rotate, &[], UtcDateTime::now());
    let selected = (0..4).map(|_| positions.select(&selection).unwrap().unwrap().backend).collect::<Vec<_>>();
    assert_eq!(selected, ["member-a", "member-b", "member-a", "member-b"]);
  }

  #[tokio::test]
  async fn an_out_member_is_skipped_and_an_exhausted_reading_is_skipped() {
    let (_home, features) = fixture();
    availability(&features)
      .record(
        &loaded_cfg(&features),
        &features.registry,
        "chatgpt",
        None,
        backend_availability::REASON_QUOTA_ERROR,
        "spent",
      )
      .unwrap();
    assert_eq!(pick(&features, "auto", &[]), Some("claude-sub".to_string()));
  }

  #[tokio::test]
  async fn rotate_skips_excluded_out_and_exhausted_members_and_keeps_one_without_a_reading() {
    let (_home, features) = fixture();
    let cfg = loaded_cfg(&features);
    let availability = availability(&features);
    let now = UtcDateTime::now();
    let rotate = |features: &FeatureContext, excluded: &[&str]| {
      let excluded = excluded.iter().map(|backend_id| (*backend_id).to_string()).collect::<Vec<_>>();
      let pool = selection_pool_for_id(&cfg, "light").unwrap();
      let selection = Selection::new(&cfg, &features.registry, &availability, &pool, &excluded, now);
      positions(features).select(&selection).unwrap().map(|member| member.backend)
    };

    // `spent`'s reading says it is exhausted. `payg-a` exposes no reading and is selectable.
    assert_eq!(rotate(&features, &[]), Some("payg-a".to_string()));
    assert_eq!(rotate(&features, &["payg-a"]), Some("chatgpt".to_string()), "a member the caller excludes is skipped");

    availability
      .record(
        &loaded_cfg(&features),
        &features.registry,
        "payg-a",
        None,
        backend_availability::REASON_QUOTA_ERROR,
        "spent",
      )
      .unwrap();
    assert_eq!(rotate(&features, &[]), Some("chatgpt".to_string()), "a member the owner recorded out is skipped");

    availability
      .record(
        &loaded_cfg(&features),
        &features.registry,
        "chatgpt",
        None,
        backend_availability::REASON_QUOTA_ERROR,
        "spent",
      )
      .unwrap();
    assert_eq!(rotate(&features, &[]), None);
  }

  #[tokio::test]
  async fn a_member_both_pools_share_is_skipped_by_both_once_recorded_out() {
    let (_home, features) = fixture();
    // `chatgpt` is a member of the priority pool and of the rotate pool. One out record covers both.
    availability(&features)
      .record(
        &loaded_cfg(&features),
        &features.registry,
        "chatgpt",
        None,
        backend_availability::REASON_QUOTA_ERROR,
        "spent",
      )
      .unwrap();
    assert_eq!(pick(&features, "auto", &[]), Some("claude-sub".to_string()));
    assert_eq!(pick(&features, "light", &[]), Some("payg-a".to_string()));
    assert_eq!(
      pick(&features, "light", &[]),
      Some("payg-a".to_string()),
      "the rotation wraps to the one selectable member"
    );
  }

  #[tokio::test]
  async fn rotation_advances_past_the_chosen_member_and_a_peek_leaves_it() {
    let (_home, features) = fixture();
    let cfg = loaded_cfg(&features);
    let availability = availability(&features);
    let now = UtcDateTime::now();
    let light = selection_pool_for_id(&cfg, "light").unwrap();
    let selection = Selection::new(&cfg, &features.registry, &availability, &light, &[], now);
    let peeked = positions(&features).peek(&selection).unwrap();
    assert_eq!(peeked.map(|member| member.backend), Some("payg-a".to_string()));
    assert_eq!(pick(&features, "light", &[]), Some("payg-a".to_string()), "the peek did not move the position");
    assert_eq!(pick(&features, "light", &[]), Some("chatgpt".to_string()), "the next select moves to the next member");
  }

  #[tokio::test]
  async fn turn_start_records_the_first_member_without_a_change_event_and_keeps_it_below_the_line() {
    let (_home, features) = fixture();
    let cfg = loaded_cfg(&features);
    let mut meta = pool_session(&features, "pool-first-turn", "auto").await;

    let change = start_turn(&features, &cfg, &mut meta).await.unwrap();
    assert_eq!(change, TurnStart::Kept, "the first assignment is not a change between members");
    assert_eq!(meta.extra(POOL_MEMBER_FIELD), Some(&json!("chatgpt")));
    let stored = features.store.read_metadata_fresh(&meta.id).unwrap().unwrap();
    assert_eq!(stored.extra(POOL_MEMBER_FIELD), Some(&json!("chatgpt")));
    assert!(features.store.load_all_facts(&meta.id).unwrap().iter().all(|event| event["type"] != "quota_failover"));

    // A member below the 90% line is kept at the next turn start.
    let change = start_turn(&features, &cfg, &mut meta).await.unwrap();
    assert_eq!(change, TurnStart::Kept);
    assert_eq!(meta.extra(POOL_MEMBER_FIELD), Some(&json!("chatgpt")));
  }

  #[tokio::test]
  async fn turn_start_switches_off_an_out_member_with_a_quota_error_change_named_by_its_pool() {
    let (_home, features) = fixture();
    let cfg = loaded_cfg(&features);
    let mut meta = pool_session(&features, "pool-out-member", "auto").await;
    start_turn(&features, &cfg, &mut meta).await.unwrap();
    availability(&features)
      .record(
        &loaded_cfg(&features),
        &features.registry,
        "chatgpt",
        None,
        backend_availability::REASON_QUOTA_ERROR,
        "spent",
      )
      .unwrap();

    let change = changed(start_turn(&features, &cfg, &mut meta).await.unwrap());
    assert_eq!((change.from.as_str(), change.to.as_str()), ("chatgpt", "claude-sub"));
    assert_eq!(change.reason, event_types::QUOTA_REASON_QUOTA_ERROR);
    assert!(change.recovers_at.is_some());
    let event = change.event();
    assert_eq!(event["type"], "quota_failover");
    assert_eq!(event["from"], "chatgpt");
    assert_eq!(event["to"], "claude-sub");
    assert_eq!(event["reason"], "quota_error");
    assert_eq!(event["set"], "auto", "the set names the pool that switched");
    assert_eq!(meta.extra(POOL_MEMBER_FIELD), Some(&json!("claude-sub")));
  }

  #[tokio::test]
  async fn turn_start_writes_the_recorded_refusal_reason_as_it_is() {
    let (_home, features) = fixture();
    let cfg = loaded_cfg(&features);
    let mut meta = pool_session(&features, "pool-refused-member", "auto").await;
    start_turn(&features, &cfg, &mut meta).await.unwrap();
    availability(&features)
      .record(
        &loaded_cfg(&features),
        &features.registry,
        "chatgpt",
        None,
        backend_availability::REASON_ACCESS_REFUSED,
        "Failed to authenticate",
      )
      .unwrap();

    let change = changed(start_turn(&features, &cfg, &mut meta).await.unwrap());
    assert_eq!((change.from.as_str(), change.to.as_str()), ("chatgpt", "claude-sub"));
    assert_eq!(change.reason, event_types::QUOTA_REASON_ACCESS_REFUSED);
    assert!(change.recovers_at.is_some());
    assert_eq!(change.event()["reason"], "access_refused");
    assert_eq!(change.event()["set"], "auto");
  }

  #[tokio::test]
  async fn a_rotate_session_keeps_its_member_until_the_member_is_out() {
    let (_home, features) = fixture();
    let cfg = loaded_cfg(&features);
    let mut meta = pool_session(&features, "pool-rotate-session", "light").await;
    start_turn(&features, &cfg, &mut meta).await.unwrap();
    assert_eq!(meta.extra(POOL_MEMBER_FIELD), Some(&json!("payg-a")));

    // The rotation moved on, but the session stays on its member across turns.
    assert_eq!(start_turn(&features, &cfg, &mut meta).await.unwrap(), TurnStart::Kept);
    assert_eq!(meta.extra(POOL_MEMBER_FIELD), Some(&json!("payg-a")));

    availability(&features)
      .record(
        &loaded_cfg(&features),
        &features.registry,
        "payg-a",
        None,
        backend_availability::REASON_QUOTA_ERROR,
        "spent",
      )
      .unwrap();
    let change = changed(start_turn(&features, &cfg, &mut meta).await.unwrap());
    assert_eq!((change.from.as_str(), change.to.as_str()), ("payg-a", "chatgpt"));
    assert_eq!(change.event()["set"], "light");
  }

  #[tokio::test]
  async fn turn_start_returns_to_a_subscription_after_it_resets() {
    let (_home, features) = fixture();
    let cfg = loaded_cfg(&features);
    let mut meta = pool_session(&features, "pool-subscription-reset", "auto").await;
    let availability = availability(&features);
    for member in ["chatgpt", "claude-sub", "near", "devin"] {
      availability
        .record(
          &loaded_cfg(&features),
          &features.registry,
          member,
          None,
          backend_availability::REASON_QUOTA_ERROR,
          "spent",
        )
        .unwrap();
    }
    start_turn(&features, &cfg, &mut meta).await.unwrap();
    assert_eq!(meta.extra(POOL_MEMBER_FIELD), Some(&json!("payg-a")));

    // Their out records expire, so turn start returns the session to the subscription tier.
    let expired = UtcDateTime::from(time::OffsetDateTime::now_utc() - time::Duration::hours(1));
    for member in ["chatgpt", "claude-sub", "near", "devin"] {
      availability
        .record(
          &loaded_cfg(&features),
          &features.registry,
          member,
          Some(expired),
          backend_availability::REASON_QUOTA_ERROR,
          "spent",
        )
        .unwrap();
    }
    let change = changed(start_turn(&features, &cfg, &mut meta).await.unwrap());
    assert_eq!((change.from.as_str(), change.to.as_str()), ("payg-a", "chatgpt"));
    assert_eq!(change.reason, event_types::QUOTA_REASON_SUBSCRIPTION_RESET);
    assert_eq!(change.event()["reason"], "subscription_reset");
    assert_eq!(meta.extra(POOL_MEMBER_FIELD), Some(&json!("chatgpt")));
  }

  #[tokio::test]
  async fn turn_start_leaves_a_near_exhausted_member_for_an_under_line_subscription() {
    let (_home, features) = fixture();
    let cfg = loaded_cfg(&features);
    let mut meta = pool_session(&features, "pool-near-exhaustion", "auto").await;
    features
      .store
      .update_slot_fields(&meta.id, OWNER, Map::from_iter([(POOL_MEMBER_FIELD.to_string(), json!("near"))]))
      .await
      .unwrap();
    meta.set_extra(POOL_MEMBER_FIELD, json!("near"));

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
    let mut meta = pool_session(&features, "pool-spent-reading", "auto").await;
    features
      .store
      .update_slot_fields(&meta.id, OWNER, Map::from_iter([(POOL_MEMBER_FIELD.to_string(), json!("spent"))]))
      .await
      .unwrap();
    meta.set_extra(POOL_MEMBER_FIELD, json!("spent"));

    let change = changed(start_turn(&features, &cfg, &mut meta).await.unwrap());
    assert_eq!((change.from.as_str(), change.to.as_str()), ("spent", "chatgpt"));
    assert_eq!(change.reason, event_types::QUOTA_REASON_QUOTA_ERROR);
    assert_eq!(meta.extra(POOL_MEMBER_FIELD), Some(&json!("chatgpt")));
  }

  #[tokio::test]
  async fn an_exhausted_pool_ends_the_turn_with_one_line_per_member() {
    let (_home, features) = fixture();
    let cfg = loaded_cfg(&features);
    let availability = availability(&features);
    let mut meta = pool_session(&features, "pool-exhausted", "auto").await;
    let reset = UtcDateTime::parse("2999-01-02T03:04:05Z").unwrap();
    for member in ["chatgpt", "claude-sub", "near", "devin", "payg-a"] {
      availability
        .record(
          &loaded_cfg(&features),
          &features.registry,
          member,
          Some(reset),
          backend_availability::REASON_QUOTA_ERROR,
          "API Error: 402",
        )
        .unwrap();
    }

    let start = start_turn(&features, &cfg, &mut meta).await.unwrap();
    let TurnStart::Exhausted(message) = start else { panic!("expected the exhausted turn, got {start:?}") };
    assert!(message.starts_with("pool 'auto' has no selectable member:"), "{message}");
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
    assert_eq!(meta.extra(POOL_MEMBER_FIELD), Some(&Value::Null), "an exhausted turn records no member");
  }

  #[tokio::test]
  async fn the_resolver_reads_the_recorded_member_and_refuses_an_unlisted_one() {
    let (_home, features) = fixture();
    let cfg = loaded_cfg(&features);
    let mut meta = pool_session(&features, "pool-resolver", "auto").await;
    let error = current_member_id(&cfg, &meta).unwrap_err().to_string();
    assert!(error.contains("has no recorded pool_member"), "{error}");

    start_turn(&features, &cfg, &mut meta).await.unwrap();
    assert_eq!(current_member_id(&cfg, &meta).unwrap(), "chatgpt");

    meta.set_extra(POOL_MEMBER_FIELD, json!("devin-gone"));
    let error = current_member_id(&cfg, &meta).unwrap_err().to_string();
    assert!(error.contains("'devin-gone'") && error.contains("backends.pools"), "{error}");
  }

  #[tokio::test]
  async fn a_model_and_its_one_member_pool_use_the_same_selection_filters() {
    let (_home, features) = fixture();
    let cfg = loaded_cfg(&features);
    let selector = Selector::from_features(&features, &cfg).unwrap();

    let model = selector.peek("chatgpt", &[]).unwrap().map(|option| option.id);
    let pool = selector.peek("chatgpt-only", &[]).unwrap().map(|option| option.id);
    assert_eq!(model, pool);
    assert_eq!(
      selector.ordered(&["chatgpt".to_string()], &[]).unwrap().map(|option| option.id),
      selector.ordered(&["chatgpt-only".to_string()], &[]).unwrap().map(|option| option.id)
    );

    assert!(selector.select("chatgpt", &["chatgpt".to_string()]).unwrap().is_none());
    assert!(selector.select("chatgpt-only", &["chatgpt".to_string()]).unwrap().is_none());

    availability(&features)
      .record(
        &loaded_cfg(&features),
        &features.registry,
        "chatgpt",
        None,
        backend_availability::REASON_QUOTA_ERROR,
        "spent",
      )
      .unwrap();
    assert!(selector.select("chatgpt", &[]).unwrap().is_none());
    assert!(selector.select("chatgpt-only", &[]).unwrap().is_none());
    assert!(selector.exhausted_message("chatgpt").unwrap().contains("- chatgpt: quota_error"));
    assert!(selector.exhausted_message("chatgpt-only").unwrap().contains("- chatgpt: quota_error"));
  }

  #[tokio::test]
  async fn a_model_session_resolves_to_itself_without_recording_pool_metadata() {
    let (_home, features) = fixture();
    let cfg = loaded_cfg(&features);
    let meta = pool_session(&features, "model-current-member", "chatgpt").await;
    let mut singleton_pool_meta = pool_session(&features, "singleton-current-member", "chatgpt-only").await;
    singleton_pool_meta.set_extra(POOL_MEMBER_FIELD, json!("chatgpt"));

    assert_eq!(current_member_id(&cfg, &meta).unwrap(), "chatgpt");
    assert_eq!(current_member_id(&cfg, &singleton_pool_meta).unwrap(), "chatgpt");
    assert_eq!(
      turn_member(&features, &cfg, &meta).unwrap(),
      turn_member(&features, &cfg, &singleton_pool_meta).unwrap()
    );
    assert!(meta.extra(POOL_MEMBER_FIELD).is_none_or(Value::is_null));

    availability(&features)
      .record(
        &loaded_cfg(&features),
        &features.registry,
        "chatgpt",
        None,
        backend_availability::REASON_QUOTA_ERROR,
        "spent",
      )
      .unwrap();
    assert_eq!(current_member_id(&cfg, &meta).unwrap(), "chatgpt", "current member resolution ignores availability");
    assert_eq!(current_member_id(&cfg, &singleton_pool_meta).unwrap(), "chatgpt");
    assert_eq!(turn_member(&features, &cfg, &meta).unwrap(), None);
    assert_eq!(turn_member(&features, &cfg, &singleton_pool_meta).unwrap(), None);
    assert!(meta.extra(POOL_MEMBER_FIELD).is_none_or(Value::is_null));
  }

  #[tokio::test]
  async fn a_cooling_model_session_refuses_a_turn_without_recording_pool_metadata() {
    let (_home, features) = fixture();
    let cfg = loaded_cfg(&features);
    let mut meta = pool_session(&features, "model-cooling-turn", "chatgpt").await;
    availability(&features)
      .record(
        &loaded_cfg(&features),
        &features.registry,
        "chatgpt",
        None,
        backend_availability::REASON_QUOTA_ERROR,
        "spent",
      )
      .unwrap();

    assert_eq!(turn_member(&features, &cfg, &meta).unwrap(), None);
    let TurnStart::Exhausted(message) = start_turn(&features, &cfg, &mut meta).await.unwrap() else {
      panic!("a cooling model must exhaust its one-member selection")
    };
    assert!(message.contains("- chatgpt: quota_error"), "{message}");
    assert!(meta.extra(POOL_MEMBER_FIELD).is_none_or(Value::is_null));
  }

  #[tokio::test]
  async fn turn_member_is_the_member_a_turn_start_records_and_a_read_records_nothing() {
    let (_home, features) = fixture();
    let cfg = loaded_cfg(&features);
    let mut meta = pool_session(&features, "pool-turn-member", "auto").await;
    assert_eq!(turn_member(&features, &cfg, &meta).unwrap().as_deref(), Some("chatgpt"));
    let stored = features.store.read_metadata_fresh(&meta.id).unwrap().unwrap();
    assert_eq!(stored.extra(POOL_MEMBER_FIELD).and_then(Value::as_str), None, "a read records no member");

    start_turn(&features, &cfg, &mut meta).await.unwrap();
    assert_eq!(current_member_id(&cfg, &meta).unwrap(), "chatgpt");

    availability(&features)
      .record(
        &loaded_cfg(&features),
        &features.registry,
        "chatgpt",
        None,
        backend_availability::REASON_QUOTA_ERROR,
        "spent",
      )
      .unwrap();
    assert_eq!(turn_member(&features, &cfg, &meta).unwrap().as_deref(), Some("claude-sub"));
    let stored = features.store.read_metadata_fresh(&meta.id).unwrap().unwrap();
    assert_eq!(stored.extra(POOL_MEMBER_FIELD), Some(&json!("chatgpt")), "a read leaves the recorded member in place");

    let change = changed(start_turn(&features, &cfg, &mut meta).await.unwrap());
    assert_eq!((change.from.as_str(), change.to.as_str()), ("chatgpt", "claude-sub"));
    assert_eq!(current_member_id(&cfg, &meta).unwrap(), "claude-sub");
  }

  #[tokio::test]
  async fn a_read_of_a_rotate_session_moves_no_position() {
    let (_home, features) = fixture();
    let cfg = loaded_cfg(&features);
    let meta = pool_session(&features, "pool-rotate-read", "light").await;
    assert_eq!(turn_member(&features, &cfg, &meta).unwrap().as_deref(), Some("payg-a"));
    assert_eq!(turn_member(&features, &cfg, &meta).unwrap().as_deref(), Some("payg-a"));
    assert_eq!(pick(&features, "light", &[]), Some("payg-a".to_string()));
  }
}
