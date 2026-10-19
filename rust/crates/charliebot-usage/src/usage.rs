//! The session usage owner, ported from `session_usage.py`.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use charliebot_core::config::CharlieBotConfig;
use charliebot_core::event_types;
use charliebot_core::hot_reload::HotReload;
use charliebot_registry::backend::{ContextLimits, UsageResolver, UsageResolverHost};
use charliebot_registry::{FeatureContext, Registry, RegistryError};
use charliebot_store::metadata::{SessionMetadata, UtcDateTime};
use charliebot_store::{Store, StoreError, StoreResult};
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
  snapshot: Option<Map<String, Value>>,
  reading_kind: String,
  reading: Option<Map<String, Value>>,
  total_cost: f64,
  unknown_cost: bool,
  last_request_at: Option<UtcDateTime>,
}

impl UsageFold {
  fn feed(&mut self, events: &[Value]) -> StoreResult<()> {
    for event in events {
      let event_type = event.get("type").and_then(Value::as_str);
      if matches!(event_type, Some(event_types::ASSISTANT | event_types::RESULT))
        && let Some(timestamp) = event.get("timestamp").and_then(Value::as_str)
        && let Ok(parsed) = UtcDateTime::parse(timestamp)
      {
        self.last_request_at = Some(parsed);
      }
      match event_type {
        Some(event_types::ASSISTANT) => self.feed_assistant(event)?,
        Some(event_types::SYSTEM) => self.feed_system(event)?,
        Some(event_types::RESULT) => self.feed_result(event)?,
        _ => {}
      }
    }
    Ok(())
  }

