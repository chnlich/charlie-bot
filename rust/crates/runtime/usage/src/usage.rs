//! The session usage owner, ported from `session_usage.py`.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_core::event_types::{self, EventFields, is_truthy};
use charliebot_core::hot_reload::HotReload;
use charliebot_registry::backend::{ContextLimits, UsageResolver, UsageResolverHost};
use charliebot_registry::{FeatureContext, Registry, RegistryError};
use charliebot_store::metadata::{SessionMetadata, TaskProfile, UtcDateTime};
use charliebot_store::run_records::RunKind;
use charliebot_store::{ChatEvent, Store, StoreError, StoreResult};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};

const READING_CLAUDE: &str = "claude";
const READING_SNAPSHOT: &str = "snapshot";
const READING_RESOLVED: &str = "resolved";
const FACTS_MEMO_CAP: usize = 8;

#[derive(Clone, Default)]
struct UsageFold {
  chosen_prompt_tokens: i64,
  chosen_model: String,
  post_compact_tokens: Option<i64>,
  model_windows: Map<String, Value>,
  snapshot: Option<Value>,
  reading_kind: String,
  reading: Option<Value>,
  total_cost: f64,
  unknown_cost: bool,
  last_request_at: Option<UtcDateTime>,
}

impl UsageFold {
  fn feed<E: EventFields>(&mut self, events: &[&E]) -> StoreResult<()> {
    for event in events {
      let event_type = (*event).field("type").and_then(EventFields::as_str_field);
      if matches!(event_type, Some(event_types::ASSISTANT | event_types::RESULT))
        && let Some(timestamp) = event.field("timestamp").and_then(EventFields::as_str_field)
        && let Ok(parsed) = UtcDateTime::parse(timestamp)
      {
        self.last_request_at = Some(parsed);
      }
      match event_type {
        Some(event_types::ASSISTANT) => self.feed_assistant(*event)?,
        Some(event_types::SYSTEM) => self.feed_system(*event)?,
        Some(event_types::CONTEXT_COMPACTED) if self.reading_kind == READING_CLAUDE => {
          self.feed_compaction_metadata(*event)?;
        }
        Some(event_types::RESULT) => self.feed_result(*event)?,
        _ => {}
      }
    }
    Ok(())
  }

  fn feed_assistant<E: EventFields>(&mut self, event: &E) -> StoreResult<()> {
    if truthy_field(event.field("parent_tool_use_id")) {
      return Ok(());
    }
    let Some(message) = truthy_object(event.field("message"), "assistant.message")? else { return Ok(()) };
    let Some(usage) = truthy_object(message.field("usage"), "assistant.message.usage")? else { return Ok(()) };
    let tokens = add_int_fields(
      usage,
      &[
        event_types::USAGE_INPUT_TOKENS,
        event_types::USAGE_CACHE_CREATION_INPUT_TOKENS,
        event_types::USAGE_CACHE_READ_INPUT_TOKENS,
      ],
    )?;
    if tokens > 0 {
      self.chosen_prompt_tokens = tokens;
      self.chosen_model =
        message.field("model").filter(|value| is_truthy(*value)).map_or_else(String::new, value_to_python_string);
      self.post_compact_tokens = None;
      self.reading_kind = READING_CLAUDE.to_string();
    }
    Ok(())
  }

  fn feed_system<E: EventFields>(&mut self, event: &E) -> StoreResult<()> {
    match event.field("subtype").and_then(EventFields::as_str_field) {
      Some(event_types::COMPACT_BOUNDARY) if self.reading_kind == READING_CLAUDE => {
        self.feed_compaction_metadata(event)?;
      }
      Some(event_types::CONTEXT_READING) => {
        if let Some(reading) = event.field(event_types::CONTEXT_READING).filter(|value| is_truthy(*value)) {
          self.reading_kind = READING_RESOLVED.to_string();
          self.reading = Some(reading.to_owned_value());
        }
      }
      _ => {}
    }
    Ok(())
  }

  fn feed_compaction_metadata<E: EventFields>(&mut self, event: &E) -> StoreResult<()> {
    let Some(compact) = truthy_object(event.field(event_types::COMPACT_METADATA), "compact_metadata")? else {
      return Ok(());
    };
    if let Some(tokens) = compact.field(event_types::COMPACT_POST_TOKENS).and_then(python_integer) {
      self.post_compact_tokens = Some(tokens);
    }
    Ok(())
  }

  fn feed_result<E: EventFields>(&mut self, event: &E) -> StoreResult<()> {
    if let Some(model_usage) = truthy_object(event.field("modelUsage"), "result.modelUsage")? {
      // One result event per run: its model-usage table converts once and reads as the Value form.
      let model_usage = model_usage.to_owned_value();
      let model_usage = model_usage.as_object().expect("a truthy modelUsage object reads as an object");
      for (model_name, info_value) in model_usage {
        let Some(info) = truthy_object(Some(info_value), "result.modelUsage entry")? else { continue };
        if let Some(window) = info.get("contextWindow").and_then(python_integer) {
          self.model_windows.insert(model_name.clone(), Value::from(window));
        }
      }
    }
    if let Some(snapshot) = event.field("context_snapshot").filter(|value| value.is_object()) {
      self.snapshot = Some(snapshot.to_owned_value());
      self.reading_kind = READING_SNAPSHOT.to_string();
    }
    match event.field(event_types::RESULT_TOTAL_COST_USD) {
      Some(value) if value.is_null() => self.unknown_cost = true,
      Some(value) => self.total_cost += python_number(&value.to_owned_value(), "result.total_cost_usd")?,
      None => {}
    }
    Ok(())
  }

