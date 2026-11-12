//! The model pool: the one selection that picks the member a call runs on, under either rule of a `backends.pools`
//! entry.
//!
//! A pool names `backends.options` ids and a rule. A priority pool picks its best selectable member; a rotate pool
//! picks the first selectable member after its rotation position. Both rules skip a member the caller excludes, that
//! [`BackendAvailability`] holds out, or whose usage reading says it is exhausted. A member one pool recorded out is
//! skipped by every pool that names it. A session whose backend is a pool id runs each turn on the member this module
//! picks, and records that member in its `pool_member` slot.
//!
//! Terms: a *member* is a `backends.options` id that a pool names; *out* is the "out until" record that
//! [`BackendAvailability`] keeps per member. A *position* is a rotate pool's place in its member list, kept in process
//! memory, one per pool id.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use anyhow::anyhow;
use charliebot_core::config::{BackendOption, CharlieBotConfig, PoolBilling, PoolConfig, PoolMember, PoolRule};
use charliebot_core::{event_types, metadata_slot};
use charliebot_registry::backend::MemberReading;
use charliebot_registry::context::FeatureContext;
use charliebot_registry::{Registry, RegistryError};
use charliebot_store::Store;
use charliebot_store::metadata::{SessionMetadata, UtcDateTime};
use serde_json::{Map, Value, json};

use crate::backend_availability::{self, BackendAvailability};

/// The owner of the session metadata slot holding the member a pool session runs on.
pub const OWNER: &str = "model_pool";

/// The session metadata field holding the member a pool session's current turn runs on.
pub const POOL_MEMBER_FIELD: &str = "pool_member";

/// The id of the ordered pool a router's answer list makes. It names no `backends.pools` entry: the answer's order is
/// the rotation, and the position it starts from is always the first member.
pub const ROUTE_POOL_ID: &str = "route";

/// One pool as the selection reads it: its id, its rule and its members in config order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelPool {
  pub id: String,
  pub rule: PoolRule,
  pub members: Vec<PoolMember>,
}

/// What one selection reads: the config, the registry whose usage readings it consults, the availability owner, the
/// pool, the ids the caller excludes, and the moment it reads at. It also reads the active sessions each member holds.
pub struct Selection<'a> {
  cfg: &'a CharlieBotConfig,
  registry: &'a Registry,
  availability: &'a BackendAvailability,
  pool: &'a ModelPool,
  excluded: &'a [String],
  now: UtcDateTime,
  /// How many active sessions each member holds, which orders a priority pool's subscription tier; empty when the
  /// caller keeps no such count.
  member_loads: &'a HashMap<String, usize>,
}

impl<'a> Selection<'a> {
  /// A selection that reads no load counts, for a caller that keeps none: a worker's or a route's pick, or a pool whose
  /// order alone decides.
  pub fn new(
    cfg: &'a CharlieBotConfig,
    registry: &'a Registry,
    availability: &'a BackendAvailability,
    pool: &'a ModelPool,
    excluded: &'a [String],
    now: UtcDateTime,
  ) -> Self {
    Self { cfg, registry, availability, pool, excluded, now, member_loads: &NO_LOADS }
  }

  /// The same selection, reading the load counts `member_loads`.
  pub fn with_loads(self, member_loads: &'a HashMap<String, usize>) -> Self {
    Self { member_loads, ..self }
  }
}

/// The load counts of a selection that reads none.
static NO_LOADS: LazyLock<HashMap<String, usize>> = LazyLock::new(HashMap::new);

/// The one selection every caller makes: the process-wide owners a member pick reads, plus the store whose active
/// sessions order a priority pool when the caller has one.
///
/// A caller with a [`FeatureContext`] builds one with [`Selector::from_features`], so a priority pool orders its
/// subscription tier by the active sessions each member holds. A caller that holds only the process-wide owners (a CLI
/// one-shot call) builds one with [`Selector::new`], and reads the pool's configured order.
pub struct Selector<'a> {
  cfg: &'a CharlieBotConfig,
  registry: &'a Registry,
  availability: Arc<BackendAvailability>,
  positions: Arc<RotationPositions>,
  store: Option<&'a Store>,
}

