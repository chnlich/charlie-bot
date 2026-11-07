//! The Auto session backend: the `auto_member` session slot and the per-turn member selection over `backends.auto`.
//!
//! A session whose backend is [`AUTO_BACKEND_ID`] runs each turn on one member of the configured set. Turn start
//! chooses the member before dispatch builds the run, records it in `auto_member`, and leaves one `quota_failover` chat
//! event per change. Everything that reads "the session's model" reads that member through the resolver.

use anyhow::anyhow;
use charliebot_core::config::{AUTO_BACKEND_ID, AutoBilling, AutoConfig, AutoMember, CharlieBotConfig};
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
}

impl MemberChange {
  /// The `quota_failover` chat event recording this change.
  pub fn event(&self) -> Value {
    Value::Object(Map::from_iter([
      ("type".to_string(), json!(event_types::QUOTA_FAILOVER)),
      ("from".to_string(), json!(self.from)),
      ("to".to_string(), json!(self.to)),
      (event_types::QUOTA_FAILOVER_REASON.to_string(), json!(self.reason)),
      (event_types::QUOTA_FAILOVER_SET.to_string(), json!(event_types::QUOTA_SET_AUTO)),
      ("recovers_at".to_string(), json!(self.recovers_at.clone().unwrap_or_default())),
    ]))
  }
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

/// Read one member's quota reading through its backend type's registered reader, or `None` when the type exposes none.
pub fn member_reading(
  cfg: &CharlieBotConfig,
  registry: &Registry,
  member: &AutoMember,
  now: UtcDateTime,
) -> anyhow::Result<Option<MemberReading>> {
  let option = cfg
    .get_backend_option(&member.backend)
    .ok_or_else(|| anyhow!("backends.auto names backend '{}', which is not a backends.options id", member.backend))?;
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

/// One selectable member with the reading selection ordered it by.
struct Candidate<'a> {
  member: &'a AutoMember,
  reading: Option<MemberReading>,
}

/// Select the member a turn runs on, or `None` when no member is selectable.
///
/// Candidates are the configured members that are neither out (per [`BackendAvailability`]) nor exhausted, and not in
/// `excluded`. Subscriptions below the 90% line come first, ordered by their weekly window's reset (earliest first),
/// with members that state no reset after them in config order. Subscriptions at or above the line are the last resort
/// and follow the same order. Only when no subscription is selectable does the first pay-as-you-go member in config
/// order run.
pub fn select_member(
  cfg: &CharlieBotConfig,
  registry: &Registry,
  availability: &BackendAvailability,
  excluded: &[String],
  now: UtcDateTime,
) -> anyhow::Result<Option<AutoMember>> {
  let auto = require_auto(cfg)?;
  let mut candidates = Vec::new();
  for member in &auto.members {
    if excluded.iter().any(|backend_id| backend_id == &member.backend) || is_out(availability, &member.backend, now) {
      continue;
    }
    let reading = member_reading(cfg, registry, member, now)?;
    if reading.as_ref().is_some_and(|reading| reading.exhausted) {
      continue;
    }
    candidates.push(Candidate { member, reading });
  }

  let subscriptions =
    candidates.iter().filter(|candidate| candidate.member.billing == AutoBilling::Subscription).collect::<Vec<_>>();
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

  Ok(candidates.iter().find(|candidate| candidate.member.billing == AutoBilling::Payg).map(|c| c.member.clone()))
}

/// Choose the member for a turn that is about to dispatch, record it, and return the change to log.
///
/// A session that is not on `auto` is left alone. A session with no recorded member, or whose member is out, gets a
/// fresh selection; the first assignment records the member without a change event. Otherwise the session keeps its
/// member unless it is on pay-as-you-go and a subscription is selectable (a subscription reset), or it is near
/// exhaustion and an under-line subscription is selectable.
pub async fn start_turn(
  features: &FeatureContext,
  cfg: &CharlieBotConfig,
  meta: &mut SessionMetadata,
) -> anyhow::Result<Option<MemberChange>> {
  if meta.backend != AUTO_BACKEND_ID {
    return Ok(None);
  }
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
    return keep_or_switch(features, cfg, meta, auto, &availability, current, now).await;
  }