  fn cost(&self) -> Option<f64> {
    if self.unknown_cost || self.total_cost <= 0.0 {
      None
    } else {
      // Python's round(total, 4) rounds the exact binary value, as {:.4} does and scaling by 10_000 does not.
      Some(format!("{:.4}", self.total_cost).parse().expect("a formatted float parses back"))
    }
  }
}

struct FactsMemoEntry {
  session_id: String,
  generation: u64,
  covered: usize,
  fold: UsageFold,
}

/// One owner for backend resolver precedence, event-fold tiers, and context state.
pub struct SessionUsage {
  config: Arc<HotReload<CharlieBotConfig>>,
  store: Arc<Store>,
  registry: Arc<Registry>,
  usage_resolvers: Mutex<HashMap<String, Option<Arc<dyn UsageResolver>>>>,
  facts_memo: Mutex<VecDeque<FactsMemoEntry>>,
}

impl SessionUsage {
  /// Create an owner bound to this server's reloaded config, store, and registrations.
  pub fn new(config: Arc<HotReload<CharlieBotConfig>>, store: Arc<Store>, registry: Arc<Registry>) -> SessionUsage {
    SessionUsage {
      config,
      store,
      registry,
      usage_resolvers: Mutex::new(HashMap::new()),
      facts_memo: Mutex::new(VecDeque::new()),
    }
  }

  /// Resolve one session's reading through its backend resolver, then the shared event-fold tiers.
  pub fn resolve_session_usage(
    &self,
    session_id: &str,
    meta: &SessionMetadata,
    backend_option: Option<&BackendOption>,
  ) -> StoreResult<Option<Value>> {
    self.resolve_with_facts(session_id, meta, backend_option).map(|(usage, _)| usage)
  }

  /// Resolve the event-only tiers for an already-loaded transcript, without a backend resolver or cross-call memo.
  pub fn resolve_events_usage<E: EventFields>(
    &self,
    events: &[&E],
    run_kind: RunKind,
    backend_option: Option<&BackendOption>,
  ) -> StoreResult<Option<Value>> {
    fold_usage(events, run_kind, &self.registry, backend_option)
  }

  /// Return the selected context reading and the latest assistant/result request time.
  pub fn context_state(
    &self,
    session_id: &str,
    meta: &SessionMetadata,
  ) -> StoreResult<(Option<i64>, Option<UtcDateTime>)> {
    let config = self.config.get().map_err(|error| StoreError::new(error.to_string()))?;
    let backend_option = config.get_backend_option(&meta.backend);
    let (usage, fold) = self.resolve_with_facts(session_id, meta, backend_option)?;
    let context_tokens = usage.as_ref().and_then(|usage| usage.get("context_tokens")).and_then(python_integer);
    Ok((context_tokens, fold.last_request_at))
  }

  fn resolve_with_facts(
    &self,
    session_id: &str,
    meta: &SessionMetadata,
    backend_option: Option<&BackendOption>,
  ) -> StoreResult<(Option<Value>, UsageFold)> {
    let (fold, event_count) = self.fold_cached(session_id)?;
    if let Some(resolver) = self.usage_resolver(&meta.backend)? {
      let events = self.store.load_chat_events_shared_sync(session_id)?;
      if let Some(usage) = resolver
        .resolve(session_id, meta, events.as_slice())
        .map_err(|error| StoreError::new(format!("usage resolver failed: {error:#}")))?
      {
        return Ok((Some(Value::Object(usage)), fold));
      }
    }
    Ok((resolve_fold_tier(&fold, &self.registry, event_count, usage_run_kind(meta), backend_option)?, fold))
  }

  fn usage_resolver(&self, backend_id: &str) -> StoreResult<Option<Arc<dyn UsageResolver>>> {
    let config = self.config.get().map_err(|error| StoreError::new(error.to_string()))?;
    let backend_type = config
      .get_backend_option(backend_id)
      .map(|option| option.backend_type.clone())
      .or_else(|| self.registry.type_for_family_prefix(backend_id).map(str::to_string));
    let Some(backend_type) = backend_type else {
      return Ok(None);
    };

    let mut resolvers = self.usage_resolvers.lock().expect("usage-resolver cache mutex is not poisoned");
    if let Some(resolver) = resolvers.get(&backend_type) {
      return Ok(resolver.clone());
    }
    let resolver = self.registry.usage_resolver_for(&backend_type).map(|constructor| {
      constructor(&UsageResolverHost { config: Arc::clone(&self.config), store: Arc::clone(&self.store) })
    });
    resolvers.insert(backend_type, resolver.clone());
    Ok(resolver)
  }

  fn fold_cached(&self, session_id: &str) -> StoreResult<(UsageFold, usize)> {
    let cached = {
      let mut memo = self.facts_memo.lock().expect("usage facts memo mutex is not poisoned");
      let index = memo.iter().position(|entry| entry.session_id == session_id);
      index.map(|index| {
        let entry = memo.remove(index).expect("memo index points at an entry");
        let cached = (entry.generation, entry.covered, entry.fold.clone());
        memo.push_back(entry);
        cached
      })
    };
    let delta = self.store.load_chat_events_delta_sync(
      session_id,
      cached.as_ref().map(|entry| entry.0),
      cached.as_ref().map_or(0, |entry| entry.1),
    )?;
    let mut fold = match cached {
      Some((generation, covered, fold)) if generation == delta.generation && covered == delta.start_index => fold,
      _ => UsageFold::default(),
    };
    let refs = delta.events[delta.start_index..].iter().map(Arc::as_ref).collect::<Vec<&ChatEvent>>();
    fold.feed(&refs)?;
    let memo_entry = FactsMemoEntry {
      session_id: session_id.to_string(),
      generation: delta.generation,
      covered: delta.event_count,
      fold: fold.clone(),
    };
    let mut memo = self.facts_memo.lock().expect("usage facts memo mutex is not poisoned");
    if let Some(index) = memo.iter().position(|entry| entry.session_id == session_id) {
      memo.remove(index);
    }
    memo.push_back(memo_entry);
    if memo.len() > FACTS_MEMO_CAP {
      memo.pop_front();
    }
    Ok((fold, delta.event_count))
  }
}