impl<'a> Selector<'a> {
  /// A selection over the given owners that reads no session load counts.
  pub fn new(
    cfg: &'a CharlieBotConfig,
    registry: &'a Registry,
    availability: Arc<BackendAvailability>,
    positions: Arc<RotationPositions>,
  ) -> Self {
    Self { cfg, registry, availability, positions, store: None }
  }

  /// The same selection, ordering a priority pool by the active sessions `store` records for each member.
  pub fn with_store(mut self, store: &'a Store) -> Self {
    self.store = Some(store);
    self
  }

  /// The selection over the owners a server process holds in its startup state.
  pub fn from_features(features: &'a FeatureContext, cfg: &'a CharlieBotConfig) -> anyhow::Result<Self> {
    Ok(Self {
      cfg,
      registry: &features.registry,
      availability: backend_availability::backend_availability(features)?,
      positions: features.startup.require::<RotationPositions>()?,
      store: Some(&features.store),
    })
  }

  /// The option a call on `pool_id` runs on, or `None` when no member is selectable. A rotate pool's position moves
  /// past the selected member.
  pub fn select(&self, pool_id: &str, excluded: &[String]) -> anyhow::Result<Option<BackendOption>> {
    let pool = pool_by_id(self.cfg, pool_id)?;
    let loads = self.loads(&pool)?;
    let selection = Selection::new(self.cfg, self.registry, &self.availability, &pool, excluded, UtcDateTime::now())
      .with_loads(&loads);
    Ok(self.option(self.positions.select(&selection)?))
  }

  /// The option the next call on `pool_id` would run on, without moving a rotate pool's position.
  pub fn peek(&self, pool_id: &str, excluded: &[String]) -> anyhow::Result<Option<BackendOption>> {
    let pool = pool_by_id(self.cfg, pool_id)?;
    let loads = self.loads(&pool)?;
    let selection = Selection::new(self.cfg, self.registry, &self.availability, &pool, excluded, UtcDateTime::now())
      .with_loads(&loads);
    Ok(self.option(self.positions.peek(&selection)?))
  }

  /// The first selectable option in the router's ordered `backends`, without moving any rotation position. A retired
  /// ordered route keeps resolving through this call; a new router answer names one id.
  pub fn ordered(&self, backends: &[String], excluded: &[String]) -> anyhow::Result<Option<BackendOption>> {
    let pool = ordered_pool(backends);
    let mut position = 0;
    let selection = Selection::new(self.cfg, self.registry, &self.availability, &pool, excluded, UtcDateTime::now());
    Ok(self.option(select(&selection, Some(&mut position))?))
  }

  /// The error that ends a call when no member of `pool_id` is selectable: one line per member.
  pub fn exhausted_message(&self, pool_id: &str) -> anyhow::Result<String> {
    let pool = pool_by_id(self.cfg, pool_id)?;
    all_exhausted_message(self.cfg, self.registry, &self.availability, &pool)
  }

  /// Whether the `backends.options` id `backend_id` can run now.
  pub fn option_available(&self, backend_id: &str, excluded: &[String]) -> anyhow::Result<bool> {
    let member = PoolMember { backend: backend_id.to_string() };
    Ok(candidate(self.cfg, self.registry, &self.availability, &member, excluded, UtcDateTime::now())?.is_some())
  }

  /// The option a selected member names. Every member is checked against `backends.options` at selection.
  fn option(&self, member: Option<PoolMember>) -> Option<BackendOption> {
    member.map(|member| {
      self.cfg.get_backend_option(&member.backend).expect("a pool selection returns a backends.options id").clone()
    })
  }