  let selected = select_member(cfg, &features.registry, &availability, &[], now)?;
  let Some(selected) = selected else { return Ok(None) };
  if current.as_deref() == Some(selected.backend.as_str()) {
    return Ok(None);
  }
  record_member(features, meta, &selected.backend).await?;
  let Some(from) = current else {
    // The first assignment is not a change between members, so it leaves no failover event.
    return Ok(None);
  };
  Ok(Some(MemberChange {
    recovers_at: availability.out_until_minutes(&from),
    from,
    to: selected.backend,
    reason: event_types::QUOTA_REASON_QUOTA_ERROR,
  }))
}

/// Apply the turn-start rules 2 and 3 to a session whose current member is selectable.
async fn keep_or_switch(
  features: &FeatureContext,
  cfg: &CharlieBotConfig,
  meta: &mut SessionMetadata,
  auto: &AutoConfig,
  availability: &BackendAvailability,
  current: &str,
  now: UtcDateTime,
) -> anyhow::Result<Option<MemberChange>> {
  let Some(selected) = select_member(cfg, &features.registry, availability, &[], now)? else { return Ok(None) };
  if selected.backend == current {
    return Ok(None);
  }
  let current_member = auto.member(current).expect("the current member is in the configured set");
  let current_reading = member_reading(cfg, &features.registry, current_member, now)?;
  let selected_reading = member_reading(cfg, &features.registry, &selected, now)?;
  let (reason, recovers_at) =
    if current_member.billing == AutoBilling::Payg && selected.billing == AutoBilling::Subscription {
      (event_types::QUOTA_REASON_SUBSCRIPTION_RESET, None)
    } else if near_exhaustion(current_reading.as_ref())
      && selected.billing == AutoBilling::Subscription
      && !near_exhaustion(selected_reading.as_ref())
    {
      let reset = current_reading.as_ref().and_then(MemberReading::earliest_reset).map(format_minutes);
      (event_types::QUOTA_REASON_NEAR_EXHAUSTION, reset)
    } else {
      return Ok(None);
    };
  record_member(features, meta, &selected.backend).await?;
  Ok(Some(MemberChange { from: current.to_string(), to: selected.backend, reason, recovers_at }))
}