/// Register the one server-wide session usage owner in the runtime startup state.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.pre_recovery_step("session_usage", initialize)
}

fn initialize(features: Arc<FeatureContext>) -> BoxFuture<'static, anyhow::Result<()>> {
  Box::pin(async move {
    features.startup.insert(SessionUsage::new(
      Arc::clone(&features.config),
      Arc::clone(&features.store),
      Arc::clone(&features.registry),
    ))?;
    Ok(())
  })
}

/// Fold session events through the source tiers, without invoking a registered backend usage resolver.
pub fn fold_usage<E: EventFields>(
  events: &[&E],
  run_kind: RunKind,
  registry: &Registry,
  backend_option: Option<&BackendOption>,
) -> StoreResult<Option<Value>> {
  let mut fold = UsageFold::default();
  fold.feed(events)?;
  resolve_fold_tier(&fold, registry, events.len(), run_kind, backend_option)
}

/// The run kind the session usage view assigns to a node profile.
pub fn usage_run_kind(meta: &SessionMetadata) -> RunKind {
  if meta.profile == TaskProfile::Manager { RunKind::ManagerTurn } else { RunKind::Work }
}

fn resolve_fold_tier(
  fold: &UsageFold,
  registry: &Registry,
  event_count: usize,
  run_kind: RunKind,
  backend_option: Option<&BackendOption>,
) -> StoreResult<Option<Value>> {
  if event_count == 0 {
    return Ok(None);
  }
  let cost = fold.cost();
  let usage = match fold.reading_kind.as_str() {
    READING_CLAUDE => Some(resolve_claude(fold, registry, cost, run_kind, backend_option)?),
    READING_SNAPSHOT => Some(resolve_snapshot(fold, registry, cost, run_kind)?),
    READING_RESOLVED => Some(resolve_reading(fold, cost)),
    _ => Some(json!({
      "context_tokens": null,
      "context_full": null,
      (event_types::CONTEXT_COMPACT_AT): null,
      "total_cost_usd": cost,
      "model": "",
    })),
  };
  Ok(usage)
}

fn resolve_claude(
  fold: &UsageFold,
  registry: &Registry,
  cost: Option<f64>,
  run_kind: RunKind,
  backend_option: Option<&BackendOption>,
) -> StoreResult<Value> {
  let context_tokens = fold.post_compact_tokens.unwrap_or(fold.chosen_prompt_tokens);
  let limits = registry.reading_limits_for(READING_CLAUDE, run_kind, backend_option);
  let (context_full, compact_at) = match limits {
    Some(limits) => {
      let declared = limits.declared_window.map(|value| value as i64);
      let model_window = fold.model_windows.get(&fold.chosen_model).and_then(python_integer);
      let full = match (model_window, declared) {
        (Some(model), Some(declared)) => Some(model.min(declared)),
        (_, declared) => declared,
      };
      (full, subtract_reserve(full, limits))
    }
    None => (None, None),
  };
  Ok(usage_value(Some(context_tokens), context_full, compact_at, &fold.chosen_model, cost))
}

fn resolve_snapshot(fold: &UsageFold, registry: &Registry, cost: Option<f64>, run_kind: RunKind) -> StoreResult<Value> {
  let snapshot = fold.snapshot.as_ref().expect("snapshot reading kind carries a snapshot");
  let tokens = truthy_object(snapshot.get("tokens"), "context_snapshot.tokens")?;
  let token_sum = match tokens {
    Some(tokens) => add_int_fields(tokens, &["input", "output", "reasoning", "cache_read", "cache_write"])?,
    None => 0,
  };
  let (context_full, compact_at) = match snapshot.get("limit").and_then(Value::as_object) {
    Some(limit) => {
      let input = limit.get("input").and_then(python_integer);
      let output = limit.get("output").and_then(python_integer);
      // Python's `limit.get("output", 0)`: only a missing key defaults to zero.
      let output_for_input = output.or_else(|| (!limit.contains_key("output")).then_some(0));
      let context = limit.get("context").and_then(python_integer);
      if let (Some(input), Some(output)) = (input, output_for_input) {
        (Some(input), snapshot_compact_at(input, output, registry, run_kind))
      } else if let (Some(context), Some(output)) = (context, output)
        && limit.contains_key("output")
      {
        (Some(context), snapshot_compact_at(context, output, registry, run_kind))
      } else {
        (None, None)
      }
    }
    None => (None, None),
  };
  let model =
    snapshot.get("model").filter(|value| truthy(Some(value))).map_or_else(String::new, value_to_python_string);
  Ok(usage_value(Some(token_sum), context_full, compact_at, &model, cost))
}

fn resolve_reading(fold: &UsageFold, cost: Option<f64>) -> Value {
  let reading = fold.reading.as_ref().expect("resolved reading kind carries a reading");
  let number = |key: &str| {
    reading.get(key).and_then(|value| match value {
      Value::Bool(_) => None,
      value => python_integer(value),
    })
  };
  let model = reading.get("model").filter(|value| truthy(Some(value))).map_or_else(String::new, value_to_python_string);
  usage_value(
    number(event_types::CONTEXT_TOKENS),
    number(event_types::CONTEXT_FULL),
    number(event_types::CONTEXT_COMPACT_AT),
    &model,
    cost,
  )
}