  /// The counts a priority pool orders by, or none for a rotate pool and for a selector without a store.
  fn loads(&self, pool: &ModelPool) -> anyhow::Result<HashMap<String, usize>> {
    match (pool.rule, self.store) {
      (PoolRule::Priority, Some(store)) => member_loads_in(store, pool),
      _ => Ok(HashMap::new()),
    }
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

/// The pool that the `backends.pools` entry `id` configures, or the refusal that names the missing entry.
pub fn pool_by_id(cfg: &CharlieBotConfig, id: &str) -> anyhow::Result<ModelPool> {
  cfg
    .backends
    .pool(id)
    .map(ModelPool::from_config)
    .ok_or_else(|| anyhow!("backend '{id}' names no backends.pools entry and no backends.options id"))
}

/// A rotate pool over `backends` in the router's order, for an answer list that keeps no position of its own.
pub(crate) fn ordered_pool(backends: &[String]) -> ModelPool {
  ModelPool {
    id: ROUTE_POOL_ID.to_string(),
    rule: PoolRule::Rotate,
    members: backends.iter().map(|backend| PoolMember { backend: backend.clone() }).collect(),
  }
}

/// The rotation positions of the rotate pools, one per pool id. A position lives in process memory: a restart or a
/// new CLI process starts every rotation from its first member.
#[derive(Default)]
pub struct RotationPositions {
  positions: Mutex<HashMap<String, usize>>,
}

impl RotationPositions {
  /// Select the member `selection`'s pool runs a call on and move the pool's position past it. The position is read
  /// under the lock, the selection reads usage without the lock, and the advanced position is written under the lock in
  /// one step.
  pub fn select(&self, selection: &Selection<'_>) -> anyhow::Result<Option<PoolMember>> {
    let mut position = self.position(&selection.pool.id);
    let selected = select(selection, Some(&mut position))?;
    self.set_position(&selection.pool.id, position);
    Ok(selected)
  }

  /// The member `select` would pick now, without moving the pool's position. A host gate and a turn read use it, so
  /// reading a member leaves the rotation where it was.
  pub fn peek(&self, selection: &Selection<'_>) -> anyhow::Result<Option<PoolMember>> {
    let mut position = self.position(&selection.pool.id);
    select(selection, Some(&mut position))
  }

  fn position(&self, pool_id: &str) -> usize {
    let positions = self.positions.lock().expect("rotation position mutex is not poisoned");
    positions.get(pool_id).copied().unwrap_or(0)
  }

  fn set_position(&self, pool_id: &str, position: usize) {
    let mut positions = self.positions.lock().expect("rotation position mutex is not poisoned");
    positions.insert(pool_id.to_string(), position);
  }
}

/// Select the member a call runs on, or `None` when the pool has no selectable member.
///
/// A selectable member is one `excluded` does not name, that is not out per [`BackendAvailability`], and whose usage
/// reading does not say it is exhausted. A member whose backend type exposes no reading is selectable. A
/// [`PoolRule::Priority`] pool orders its selectable members. A [`PoolRule::Rotate`] pool walks from `position` in
/// member order and advances `position` past the member it selects, so `position` is required for a rotate pool.
pub(crate) fn select(selection: &Selection<'_>, position: Option<&mut usize>) -> anyhow::Result<Option<PoolMember>> {
  let Selection { cfg, registry, availability, pool, excluded, now, member_loads } = *selection;
  match pool.rule {
    PoolRule::Priority => select_priority(cfg, registry, availability, pool, excluded, now, member_loads),
    PoolRule::Rotate => {
      let position = position.ok_or_else(|| anyhow!("a rotate pool needs the rotation position to advance"))?;
      select_rotate(cfg, registry, availability, pool, position, excluded, now)
    }
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

/// Pick the least-assigned member from the subscription tier that the quota readings allow.
fn select_priority(
  cfg: &CharlieBotConfig,
  registry: &Registry,
  availability: &BackendAvailability,
  pool: &ModelPool,
  excluded: &[String],
  now: UtcDateTime,
  member_loads: &HashMap<String, usize>,
) -> anyhow::Result<Option<PoolMember>> {
  let mut candidates = Vec::new();
  for member in &pool.members {
    if let Some(candidate) = candidate(cfg, registry, availability, member, excluded, now)? {
      candidates.push(candidate);
    }
  }

  let subscriptions = candidates
    .iter()
    .filter(|candidate| member_billing(cfg, &candidate.member) == Some(PoolBilling::Subscription))
    .collect::<Vec<_>>();
  if !subscriptions.is_empty() {
    let under_line = subscriptions
      .iter()
      .filter(|candidate| !near_exhaustion(candidate.reading.as_ref()))
      .copied()
      .collect::<Vec<_>>();
    let tier = if under_line.is_empty() { subscriptions } else { under_line };
    return Ok(
      tier
        .iter()
        .enumerate()
        .min_by_key(|(config_order, candidate)| {
          (member_loads.get(&candidate.member.backend).copied().unwrap_or_default(), *config_order)
        })
        .map(|(_, candidate)| candidate.member.clone()),
    );
  }

  Ok(
    candidates
      .iter()
      .find(|candidate| member_billing(cfg, &candidate.member) == Some(PoolBilling::Payg))
      .map(|candidate| candidate.member.clone()),
  )
}

/// Pick from a rotate pool: the first selectable member after `position`, then move `position` past it.
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
  /// The pool whose member changed; the `quota_failover` event's `set`.
  pub pool: String,
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
  fn at_turn_start(pool: &str, from: String, to: String, reason: &'static str, recovers_at: Option<String>) -> Self {
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
  /// The session moves to `member`, which the turn start records. `change` is the event to log; a first assignment
  /// logs none.
  Move { member: String, change: Option<MemberChange> },
  /// No member is selectable: the turn ends with this error.
  Exhausted(String),
}

/// Choose the member for a turn that is about to dispatch, record it, and return the change to log.
///
/// A session whose backend is not a pool id is left alone. A session with no recorded member, or whose member is out,
/// gets a fresh selection; the first assignment records the member without a change event. A rotate pool's session
/// otherwise keeps its member until the member is out or its reading is exhausted. A priority pool's session keeps its
/// member unless it is on pay-as-you-go and a subscription is selectable (a subscription reset). It also switches when
/// it is near exhaustion and an under-line subscription is selectable. When no member is selectable the turn ends
/// with the error that lists every member.
pub async fn start_turn(
  features: &FeatureContext,
  cfg: &CharlieBotConfig,
  meta: &mut SessionMetadata,
) -> anyhow::Result<TurnStart> {
  if cfg.backends.pool(&meta.backend).is_none() {
    return Ok(TurnStart::Kept);
  }
  match decide_turn(features, cfg, meta, true)? {
    TurnDecision::Keep(_) => Ok(TurnStart::Kept),
    TurnDecision::Move { member, change } => {
      record_member(features, meta, &member).await?;
      Ok(change.map_or(TurnStart::Kept, TurnStart::Changed))
    }
    TurnDecision::Exhausted(message) => Ok(TurnStart::Exhausted(message)),
  }
}

/// The member a turn of the session runs on, as the turn start decides it, read without recording anything or moving
/// a rotation position. The cron host gate asks it before a fire, so the gate judges the member the fire then runs on.
/// `None` for a session whose backend is not a pool id, and when no member is selectable: the turn then ends with the
/// exhausted error and runs nothing.
pub fn turn_member(
  features: &FeatureContext,
  cfg: &CharlieBotConfig,
  meta: &SessionMetadata,
) -> anyhow::Result<Option<String>> {
  if cfg.backends.pool(&meta.backend).is_none() {
    return Ok(None);
  }
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
  let pool = pool_by_id(cfg, &meta.backend)?;
  let availability = backend_availability::backend_availability(features)?;
  let positions = features.startup.require::<RotationPositions>()?;
  let now = UtcDateTime::now();
  let member_loads = member_loads(features, &pool)?;
  let selection = Selection::new(cfg, &features.registry, &availability, &pool, &[], now).with_loads(&member_loads);
  let current = meta
    .extra(POOL_MEMBER_FIELD)
    .and_then(Value::as_str)
    .filter(|backend_id| pool.member(backend_id).is_some())
    .map(str::to_string);

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
      PoolRule::Priority => decide_keep_or_switch(features, &selection, &positions, current),
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
      Some(MemberChange::at_turn_start(
        &selection.pool.id,
        from.to_string(),
        selected.backend.clone(),
        event_types::QUOTA_REASON_QUOTA_ERROR,
        recovers_at,
      ))
    }
  };
  Ok(TurnDecision::Move { member: selected.backend, change })
}

/// Apply the priority turn-start rules to a session whose current member is selectable.
fn decide_keep_or_switch(
  features: &FeatureContext,
  selection: &Selection<'_>,
  positions: &RotationPositions,
  current: &str,
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
    (event_types::QUOTA_REASON_SUBSCRIPTION_RESET, None)
  } else if current_reading.as_ref().is_some_and(|reading| reading.exhausted) {
    // The reading says the member has no capacity left; the turn start leaves it, as an out record would.
    let reset = current_reading.as_ref().and_then(MemberReading::earliest_reset).map(format_minutes);
    (event_types::QUOTA_REASON_QUOTA_ERROR, reset)
  } else if near_exhaustion(current_reading.as_ref())
    && member_billing(selection.cfg, &selected) == Some(PoolBilling::Subscription)
    && !near_exhaustion(selected_reading.as_ref())
  {
    let reset = current_reading.as_ref().and_then(MemberReading::earliest_reset).map(format_minutes);
    (event_types::QUOTA_REASON_NEAR_EXHAUSTION, reset)
  } else {
    return Ok(TurnDecision::Keep(current.to_string()));
  };
  Ok(TurnDecision::Move {
    member: selected.backend.clone(),
    change: Some(MemberChange::at_turn_start(
      &selection.pool.id,
      current.to_string(),
      selected.backend,
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
    let state = match availability.row(&member.backend).filter(|row| row.out_until.into_inner() > now.into_inner()) {
      Some(row) => {
        let detail = row.detail.lines().next().unwrap_or_default().trim();
        let recovery = format!("{}, recovers at {}", row.reason, format_minutes(row.out_until));
        if detail.is_empty() { recovery } else { format!("{recovery} ({detail})") }
      }
      None => match member_reading(cfg, registry, &member.backend, now)? {
        Some(reading) if reading.exhausted => reading.earliest_reset().map_or_else(
          || "quota exhausted".to_string(),
          |reset| format!("quota exhausted, resets at {}", format_minutes(reset)),
        ),
        _ => "not selectable".to_string(),
      },
    };
    let label = match (pool.rule, member_billing(cfg, member)) {
      (PoolRule::Priority, Some(billing)) => format!(" ({})", billing_label(billing)),
      _ => String::new(),
    };
    lines.push(format!("- {}{label}: {state}", member.backend));
  }
  Ok(format!("pool '{}' has no selectable member:\n{}", pool.id, lines.join("\n")))
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

/// The active sessions each member of `pool` holds: the counts a priority pool's subscription tier is ordered by. A
/// rotate pool keeps its configured order, so it reads no count.
pub fn member_loads(features: &FeatureContext, pool: &ModelPool) -> anyhow::Result<HashMap<String, usize>> {
  member_loads_in(&features.store, pool)
}

/// The same counts, read from `store` alone, for a caller that holds no [`FeatureContext`].
pub fn member_loads_in(store: &Store, pool: &ModelPool) -> anyhow::Result<HashMap<String, usize>> {
  match pool.rule {
    PoolRule::Priority => Ok(store.active_session_string_metadata_counts(&pool.id, POOL_MEMBER_FIELD)?),
    PoolRule::Rotate => Ok(HashMap::new()),
  }
}

/// The member a session whose backend is a pool id runs on now, for the resolver. Every turn records one before it
/// runs, so a pool session with no recorded member is a refusal, and a recorded member the pool no longer lists is too.
pub fn current_member_id(cfg: &CharlieBotConfig, meta: &SessionMetadata) -> anyhow::Result<String> {
  let pool = pool_by_id(cfg, &meta.backend)?;
  let member =
    meta.extra(POOL_MEMBER_FIELD).and_then(Value::as_str).filter(|backend_id| !backend_id.is_empty()).ok_or_else(
      || {
        anyhow!(
          "session backend '{}' has no recorded {POOL_MEMBER_FIELD}; turn start records one before every run",
          pool.id
        )
      },
    )?;
  if pool.member(member).is_none() {
    return Err(anyhow!(
      "session backend '{}' records {POOL_MEMBER_FIELD} '{member}', which config.yaml backends.pools no longer lists",
      pool.id
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
      anyhow::bail!("the model pool test backend is not launched")
    }
  }

  /// The reading of a synthetic member: the canned windows the selection tests order.
  fn test_reading(option: &BackendOption, now: UtcDateTime) -> Option<MemberReading> {
    let reset = |minutes: i64| Some(UtcDateTime::from(now.into_inner() + time::Duration::minutes(minutes)));
    let weekly = |utilization: f64, minutes: i64| MemberReading {
      windows: vec![MemberWindow { utilization, window_minutes: Some(10_080), resets_at: reset(minutes) }],
      weekly_resets_at: reset(minutes),
      near_exhaustion: false,
      exhausted: false,
    };
    match option.id.as_str() {
      "chatgpt" => Some(weekly(0.80, 360)),
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
";

  /// The priority pool `auto` over five subscriptions, for the tests that count the sessions each member holds.
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
    let pool = pool_by_id(&cfg, id).unwrap();
    let availability = availability(features);
    let selection = Selection::new(&cfg, &features.registry, &availability, &pool, &excluded, UtcDateTime::now());
    positions(features).select(&selection).unwrap().map(|member| member.backend)
  }

  /// The priority pool picks subscription members first, and the pay-as-you-go member only as the last resort.
  #[tokio::test]
  async fn subscription_tiers_precede_pay_as_you_go() {
    let (_home, features) = fixture();
    // Trade-off 3's example: ChatGPT (20% left, 6h), Claude (51%, 2 days), Devin (no reading).
    assert_eq!(pick(&features, "auto", &[]), Some("chatgpt".to_string()));
    assert_eq!(pick(&features, "auto", &["chatgpt"]), Some("claude-sub".to_string()));
    assert_eq!(pick(&features, "auto", &["chatgpt", "claude-sub"]), Some("devin".to_string()));
    // A subscription at or above the line is the last resort, before the first pay-as-you-go member.
    assert_eq!(pick(&features, "auto", &["chatgpt", "claude-sub", "devin"]), Some("near".to_string()));
    assert_eq!(pick(&features, "auto", &["chatgpt", "claude-sub", "devin", "near"]), Some("payg-a".to_string()));
    assert_eq!(pick(&features, "auto", &["chatgpt", "claude-sub", "devin", "near", "payg-a"]), None);
  }

  #[tokio::test]
  async fn new_auto_sessions_take_the_least_assigned_member_in_turn() {
    let (_home, features) = fixture_with(ROTATION_CONFIG);
    let cfg = loaded_cfg(&features);
    let expected = ["member-a", "member-b", "member-c", "member-d", "member-e", "member-a"];

    for (index, expected_member) in expected.into_iter().enumerate() {
      let mut meta = pool_session(&features, &format!("pool-rotation-{index}"), "auto").await;
      let gate_member = turn_member(&features, &cfg, &meta).unwrap();
      assert_eq!(gate_member.as_deref(), Some(expected_member));

      start_turn(&features, &cfg, &mut meta).await.unwrap();
      assert_eq!(current_member_id(&cfg, &meta).unwrap(), expected_member);
    }
  }

  #[tokio::test]
  async fn archived_and_other_backend_sessions_do_not_add_member_load() {
    let (_home, features) = fixture_with(ROTATION_CONFIG);
    let cfg = loaded_cfg(&features);
    features.store.active_session_string_metadata_counts("auto", POOL_MEMBER_FIELD).unwrap();

    let active = pool_session(&features, "pool-load-active", "auto").await;
    features
      .store
      .update_slot_fields(&active.id, OWNER, Map::from_iter([(POOL_MEMBER_FIELD.to_string(), json!("member-a"))]))
      .await
      .unwrap();

    let archived = pool_session(&features, "pool-load-archived", "auto").await;
    features
      .store
      .update_slot_fields(&archived.id, OWNER, Map::from_iter([(POOL_MEMBER_FIELD.to_string(), json!("member-b"))]))
      .await
      .unwrap();
    features.store.archive_session(&archived.id).await.unwrap();

    let other_backend = pool_session(&features, "pool-load-other-backend", "member-a").await;
    features
      .store
      .update_slot_fields(
        &other_backend.id,
        OWNER,
        Map::from_iter([(POOL_MEMBER_FIELD.to_string(), json!("member-b"))]),
      )
      .await
      .unwrap();

    let counts = features.store.active_session_string_metadata_counts("auto", POOL_MEMBER_FIELD).unwrap();
    assert_eq!(counts.get("member-a"), Some(&1));
    assert!(!counts.contains_key("member-b"));

    let fresh = pool_session(&features, "pool-load-probe", "auto").await;
    assert_eq!(turn_member(&features, &cfg, &fresh).unwrap().as_deref(), Some("member-b"));
  }

  #[tokio::test]
  async fn under_line_subscriptions_win_over_a_less_assigned_near_exhausted_member() {
    let (_home, features) = fixture();
    let cfg = loaded_cfg(&features);
    for (index, member) in ["chatgpt", "chatgpt", "claude-sub", "devin"].into_iter().enumerate() {
      let session = pool_session(&features, &format!("pool-load-under-line-{index}"), "auto").await;
      features
        .store
        .update_slot_fields(&session.id, OWNER, Map::from_iter([(POOL_MEMBER_FIELD.to_string(), json!(member))]))
        .await
        .unwrap();
    }

    let fresh = pool_session(&features, "pool-load-under-line-probe", "auto").await;
    assert_eq!(turn_member(&features, &cfg, &fresh).unwrap().as_deref(), Some("claude-sub"));
  }

  #[tokio::test]
  async fn an_out_member_is_skipped_and_an_exhausted_reading_is_skipped() {
    let (_home, features) = fixture();
    availability(&features).record("chatgpt", None, backend_availability::REASON_QUOTA_ERROR, "spent").unwrap();
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
      let pool = pool_by_id(&cfg, "light").unwrap();
      let selection = Selection::new(&cfg, &features.registry, &availability, &pool, &excluded, now);
      positions(features).select(&selection).unwrap().map(|member| member.backend)
    };

    // `spent`'s reading says it is exhausted; `payg-a` exposes no reading and is selectable.
    assert_eq!(rotate(&features, &[]), Some("payg-a".to_string()));
    assert_eq!(rotate(&features, &["payg-a"]), Some("chatgpt".to_string()), "a member the caller excludes is skipped");

    availability.record("payg-a", None, backend_availability::REASON_QUOTA_ERROR, "spent").unwrap();
    assert_eq!(rotate(&features, &[]), Some("chatgpt".to_string()), "a member the owner recorded out is skipped");

    availability.record("chatgpt", None, backend_availability::REASON_QUOTA_ERROR, "spent").unwrap();
    assert_eq!(rotate(&features, &[]), None);
  }

  #[tokio::test]
  async fn a_member_both_pools_share_is_skipped_by_both_once_recorded_out() {
    let (_home, features) = fixture();
    // `chatgpt` is a member of the priority pool and of the rotate pool; one out record covers both.
    availability(&features).record("chatgpt", None, backend_availability::REASON_QUOTA_ERROR, "spent").unwrap();
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
    let light = pool_by_id(&cfg, "light").unwrap();
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
    assert_eq!(event["set"], "auto", "the set names the pool that switched");
    assert_eq!(meta.extra(POOL_MEMBER_FIELD), Some(&json!("claude-sub")));
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

    availability(&features).record("payg-a", None, backend_availability::REASON_QUOTA_ERROR, "spent").unwrap();
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
      availability.record(member, None, backend_availability::REASON_QUOTA_ERROR, "spent").unwrap();
    }
    start_turn(&features, &cfg, &mut meta).await.unwrap();
    assert_eq!(meta.extra(POOL_MEMBER_FIELD), Some(&json!("payg-a")));

    // Their out records expire, so turn start returns the session to the subscription tier.
    let expired = UtcDateTime::from(time::OffsetDateTime::now_utc() - time::Duration::hours(1));
    for member in ["chatgpt", "claude-sub", "near", "devin"] {
      availability.record(member, Some(expired), backend_availability::REASON_QUOTA_ERROR, "spent").unwrap();
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
      availability.record(member, Some(reset), backend_availability::REASON_QUOTA_ERROR, "API Error: 402").unwrap();
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
  async fn turn_member_is_the_member_a_turn_start_records_and_a_read_records_nothing() {
    let (_home, features) = fixture();
    let cfg = loaded_cfg(&features);
    let mut meta = pool_session(&features, "pool-turn-member", "auto").await;
    assert_eq!(turn_member(&features, &cfg, &meta).unwrap().as_deref(), Some("chatgpt"));
    let stored = features.store.read_metadata_fresh(&meta.id).unwrap().unwrap();
    assert_eq!(stored.extra(POOL_MEMBER_FIELD).and_then(Value::as_str), None, "a read records no member");

    start_turn(&features, &cfg, &mut meta).await.unwrap();
    assert_eq!(current_member_id(&cfg, &meta).unwrap(), "chatgpt");

    availability(&features).record("chatgpt", None, backend_availability::REASON_QUOTA_ERROR, "spent").unwrap();
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