/// Persist `member` as the session's current member and mirror it into the in-memory metadata the resolver reads.
async fn record_member(features: &FeatureContext, meta: &mut SessionMetadata, member: &str) -> anyhow::Result<()> {
  let values = Map::from_iter([(AUTO_MEMBER_FIELD.to_string(), json!(member))]);
  let updated = features.store.update_slot_fields(&meta.id, OWNER, values).await?;
  if updated.is_none() {
    return Err(anyhow!("session '{}' disappeared while recording {AUTO_MEMBER_FIELD}", meta.id));
  }
  meta.set_extra(AUTO_MEMBER_FIELD, json!(member));
  tracing::info!(target: "charliebot_agents", session_id = %meta.id, member, "auto_member_recorded");
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
    - {id: devin, label: Devin, type: synthetic, model: model-devin}
    - {id: payg-a, label: Payg A, type: synthetic, model: model-payg}
  auto:
    label: Auto
    members:
      - {backend: chatgpt, billing: subscription}
      - {backend: claude-sub, billing: subscription}
      - {backend: near, billing: subscription}
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
    registry.register_package("test.auto_backend", register_test).unwrap();
    let registry = Arc::new(registry);
    let cfg = registry.load_config(home.path()).unwrap();
    let store = Arc::new(
      Store::new(cfg.clone(), registry.metadata_slots(), store_hooks::store_hooks(Arc::clone(&registry))).unwrap(),
    );
    let config_file = cfg.config_file();
    let config = Arc::new(HotReload::new(
      config_file,
      "auto_backend_test_config_reload_failed",
      Box::new(move || Ok(cfg.clone())),
    ));
    let credential_home = home.path().to_path_buf();
    let credentials = Arc::new(HotReload::new(
      home.path().join("credentials.yaml"),
      "auto_backend_test_credentials_reload_failed",
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

  #[tokio::test]
  async fn selection_orders_by_weekly_reset_then_config_order_then_pay_as_you_go() {
    let (_home, features) = fixture();
    let cfg = loaded_cfg(&features);
    let availability = availability(&features);
    let now = UtcDateTime::now();
    let select = |excluded: &[&str]| {
      let excluded = excluded.iter().map(|id| (*id).to_string()).collect::<Vec<_>>();
      select_member(&cfg, &features.registry, &availability, &excluded, now).unwrap().map(|member| member.backend)
    };

    // Trade-off 3's example: ChatGPT (20% left, 6h), Claude (51%, 2 days), Devin (no reading).
    assert_eq!(select(&[]), Some("chatgpt".to_string()));
    assert_eq!(select(&["chatgpt"]), Some("claude-sub".to_string()));
    assert_eq!(select(&["chatgpt", "claude-sub"]), Some("devin".to_string()));
    // A subscription at or above the line is the last resort, before the first pay-as-you-go member.
    assert_eq!(select(&["chatgpt", "claude-sub", "devin"]), Some("near".to_string()));
    assert_eq!(select(&["chatgpt", "claude-sub", "devin", "near"]), Some("payg-a".to_string()));
    assert_eq!(select(&["chatgpt", "claude-sub", "devin", "near", "payg-a"]), None);
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
  async fn turn_start_records_the_first_member_without_a_change_event_and_keeps_it_below_the_line() {
    let (_home, features) = fixture();
    let cfg = loaded_cfg(&features);
    let mut meta = auto_session(&features, "auto-first-turn").await;

    let change = start_turn(&features, &cfg, &mut meta).await.unwrap();
    assert_eq!(change, None, "the first assignment is not a change between members");
    assert_eq!(meta.extra(AUTO_MEMBER_FIELD), Some(&json!("chatgpt")));
    let stored = features.store.read_metadata_fresh(&meta.id).unwrap().unwrap();
    assert_eq!(stored.extra(AUTO_MEMBER_FIELD), Some(&json!("chatgpt")));
    assert!(features.store.load_all_facts(&meta.id).unwrap().iter().all(|event| event["type"] != "quota_failover"));

    // A member below the 90% line is kept at the next turn start.
    let change = start_turn(&features, &cfg, &mut meta).await.unwrap();
    assert_eq!(change, None);
    assert_eq!(meta.extra(AUTO_MEMBER_FIELD), Some(&json!("chatgpt")));
  }

  #[tokio::test]
  async fn turn_start_switches_off_an_out_member_with_a_quota_error_change() {
    let (_home, features) = fixture();
    let cfg = loaded_cfg(&features);
    let mut meta = auto_session(&features, "auto-out-member").await;
    start_turn(&features, &cfg, &mut meta).await.unwrap();
    availability(&features).record("chatgpt", None, backend_availability::REASON_QUOTA_ERROR, "spent").unwrap();

    let change = start_turn(&features, &cfg, &mut meta).await.unwrap().expect("the out member is left");
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
    let change = start_turn(&features, &cfg, &mut meta).await.unwrap().expect("the pay-as-you-go turn returns");
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

    let change = start_turn(&features, &cfg, &mut meta).await.unwrap().expect("the near member is left");
    assert_eq!((change.from.as_str(), change.to.as_str()), ("near", "chatgpt"));
    assert_eq!(change.reason, event_types::QUOTA_REASON_NEAR_EXHAUSTION);
    assert!(change.recovers_at.is_some());
    assert_eq!(change.event()["reason"], "near_exhaustion");
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
}