  fn feed_assistant(&mut self, event: &Value) -> StoreResult<()> {
    if truthy(event.get("parent_tool_use_id")) {
      return Ok(());
    }
    let message = truthy_object(event.get("message"), "assistant.message")?;
    let usage = truthy_object(message.get("usage"), "assistant.message.usage")?;
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
        message.get("model").filter(|value| truthy(Some(value))).map_or_else(String::new, value_to_python_string);
      self.post_compact_tokens = None;
      self.reading_kind = READING_CLAUDE.to_string();
    }
    Ok(())
  }

  fn feed_system(&mut self, event: &Value) -> StoreResult<()> {
    match event.get("subtype").and_then(Value::as_str) {
      Some(event_types::COMPACT_BOUNDARY) if self.reading_kind == READING_CLAUDE => {
        let compact = truthy_object(event.get(event_types::COMPACT_METADATA), "system.compact_metadata")?;
        if let Some(tokens) = compact.get(event_types::COMPACT_POST_TOKENS).and_then(python_integer) {
          self.post_compact_tokens = Some(tokens);
        }
      }
      Some(event_types::CONTEXT_READING) => {
        if let Some(reading) = event.get(event_types::CONTEXT_READING).and_then(Value::as_object) {
          self.reading_kind = READING_RESOLVED.to_string();
          self.reading = Some(reading.clone());
        }
      }
      _ => {}
    }
    Ok(())
  }

  fn feed_result(&mut self, event: &Value) -> StoreResult<()> {
    let model_usage = truthy_object(event.get("modelUsage"), "result.modelUsage")?;
    for (model_name, info_value) in model_usage {
      let info = truthy_object(Some(info_value), "result.modelUsage entry")?;
      if let Some(window) = info.get("contextWindow").and_then(python_integer) {
        self.model_windows.insert(model_name.clone(), Value::from(window));
      }
    }
    if let Some(snapshot) = event.get("context_snapshot").and_then(Value::as_object) {
      self.snapshot = Some(snapshot.clone());
      self.reading_kind = READING_SNAPSHOT.to_string();
    }
    match event.get(event_types::RESULT_TOTAL_COST_USD) {
      Some(Value::Null) => self.unknown_cost = true,
      Some(value) => self.total_cost += python_number(value, "result.total_cost_usd")?,
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
  pub fn resolve_session_usage(&self, session_id: &str, meta: &SessionMetadata) -> StoreResult<Option<Value>> {
    self.resolve_with_facts(session_id, meta).map(|(usage, _)| usage)
  }

  /// Resolve the event-only tiers for an already-loaded transcript, without a backend resolver or cross-call memo.
  pub fn resolve_events_usage(&self, events: &[Value]) -> StoreResult<Option<Value>> {
    fold_usage(events, &self.registry)
  }

  /// Return the selected context reading and the latest assistant/result request time.
  pub fn context_state(
    &self,
    session_id: &str,
    meta: &SessionMetadata,
  ) -> StoreResult<(Option<i64>, Option<UtcDateTime>)> {
    let (usage, fold) = self.resolve_with_facts(session_id, meta)?;
    let context_tokens = usage.as_ref().and_then(|usage| usage.get("context_tokens")).and_then(python_integer);
    Ok((context_tokens, fold.last_request_at))
  }

  fn resolve_with_facts(&self, session_id: &str, meta: &SessionMetadata) -> StoreResult<(Option<Value>, UsageFold)> {
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
    Ok((resolve_fold_tier(&fold, &self.registry, event_count)?, fold))
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
    fold.feed(&delta.events)?;
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
pub fn fold_usage(events: &[Value], registry: &Registry) -> StoreResult<Option<Value>> {
  let mut fold = UsageFold::default();
  fold.feed(events)?;
  resolve_fold_tier(&fold, registry, events.len())
}

fn resolve_fold_tier(fold: &UsageFold, registry: &Registry, event_count: usize) -> StoreResult<Option<Value>> {
  if event_count == 0 {
    return Ok(None);
  }
  let cost = fold.cost();
  let usage = match fold.reading_kind.as_str() {
    READING_CLAUDE => Some(resolve_claude(fold, registry, cost)?),
    READING_SNAPSHOT => Some(resolve_snapshot(fold, registry, cost)?),
    READING_RESOLVED => Some(resolve_reading(fold, cost)),
    _ => Some(json!({
      "context_tokens": null,
      "context_full": null,
      "context_compact_at": null,
      "total_cost_usd": cost,
      "model": "",
    })),
  };
  Ok(usage)
}

fn resolve_claude(fold: &UsageFold, registry: &Registry, cost: Option<f64>) -> StoreResult<Value> {
  let context_tokens = fold.post_compact_tokens.unwrap_or(fold.chosen_prompt_tokens);
  let limits = registry.reading_limits_for(READING_CLAUDE);
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

fn resolve_snapshot(fold: &UsageFold, registry: &Registry, cost: Option<f64>) -> StoreResult<Value> {
  let snapshot = fold.snapshot.as_ref().expect("snapshot reading kind carries a snapshot");
  let tokens = truthy_object(snapshot.get("tokens"), "context_snapshot.tokens")?;
  let token_sum = add_int_fields(tokens, &["input", "output", "reasoning", "cache_read", "cache_write"])?;
  let (context_full, compact_at) = match snapshot.get("limit").and_then(Value::as_object) {
    Some(limit) => {
      let input = limit.get("input").and_then(python_integer);
      let output =
        limit.get("output").and_then(python_integer).or_else(|| (!limit.contains_key("output")).then_some(0));
      let context = limit.get("context").and_then(python_integer);
      match (input, output, context) {
        (Some(input), Some(output), _) => {
          let reserve = registry.reading_limits_for(READING_SNAPSHOT).and_then(|limits| limits.compact_reserve);
          let compact_at =
            reserve.map(|reserve| input - i64::try_from(reserve).expect("compact reserve fits i64").min(output));
          (Some(input), compact_at)
        }
        (_, Some(output), Some(context)) => (Some(context - output), None),
        _ => (None, None),
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
    "context_compact_at": context_compact_at,
    "total_cost_usd": cost,
    "model": model,
  })
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

fn truthy_object<'a>(value: Option<&'a Value>, field: &str) -> StoreResult<&'a Map<String, Value>> {
  if !truthy(value) {
    return Ok(empty_object());
  }
  value.and_then(Value::as_object).ok_or_else(|| StoreError::new(format!("{field} must be an object")))
}

fn empty_object() -> &'static Map<String, Value> {
  static EMPTY: std::sync::LazyLock<Map<String, Value>> = std::sync::LazyLock::new(Map::new);
  &EMPTY
}

fn python_integer(value: &Value) -> Option<i64> {
  match value {
    Value::Bool(value) => Some(i64::from(*value)),
    Value::Number(number) => number.as_i64().or_else(|| number.as_u64().and_then(|value| i64::try_from(value).ok())),
    _ => None,
  }
}

fn add_int_fields(object: &Map<String, Value>, fields: &[&str]) -> StoreResult<i64> {
  fields.iter().try_fold(0_i64, |sum, field| match object.get(*field) {
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

fn value_to_python_string(value: &Value) -> String {
  match value {
    Value::String(value) => value.clone(),
    Value::Null => "None".to_string(),
    Value::Bool(value) => if *value { "True" } else { "False" }.to_string(),
    _ => value.to_string(),
  }
}

#[cfg(test)]
mod tests {
  use std::path::Path;
  use std::sync::OnceLock;

  use charliebot_agents::backends::codex::{self, CodexHost};
  use charliebot_core::hot_reload::HotReload;
  use charliebot_registry::store_hooks;
  use charliebot_store::Store;
  use charliebot_store::metadata::TaskProfile;

  use super::*;

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

  #[test]
  fn claude_tier_uses_assistant_event_tokens_not_result_cumulative() {
    let events = vec![
      json!({"type": "assistant", "message": {"model": "model-a", "usage": {"input_tokens": 10, "cache_creation_input_tokens": 2, "cache_read_input_tokens": 3}}}),
      json!({"type": "result", "modelUsage": {"model-a": {"contextWindow": 100}}, "total_cost_usd": 0.25}),
    ];
    let usage = fold_usage(&events, &registry()).unwrap().unwrap();
    assert_eq!(usage["context_tokens"], 15);
    assert_eq!(usage["model"], "model-a");
    assert_eq!(usage["total_cost_usd"], 0.25);
  }

  #[test]
  fn claude_tier_ignores_subagent_and_synthetic_assistant_events() {
    let events = vec![
      json!({"type": "assistant", "parent_tool_use_id": "tool-1", "message": {"model": "subagent", "usage": {"input_tokens": 100}}}),
      json!({"type": "assistant", "message": {"model": "empty", "usage": {"input_tokens": 0}}}),
      json!({"type": "assistant", "message": {"model": "main", "usage": {"input_tokens": 7}}}),
    ];
    let usage = fold_usage(&events, &registry()).unwrap().unwrap();
    assert_eq!(usage["context_tokens"], 7);
    assert_eq!(usage["model"], "main");
  }

  #[test]
  fn total_cost_sums_results_and_rounds_to_four_places() {
    let events =
      vec![json!({"type": "result", "total_cost_usd": 0.12345}), json!({"type": "result", "total_cost_usd": 0.00005})];
    assert_eq!(fold_usage(&events, &registry()).unwrap().unwrap()["total_cost_usd"], 0.1235);
  }

  #[test]
  fn total_cost_is_unknown_when_all_result_reports_are_zero() {
    let events =
      vec![json!({"type": "result", "total_cost_usd": 0.0}), json!({"type": "result", "total_cost_usd": 0.0})];
    assert!(fold_usage(&events, &registry()).unwrap().unwrap()["total_cost_usd"].is_null());
  }

  #[test]
  fn total_cost_sums_positive_result_reports() {
    let events =
      vec![json!({"type": "result", "total_cost_usd": 0.1}), json!({"type": "result", "total_cost_usd": 0.2})];
    assert_eq!(fold_usage(&events, &registry()).unwrap().unwrap()["total_cost_usd"], 0.3);
  }

  #[test]
  fn total_cost_rounds_the_exact_binary_value_like_python() {
    // Python: round(0.12345, 4) == 0.1235 and round(2.67895, 4) == 2.6789; scaling by 10_000 gets the first wrong.
    for (cost, expected) in [(0.12345, 0.1235), (2.67895, 2.6789)] {
      let events = vec![json!({"type": "result", "total_cost_usd": cost})];
      assert_eq!(fold_usage(&events, &registry()).unwrap().unwrap()["total_cost_usd"], expected);
    }
  }

  #[test]
  fn claude_reading_takes_the_smaller_of_model_and_declared_windows() {
    let mut registry = Registry::new();
    registry
      .register_package("src.backends.claude_code", |registry| {
        registry
          .reading_limits("claude", || ContextLimits { declared_window: Some(500_000), compact_reserve: Some(20_000) })
      })
      .unwrap();
    let events = vec![
      json!({"type": "assistant", "message": {"model": "model-a", "usage": {"input_tokens": 150_000}}}),
      json!({"type": "result", "modelUsage": {"model-a": {"contextWindow": 200_000}}, "total_cost_usd": 0.5}),
    ];
    let usage = fold_usage(&events, &registry).unwrap().unwrap();
    assert_eq!(usage["context_tokens"], 150_000);
    assert_eq!(usage["context_full"], 200_000);
    assert_eq!(usage["context_compact_at"], 180_000);
    assert_eq!(usage["total_cost_usd"], 0.5);

    // A model absent from modelUsage falls back to the declared window alone.
    let unlisted = vec![json!({"type": "assistant", "message": {"model": "model-b", "usage": {"input_tokens": 1}}})];
    let usage = fold_usage(&unlisted, &registry).unwrap().unwrap();
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
    let usage = fold_usage(&events, &registry()).unwrap().unwrap();
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
    let usage = fold_usage(&events, &registry()).unwrap().unwrap();
    assert_eq!(usage["context_tokens"], 35);
    assert_eq!(usage["context_full"], 100);
    assert_eq!(usage["model"], "snapshot-model");
  }

  #[test]
  fn empty_slot_keeps_context_unknown() {
    let events = vec![json!({"type": "user", "content": "hello"})];
    let usage = fold_usage(&events, &registry()).unwrap().unwrap();
    assert!(usage["context_tokens"].is_null());
    assert!(usage["context_full"].is_null());
    assert!(usage["context_compact_at"].is_null());
    assert_eq!(usage["model"], "");
  }

  #[test]
  fn empty_event_list_has_no_usage_value() {
    assert!(fold_usage(&[], &registry()).unwrap().is_none());
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

    let first = usage_owner.resolve_session_usage(&meta.id, &meta).unwrap().unwrap();
    assert_eq!(first["context_tokens"], 321);
    assert_eq!(first["context_full"], 12345);
    assert_eq!(first["model"], "gpt-5.5");
    let first_resolver = usage_owner.usage_resolvers.lock().unwrap()["codex"].as_ref().unwrap().clone();

    let second = usage_owner.resolve_session_usage(&meta.id, &meta).unwrap().unwrap();
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

    let first = usage_owner.resolve_session_usage(session_id, &meta).unwrap().unwrap();
    assert_eq!(first["context_tokens"], 20);
    assert_eq!(first["total_cost_usd"], 0.1);

    store.append_chat_event(session_id, json!({"type": "result", "total_cost_usd": 0.2})).await.unwrap();
    let appended = usage_owner.resolve_session_usage(session_id, &meta).unwrap().unwrap();
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
    let replaced = usage_owner.resolve_session_usage(session_id, &meta).unwrap().unwrap();
    assert_eq!(replaced["context_tokens"], 5);
    assert_eq!(replaced["total_cost_usd"], 0.4);
  }
}