fn subtract_reserve(full: Option<i64>, limits: ContextLimits) -> Option<i64> {
  let reserve = i64::try_from(limits.compact_reserve?).ok()?;
  Some(full? - reserve)
}

/// The compaction line of a snapshot reading: `full` minus the registered reserve, bounded by the output cap.
fn snapshot_compact_at(full: i64, output: i64, registry: &Registry, run_kind: RunKind) -> Option<i64> {
  // The backend option selects only the claude limits, as in Python's `_snapshot_full_and_compact`.
  let reserve = registry.reading_limits_for(READING_SNAPSHOT, run_kind, None).and_then(|limits| limits.compact_reserve);
  reserve.map(|reserve| full - i64::try_from(reserve).expect("compact reserve fits i64").min(output))
}

fn usage_value(
  context_tokens: Option<i64>,
  context_full: Option<i64>,
  context_compact_at: Option<i64>,
  model: &str,
  cost: Option<f64>,
) -> Value {
  json!({
    "context_tokens": context_tokens,
    "context_full": context_full,
    (event_types::CONTEXT_COMPACT_AT): context_compact_at,
    "total_cost_usd": cost,
    "model": model,
  })
}

/// `truthy` over one borrowed event field, `None` included.
fn truthy_field<E: EventFields>(value: Option<&E>) -> bool {
  value.is_some_and(is_truthy)
}

fn truthy(value: Option<&Value>) -> bool {
  match value {
    None | Some(Value::Null) => false,
    Some(Value::Bool(value)) => *value,
    Some(Value::Number(value)) => value.as_f64().is_some_and(|number| number != 0.0),
    Some(Value::String(value)) => !value.is_empty(),
    Some(Value::Array(value)) => !value.is_empty(),
    Some(Value::Object(value)) => !value.is_empty(),
  }
}

fn truthy_object<'a, E: EventFields>(value: Option<&'a E>, field: &str) -> StoreResult<Option<&'a E>> {
  let Some(value) = value else { return Ok(None) };
  if !is_truthy(value) {
    return Ok(None);
  }
  if !value.is_object() {
    return Err(StoreError::new(format!("{field} must be an object")));
  }
  Ok(Some(value))
}

fn python_integer<E: EventFields>(value: &E) -> Option<i64> {
  value
    .as_bool_field()
    .map(i64::from)
    .or_else(|| value.as_i64_field())
    .or_else(|| value.as_u64_field().and_then(|value| i64::try_from(value).ok()))
}

fn add_int_fields<E: EventFields>(object: &E, fields: &[&str]) -> StoreResult<i64> {
  fields.iter().try_fold(0_i64, |sum, field| match object.field(field) {
    None => Ok(sum),
    Some(value) => python_integer(value)
      .map(|value| sum + value)
      .ok_or_else(|| StoreError::new(format!("usage field {field} is not an integer"))),
  })
}

fn python_number(value: &Value, field: &str) -> StoreResult<f64> {
  match value {
    Value::Bool(value) => Ok(f64::from(u8::from(*value))),
    Value::Number(value) => {
      value.as_f64().ok_or_else(|| StoreError::new(format!("usage field {field} is not numeric")))
    }
    _ => Err(StoreError::new(format!("usage field {field} is not numeric"))),
  }
}

fn value_to_python_string<E: EventFields>(value: &E) -> String {
  if let Some(text) = value.as_str_field() {
    return text.to_string();
  }
  if value.is_null() {
    return "None".to_string();
  }
  if let Some(flag) = value.as_bool_field() {
    return if flag { "True" } else { "False" }.to_string();
  }
  value.to_owned_value().to_string()
}

#[cfg(test)]
mod tests {
  /// The fold input over a test's owned events, matching the shared slices the routes fold.
  fn event_refs(events: &[Value]) -> Vec<&Value> {
    events.iter().collect()
  }

  use std::path::Path;
  use std::sync::OnceLock;

  use charliebot_anthropic_compatible as anthropic_compatible;
  use charliebot_claude_code as claude_code;
  use charliebot_codex as codex;
  use charliebot_codex::CodexHost;
  use charliebot_core::hot_reload::HotReload;
  use charliebot_registry::backend::Env;
  use charliebot_registry::{RegisterFn, store_hooks};
  use charliebot_store::Store;
  use charliebot_store::metadata::TaskProfile;

  use super::*;

  const PYTHON_FIXTURE: &str = include_str!("../tests/fixtures/python_usage_reference.json");

  fn registry() -> Registry {
    Registry::default()
  }

  fn synthetic_codex_home() -> &'static Path {
    static HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    HOME.get_or_init(|| tempfile::tempdir().unwrap()).path()
  }

  fn register_codex(registry: &mut Registry) -> Result<(), RegistryError> {
    codex::register_with_host(
      registry,
      &CodexHost { path: "/synthetic/bin".to_string(), home: synthetic_codex_home().to_path_buf() },
    )
  }

  fn register_anthropic_compatible(registry: &mut Registry) -> Result<(), RegistryError> {
    anthropic_compatible::register(registry)
  }

  fn python_usage_reference(profile: &str, events: &[Value], backend_option: Option<Value>) -> Value {
    let mut request = json!({"profile": profile, "events": events});
    if let Some(backend_option) = backend_option {
      request["backend_option"] = backend_option;
    }
    let fixture: Value = serde_json::from_str(PYTHON_FIXTURE).unwrap();
    assert_eq!(fixture["source"], "rust/crates/runtime/usage/src/usage.rs::PYTHON_USAGE_REFERENCE");
    assert_eq!(fixture["base_commit"], "a572e7ca71858d9ba52f0abc2726fa44213210ac");
    for case in fixture["cases"].as_array().unwrap() {
      let mut recorded_request = case["request"].clone();
      replace_fixture_hosts(&mut recorded_request);
      if recorded_request == request {
        return case["expected"].clone();
      }
    }
    panic!("no recorded Python usage result matches request {request}");
  }

  fn replace_fixture_hosts(value: &mut Value) {
    match value {
      Value::String(text) => {
        *text = text.replace("api.$HOST", "api.synthetic.example").replace("$HOST", "synthetic.example");
      }
      Value::Array(values) => values.iter_mut().for_each(replace_fixture_hosts),
      Value::Object(fields) => fields.values_mut().for_each(replace_fixture_hosts),
      _ => {}
    }
  }

  fn owner_over(home: &Path, registry: Arc<Registry>) -> (SessionUsage, Arc<Store>) {
    let config = registry.load_config(home).unwrap();
    let config_file = config.config_file();
    let reload_value = config.clone();
    let hot_config = Arc::new(HotReload::new(
      config_file,
      "session_usage_test_config_reload_failed",
      Box::new(move || Ok(reload_value.clone())),
    ));
    let store =
      Arc::new(Store::new(config, registry.metadata_slots(), store_hooks::store_hooks(Arc::clone(&registry))).unwrap());
    (SessionUsage::new(hot_config, Arc::clone(&store), registry), store)
  }

  fn write_events(store: &Store, session_id: &str, events: &[Value]) {
    let path = store.chat_events_path(session_id);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let body = events.iter().map(serde_json::to_string).collect::<Result<Vec<_>, _>>().unwrap().join("\n");
    std::fs::write(path, format!("{body}\n")).unwrap();
  }

  /// The generic fold reads compact events in place: over one corpus the shared-form fold answers
  /// exactly what the Value-form fold answers, with no per-event conversion in between.
  #[test]
  fn the_compact_form_fold_answers_what_the_value_form_fold_answers() {
    let events = vec![
      json!({"type": "assistant", "message": {"model": "model-a", "usage": {"input_tokens": 10, "cache_creation_input_tokens": 2, "cache_read_input_tokens": 3}}}),
      json!({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": "synthetic body"}], "usage": {"input_tokens": 20, "output_tokens": 5}}, "timestamp": "2026-01-01T00:00:00Z"}),
      json!({"type": "result", "modelUsage": {"model-a": {"contextWindow": 100}}, "total_cost_usd": 0.25}),
      json!({"type": "system", "subtype": "compact_boundary", "compact_metadata": {"post_tokens": 12345}}),
    ];
    let compact = events.iter().map(ChatEvent::from_value).collect::<Vec<_>>();
    let value_refs = event_refs(&events);
    let compact_refs = compact.iter().collect::<Vec<_>>();
    let from_values = fold_usage(&value_refs, RunKind::Work, &registry(), None).unwrap().unwrap();
    let from_compact = fold_usage(&compact_refs, RunKind::Work, &registry(), None).unwrap().unwrap();
    assert_eq!(from_values, from_compact);
    // The compact boundary's post-tokens replace the chosen prompt tokens, in both forms alike.
    assert_eq!(from_compact["context_tokens"], 12345);
  }

  #[test]
  fn claude_tier_uses_assistant_event_tokens_not_result_cumulative() {
    let events = vec![
      json!({"type": "assistant", "message": {"model": "model-a", "usage": {"input_tokens": 10, "cache_creation_input_tokens": 2, "cache_read_input_tokens": 3}}}),
      json!({"type": "result", "modelUsage": {"model-a": {"contextWindow": 100}}, "total_cost_usd": 0.25}),
    ];
    let usage = fold_usage(&event_refs(&events), RunKind::Work, &registry(), None).unwrap().unwrap();
    assert_eq!(usage["context_tokens"], 15);
    assert_eq!(usage["model"], "model-a");
    assert_eq!(usage["total_cost_usd"], 0.25);
  }

  // Python: tests/test_anthropic_compatible_backend.py:111.
  #[test]
  fn anthropic_compatible_usage_uses_the_option_context_window() {
    let mut registry = Registry::new();
    registry.register_package("src.backends.anthropic_compatible", register_anthropic_compatible).unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
      home.path().join("config.yaml"),
      "backends:\n  options:\n    - id: anthro\n      label: Anthropic compatible\n      type: cc-anthropic-compatible\n      model: synthetic-model\n      api_base: https://api.synthetic.example\n      context_window: 409600\n",
    )
    .unwrap();
    let config = registry.load_config(home.path()).unwrap();
    let option = &config.backends.options[0];
    let events = vec![
      json!({"type": "assistant", "message": {"model": "synthetic-model", "usage": {
        "input_tokens": 1200, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0
      }}}),
      json!({"type": "result", "modelUsage": {"synthetic-model": {"contextWindow": 1_000_000}}}),
    ];

    let python = python_usage_reference(
      "manager",
      &events,
      Some(json!({
        "id": "anthro",
        "label": "Anthropic compatible",
        "type": "cc-anthropic-compatible",
        "model": "synthetic-model",
        "api_base": "https://api.synthetic.example",
        "context_window": 409_600,
      })),
    );
    let usage = fold_usage(&event_refs(&events), RunKind::ManagerTurn, &registry, Some(option)).unwrap().unwrap();

    assert_eq!(serde_json::to_value(RunKind::ManagerTurn).unwrap(), python["run_kind"]);
    assert_eq!(usage, python["usage"]);
    assert_eq!(usage["context_full"], 409_600);
    assert_eq!(usage["context_compact_at"], 376_600);
  }

  #[test]
  fn claude_tier_ignores_subagent_and_synthetic_assistant_events() {
    let events = vec![
      json!({"type": "assistant", "parent_tool_use_id": "tool-1", "message": {"model": "subagent", "usage": {"input_tokens": 100}}}),
      json!({"type": "assistant", "message": {"model": "empty", "usage": {"input_tokens": 0}}}),
      json!({"type": "assistant", "message": {"model": "main", "usage": {"input_tokens": 7}}}),
    ];
    let usage = fold_usage(&event_refs(&events), RunKind::Work, &registry(), None).unwrap().unwrap();
    assert_eq!(usage["context_tokens"], 7);
    assert_eq!(usage["model"], "main");
  }

  #[test]
  fn total_cost_sums_results_and_rounds_to_four_places() {
    let events =
      vec![json!({"type": "result", "total_cost_usd": 0.12345}), json!({"type": "result", "total_cost_usd": 0.00005})];
    assert_eq!(
      fold_usage(&event_refs(&events), RunKind::Work, &registry(), None).unwrap().unwrap()["total_cost_usd"],
      0.1235
    );
  }

  #[test]
  fn usage_run_kind_and_claude_resolution_match_python_for_session_profiles() {
    let events = vec![
      json!({
        "type": "assistant",
        "message": {
          "model": "claude-synthetic-5",
          "usage": {"input_tokens": 120_000, "cache_creation_input_tokens": 10_000, "cache_read_input_tokens": 20_000}
        }
      }),
      json!({
        "type": "result",
        "modelUsage": {"claude-synthetic-5": {"contextWindow": 500_000}},
        "total_cost_usd": 0.125
      }),
    ];
    let mut registry = Registry::new();
    registry
      .register_package("src.backends.claude_code", |registry| {
        registry.reading_limits("claude", |run_kind| claude_code::claude_reading_limits_from(run_kind, &Env::new()))
      })
      .unwrap();

    for (profile_name, profile) in [("manager", TaskProfile::Manager), ("worker", TaskProfile::Worker)] {
      let meta = SessionMetadata::new("synthetic-usage", "Synthetic usage", profile);
      let run_kind = usage_run_kind(&meta);
      let python = python_usage_reference(profile_name, &events, None);
      assert_eq!(serde_json::to_value(run_kind).unwrap(), python["run_kind"]);
      assert_eq!(fold_usage(&event_refs(&events), run_kind, &registry, None).unwrap().unwrap(), python["usage"]);
    }
  }

  #[test]
  fn total_cost_is_unknown_when_all_result_reports_are_zero() {
    let events =
      vec![json!({"type": "result", "total_cost_usd": 0.0}), json!({"type": "result", "total_cost_usd": 0.0})];
    assert!(
      fold_usage(&event_refs(&events), RunKind::Work, &registry(), None).unwrap().unwrap()["total_cost_usd"].is_null()
    );
  }

  #[test]
  fn total_cost_sums_positive_result_reports() {
    let events =
      vec![json!({"type": "result", "total_cost_usd": 0.1}), json!({"type": "result", "total_cost_usd": 0.2})];
    assert_eq!(
      fold_usage(&event_refs(&events), RunKind::Work, &registry(), None).unwrap().unwrap()["total_cost_usd"],
      0.3
    );
  }

  #[test]
  fn total_cost_rounds_the_exact_binary_value_like_python() {
    // Python: round(0.12345, 4) == 0.1235 and round(2.67895, 4) == 2.6789; scaling by 10_000 gets the first wrong.
    for (cost, expected) in [(0.12345, 0.1235), (2.67895, 2.6789)] {
      let events = vec![json!({"type": "result", "total_cost_usd": cost})];
      assert_eq!(
        fold_usage(&event_refs(&events), RunKind::Work, &registry(), None).unwrap().unwrap()["total_cost_usd"],
        expected
      );
    }
  }

  #[test]
  fn claude_reading_takes_the_smaller_of_model_and_declared_windows() {
    let mut registry = Registry::new();
    registry
      .register_package("src.backends.claude_code", |registry| {
        registry.reading_limits("claude", |_run_kind| ContextLimits {
          declared_window: Some(500_000),
          compact_reserve: Some(20_000),
        })
      })
      .unwrap();
    let events = vec![
      json!({"type": "assistant", "message": {"model": "model-a", "usage": {"input_tokens": 150_000}}}),
      json!({"type": "result", "modelUsage": {"model-a": {"contextWindow": 200_000}}, "total_cost_usd": 0.5}),
    ];
    let usage = fold_usage(&event_refs(&events), RunKind::Work, &registry, None).unwrap().unwrap();
    assert_eq!(usage["context_tokens"], 150_000);
    assert_eq!(usage["context_full"], 200_000);
    assert_eq!(usage["context_compact_at"], 180_000);
    assert_eq!(usage["total_cost_usd"], 0.5);

    // A model absent from modelUsage falls back to the declared window alone.
    let unlisted = vec![json!({"type": "assistant", "message": {"model": "model-b", "usage": {"input_tokens": 1}}})];
    let usage = fold_usage(&event_refs(&unlisted), RunKind::Work, &registry, None).unwrap().unwrap();
    assert_eq!(usage["context_full"], 500_000);
    assert_eq!(usage["context_compact_at"], 480_000);
  }

  #[test]
  fn context_reading_tier_beats_cumulative_result_usage() {
    let events = vec![
      json!({"type": "assistant", "message": {"model": "model-a", "usage": {"input_tokens": 100}}}),
      json!({"type": "system", "subtype": "context_reading", "context_reading": {"context_tokens": 31, "context_full": 80, "context_compact_at": 70, "model": "resolved"}}),
      json!({"type": "result", "modelUsage": {"model-a": {"contextWindow": 1000}}}),
    ];
    let usage = fold_usage(&event_refs(&events), RunKind::Work, &registry(), None).unwrap().unwrap();
    assert_eq!(usage["context_tokens"], 31);
    assert_eq!(usage["model"], "resolved");
  }

  #[test]
  fn snapshot_tier_beats_older_assistant_reading() {
    let events = vec![
      json!({"type": "assistant", "message": {"model": "model-a", "usage": {"input_tokens": 100}}}),
      json!({
        "type": "result",
        "context_snapshot": {
          "model": "snapshot-model",
          "tokens": {"input": 21, "output": 3, "reasoning": 2, "cache_read": 4, "cache_write": 5},
          "limit": {"input": 100, "output": 20}
        }
      }),
    ];
    let usage = fold_usage(&event_refs(&events), RunKind::Work, &registry(), None).unwrap().unwrap();
    assert_eq!(usage["context_tokens"], 35);
    assert_eq!(usage["context_full"], 100);
    assert_eq!(usage["model"], "snapshot-model");
  }

  fn opencode_snapshot_limits(_run_kind: RunKind) -> ContextLimits {
    ContextLimits { declared_window: None, compact_reserve: Some(20_000) }
  }

  fn custom_snapshot_limits(_run_kind: RunKind) -> ContextLimits {
    ContextLimits { declared_window: None, compact_reserve: Some(17_000) }
  }

  fn unknown_reserve_snapshot_limits(_run_kind: RunKind) -> ContextLimits {
    ContextLimits { declared_window: None, compact_reserve: None }
  }

  fn register_opencode_snapshot_limits(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.reading_limits("snapshot", opencode_snapshot_limits)
  }

  fn register_custom_snapshot_limits(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.reading_limits("snapshot", custom_snapshot_limits)
  }

  fn register_unknown_reserve_snapshot_limits(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.reading_limits("snapshot", unknown_reserve_snapshot_limits)
  }

  fn registry_with_snapshot_limits(register: RegisterFn) -> Registry {
    let mut registry = Registry::new();
    registry.register_package("src.backends.opencode", register).unwrap();
    registry
  }

  fn snapshot_event(limit: Value) -> Value {
    json!({
      "type": "result",
      "context_snapshot": {
        "model": "snapshot-model",
        "tokens": {"input": 95_000, "output": 2_000, "reasoning": 1_000, "cache_read": 4_000, "cache_write": 131},
        "limit": limit,
      }
    })
  }

  // Python: tests/test_session_usage.py::test_snapshot_tier_window_and_compaction_line.
  #[test]
  fn snapshot_tier_uses_the_context_window_and_the_reserve_bounded_by_output() {
    let registry = registry_with_snapshot_limits(register_opencode_snapshot_limits);
    let cases = [
      ("large-output-cap", json!({"context": 420_000, "output": 384_000}), Some(420_000), Some(400_000)),
      ("output-above-reserve", json!({"context": 200_000, "output": 32_000}), Some(200_000), Some(180_000)),
      ("output-below-reserve", json!({"context": 200_000, "output": 8_000}), Some(200_000), Some(192_000)),
      ("missing-output", json!({"context": 200_000}), None, None),
      ("null-output", json!({"context": 200_000, "output": null}), None, None),
      ("string-output", json!({"context": 200_000, "output": "32000"}), None, None),
      ("float-output", json!({"context": 200_000, "output": 32_000.0}), None, None),
      (
        "input-takes-precedence",
        json!({"context": 420_000, "input": 300_000, "output": 384_000}),
        Some(300_000),
        Some(280_000),
      ),
      ("input-without-output", json!({"context": 420_000, "input": 300_000}), Some(300_000), Some(300_000)),
    ];
    for (case, limit, expected_full, expected_compact_at) in cases {
      let events = [snapshot_event(limit)];
      let usage = fold_usage(&event_refs(&events), RunKind::ManagerTurn, &registry, None).unwrap().unwrap();
      assert_eq!(usage["context_tokens"], 102_131, "case {case}");
      assert_eq!(usage["context_full"], json!(expected_full), "case {case}");
      assert_eq!(usage["context_compact_at"], json!(expected_compact_at), "case {case}");
    }
  }

  // Python: tests/test_session_usage.py::test_snapshot_tier_reads_compaction_reserve.
  #[test]
  fn snapshot_tier_reads_the_registered_compaction_reserve() {
    let limit = json!({"context": 420_000, "output": 384_000});
    let cases: [(&str, Option<RegisterFn>, Option<i64>); 3] = [
      ("missing-registration", None, None),
      ("unknown-reserve", Some(register_unknown_reserve_snapshot_limits), None),
      ("registered-reserve", Some(register_custom_snapshot_limits), Some(403_000)),
    ];
    for (case, register, expected_compact_at) in cases {
      let registry = match register {
        Some(register) => registry_with_snapshot_limits(register),
        None => registry(),
      };
      let events = [snapshot_event(limit.clone())];
      let usage = fold_usage(&event_refs(&events), RunKind::Work, &registry, None).unwrap().unwrap();
      assert_eq!(usage["context_full"], 420_000, "case {case}");
      assert_eq!(usage["context_compact_at"], json!(expected_compact_at), "case {case}");
    }
  }

  #[test]
  fn empty_slot_keeps_context_unknown() {
    let events = vec![json!({"type": "user", "content": "hello"})];
    let usage = fold_usage(&event_refs(&events), RunKind::Work, &registry(), None).unwrap().unwrap();
    assert!(usage["context_tokens"].is_null());
    assert!(usage["context_full"].is_null());
    assert!(usage["context_compact_at"].is_null());
    assert_eq!(usage["model"], "");
  }

  #[test]
  fn empty_event_list_has_no_usage_value() {
    assert!(fold_usage::<Value>(&[], RunKind::Work, &registry(), None).unwrap().is_none());
  }

  #[test]
  fn registered_codex_resolver_precedes_the_fold_and_is_cached_once() {
    let mut registry = Registry::new();
    registry.register_package("src.backends.codex", register_codex).unwrap();
    let registry = Arc::new(registry);
    let home = tempfile::tempdir().unwrap();
    let (usage_owner, _store) = owner_over(home.path(), Arc::clone(&registry));

    let thread_id = "12345678-1234-4234-9234-123456789abc";
    let rollout = synthetic_codex_home()
      .join(".codex/sessions/2026/10/18")
      .join(format!("rollout-2026-10-18T10-00-00-{thread_id}.jsonl"));
    std::fs::create_dir_all(rollout.parent().unwrap()).unwrap();
    let records = [
      json!({"type": "turn_context", "payload": {"model": "gpt-5.5"}}),
      json!({
        "type": "event_msg",
        "payload": {
          "type": "token_count",
          "info": {
            "last_token_usage": {"input_tokens": 321},
            "model_context_window": 12345
          }
        }
      }),
    ];
    let body = records.iter().map(serde_json::to_string).collect::<Result<Vec<_>, _>>().unwrap().join("\n");
    std::fs::write(rollout, format!("{body}\n")).unwrap();

    let mut meta = SessionMetadata::new("synthetic-codex-session", "Synthetic Codex", TaskProfile::Manager);
    meta.backend = "codex-retired-option".to_string();
    meta.cc_session_id = Some(thread_id.to_string());

    let first = usage_owner.resolve_session_usage(&meta.id, &meta, None).unwrap().unwrap();
    assert_eq!(first["context_tokens"], 321);
    assert_eq!(first["context_full"], 12345);
    assert_eq!(first["model"], "gpt-5.5");
    let first_resolver = usage_owner.usage_resolvers.lock().unwrap()["codex"].as_ref().unwrap().clone();

    let second = usage_owner.resolve_session_usage(&meta.id, &meta, None).unwrap().unwrap();
    assert_eq!(second, first);
    let second_resolver = usage_owner.usage_resolvers.lock().unwrap()["codex"].as_ref().unwrap().clone();
    assert!(Arc::ptr_eq(&first_resolver, &second_resolver));

    let (context_tokens, last_request_at) = usage_owner.context_state(&meta.id, &meta).unwrap();
    assert_eq!(context_tokens, Some(321));
    assert_eq!(last_request_at, None);
  }

  #[tokio::test]
  async fn facts_memo_folds_only_appended_events_and_rebuilds_after_cache_replacement() {
    let home = tempfile::tempdir().unwrap();
    let registry = Arc::new(registry());
    let (usage_owner, store) = owner_over(home.path(), registry);
    let session_id = "synthetic-usage-memo";
    let mut meta = SessionMetadata::new(session_id, "Usage memo", TaskProfile::Manager);
    meta.backend = "unregistered-backend".to_string();
    write_events(
      &store,
      session_id,
      &[
        json!({"type": "assistant", "timestamp": "2026-10-18T10:00:00Z", "message": {"model": "model-a", "usage": {"input_tokens": 20}}}),
        json!({"type": "result", "total_cost_usd": 0.1}),
      ],
    );

    let first = usage_owner.resolve_session_usage(session_id, &meta, None).unwrap().unwrap();
    assert_eq!(first["context_tokens"], 20);
    assert_eq!(first["total_cost_usd"], 0.1);

    store.append_chat_event(session_id, json!({"type": "result", "total_cost_usd": 0.2})).await.unwrap();
    let appended = usage_owner.resolve_session_usage(session_id, &meta, None).unwrap().unwrap();
    assert_eq!(appended["context_tokens"], 20);
    assert_eq!(appended["total_cost_usd"], 0.3);
    assert_eq!(usage_owner.context_state(session_id, &meta).unwrap().0, Some(20));

    write_events(
      &store,
      session_id,
      &[
        json!({"type": "assistant", "message": {"model": "model-b", "usage": {"input_tokens": 5}}}),
        json!({"type": "result", "total_cost_usd": 0.4}),
      ],
    );
    store.clear_chat_events_cache(session_id);
    let replaced = usage_owner.resolve_session_usage(session_id, &meta, None).unwrap().unwrap();
    assert_eq!(replaced["context_tokens"], 5);
    assert_eq!(replaced["total_cost_usd"], 0.4);
  }

  // Python: tests/test_session_usage.py:334.
  #[test]
  fn context_compacted_post_tokens_becomes_the_claude_reading() {
    let now = UtcDateTime::parse("2026-09-06T20:00:00Z").unwrap();
    let last_request = UtcDateTime::parse("2026-09-06T18:59:00Z").unwrap();
    let events = [
      json!({"type": "assistant", "message": {"model": "claude-fable-5-1", "usage": {"input_tokens": 281000}}}),
      json!({
        "type": "context_compacted",
        "trigger": "idle",
        "compact_metadata": {"post_tokens": 9111},
        "model": "claude-fable-5-1"
      }),
    ];
    let usage = fold_usage(&event_refs(&events), RunKind::ManagerTurn, &registry(), None).unwrap().unwrap();

    assert_eq!(usage["context_tokens"], 9111);

    let home = tempfile::tempdir().unwrap();
    let mut registry = Registry::new();
    registry.register_package("src.backends.claude_code", claude_code::register).unwrap();
    let cfg = registry.load_config(home.path()).unwrap();
    assert!(!claude_code::claude_compaction::expired_cache_compaction_wanted(
      &cfg,
      Some("claude-fable-5-1"),
      usage["context_tokens"].as_i64(),
      Some(last_request),
      Some(now),
    ));
  }
}
