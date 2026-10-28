//! The codex usage resolver (`CodexUsageResolver`, `src/backends/codex/codex_usage.py`): one session's context reading
//! from its rollout.
//!
//! The registry's resolver trait takes the session's metadata and events only. The rollout home comes from the
//! codex package's registered host. The config and store values the Python resolver reads come from [`UsageHost`],
//! retained by the resolver constructed for the usage owner.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::event_types;
use charliebot_core::event_types::EventFields;
use charliebot_core::hot_reload::HotReload;
use charliebot_registry::backend::{UsageResolver, UsageResolverHost};
use charliebot_store::SharedChatEvent;
use charliebot_store::Store;
use charliebot_store::metadata::SessionMetadata;
use serde_json::{Map, Value, json};

use super::options;
use super::pricing;
use super::rollout::{self, RolloutUsage, mtime_ns, walk_files};

/// The two server values the resolver reads beyond the registry's arguments.
pub trait CodexSessionHost: Send + Sync {
  /// The `model_auto_compact_token_limit` of the backend option `backend_id`, or `None` when it is unset or absent.
  fn auto_compact_limit(&self, backend_id: &str) -> anyhow::Result<Option<i64>>;
  /// The chat-events file of `session_id`.
  fn chat_events_path(&self, session_id: &str) -> PathBuf;
}

/// The config and store retained by one usage resolver.
pub struct UsageHost {
  config: Arc<HotReload<CharlieBotConfig>>,
  store: Arc<Store>,
}

impl UsageHost {
  fn new(host: &UsageResolverHost) -> Self {
    Self { config: Arc::clone(&host.config), store: Arc::clone(&host.store) }
  }
}

impl CodexSessionHost for UsageHost {
  fn auto_compact_limit(&self, backend_id: &str) -> anyhow::Result<Option<i64>> {
    let config = self.config.get().context("loading config.yaml for the codex compaction limit")?;
    match config.get_backend_option(backend_id) {
      None => Ok(None),
      Some(option) => {
        let fields = options::parse(&option.type_fields).map_err(anyhow::Error::msg)?;
        Ok(fields.model_auto_compact_token_limit)
      }
    }
  }

  fn chat_events_path(&self, session_id: &str) -> PathBuf {
    self.store.chat_events_path(session_id)
  }
}

/// Construct the resolver for one usage owner from the registered codex home and server values.
pub fn usage_resolver(codex_home: PathBuf, host: &UsageResolverHost) -> Arc<dyn UsageResolver> {
  Arc::new(CodexUsageResolver::new(codex_home, Arc::new(UsageHost::new(host))))
}

/// Reads one session's context and cost from its codex rollout.
pub struct CodexUsageResolver {
  codex_home: PathBuf,
  host: Arc<dyn CodexSessionHost>,
  rollout_paths: Mutex<HashMap<String, PathBuf>>,
  usages: Mutex<HashMap<String, CachedUsage>>,
}

/// A rollout's usage as read at one (mtime, size), so a changed file is read again.
#[derive(Clone)]
struct CachedUsage {
  mtime_ns: i128,
  size: u64,
  usage: Option<RolloutUsage>,
}

impl CodexUsageResolver {
  pub fn new(codex_home: PathBuf, host: Arc<dyn CodexSessionHost>) -> Self {
    Self { codex_home, host, rollout_paths: Mutex::new(HashMap::new()), usages: Mutex::new(HashMap::new()) }
  }

  /// The context reading of one session. The merged usage carries the rollout's context tokens and window, the
  /// session backend's compaction limit, the cost of the cumulative totals, and the model.
  fn resolve_with_host(
    &self,
    session_id: &str,
    meta: &SessionMetadata,
    events: &[SharedChatEvent],
  ) -> anyhow::Result<Option<Map<String, Value>>> {
    let host = self.host.as_ref();
    let Some(thread_id) = self.thread_id(host, session_id, meta.cc_session_id.as_deref(), events)? else {
      return Ok(None);
    };
    let Some(native) = self.load_usage(&thread_id)? else {
      return Ok(None);
    };
    let auto_compact_limit = match meta.backend.as_str() {
      "" => None,
      backend_id => host.auto_compact_limit(backend_id)?,
    };
    let cost = match &native.total_token_usage {
      Some(totals) => pricing::calculate_codex_usage_cost_usd(&native.model, totals)?,
      None => None,
    };
    Ok(Some(Map::from_iter([
      ("context_tokens".to_string(), native.context_tokens),
      ("context_full".to_string(), native.context_full),
      (event_types::CONTEXT_COMPACT_AT.to_string(), json!(auto_compact_limit)),
      (event_types::RESULT_TOTAL_COST_USD.to_string(), json!(cost)),
      ("model".to_string(), json!(native.model)),
    ])))
  }

  /// The codex thread id of a session: the persisted one, else the first event's, else the chat-events file's.
  fn thread_id(
    &self,
    host: &dyn CodexSessionHost,
    session_id: &str,
    persisted: Option<&str>,
    events: &[SharedChatEvent],
  ) -> anyhow::Result<Option<String>> {
    if let Some(id) = persisted.filter(|id| !id.is_empty()) {
      return Ok(Some(id.to_string()));
    }
    if let Some(id) = events.iter().find_map(|event| translated_session_id(event.as_ref())) {
      return Ok(Some(id));
    }
    Ok(charliebot_store::chat_events::find_in_log(&host.chat_events_path(session_id), translated_session_id)?)
  }

  fn load_usage(&self, thread_id: &str) -> anyhow::Result<Option<RolloutUsage>> {
    let Some(path) = self.rollout_path(thread_id)? else {
      return Ok(None);
    };
    let metadata = fs::metadata(&path).with_context(|| format!("statting rollout {}", path.display()))?;
    let (mtime_ns, size) = (mtime_ns(&metadata), metadata.len());
    if let Some(cached) = self.usages.lock().expect("the codex usage cache is not poisoned").get(thread_id)
      && cached.mtime_ns == mtime_ns
      && cached.size == size
    {
      return Ok(cached.usage.clone());
    }
    let usage = rollout::extract_latest_rollout_usage(&path)?;
    self
      .usages
      .lock()
      .expect("the codex usage cache is not poisoned")
      .insert(thread_id.to_string(), CachedUsage { mtime_ns, size, usage: usage.clone() });
    Ok(usage)
  }

  /// The newest rollout whose name ends with the thread id, under the codex home's `sessions` tree.
  fn rollout_path(&self, thread_id: &str) -> anyhow::Result<Option<PathBuf>> {
    if let Some(cached) =
      self.rollout_paths.lock().expect("the codex rollout path cache is not poisoned").get(thread_id)
      && cached.exists()
    {
      return Ok(Some(cached.clone()));
    }
    let sessions = self.codex_home.join(rollout::CODEX_SESSIONS_DIR_NAME);
    if !sessions.exists() {
      return Ok(None);
    }
    let suffix = format!("{thread_id}.jsonl");
    let matches = walk_files(&sessions, |name| name.starts_with("rollout-") && name.ends_with(&suffix))
      .with_context(|| format!("listing rollouts under {}", sessions.display()))?;
    let mut newest: Vec<(i128, PathBuf)> = matches
      .into_iter()
      .map(|path| {
        let metadata = fs::metadata(&path).with_context(|| format!("statting rollout {}", path.display()))?;
        Ok((mtime_ns(&metadata), path))
      })
      .collect::<anyhow::Result<_>>()?;
    newest.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime));
    let Some((_, path)) = newest.into_iter().next() else {
      return Ok(None);
    };
    self
      .rollout_paths
      .lock()
      .expect("the codex rollout path cache is not poisoned")
      .insert(thread_id.to_string(), path.clone());
    Ok(Some(path))
  }
}

impl UsageResolver for CodexUsageResolver {
  fn resolve(
    &self,
    session_id: &str,
    meta: &SessionMetadata,
    events: &[SharedChatEvent],
  ) -> anyhow::Result<Option<Map<String, Value>>> {
    self.resolve_with_host(session_id, meta, events)
  }
}

/// The first event's non-empty string session id.
fn translated_session_id(event: &impl EventFields) -> Option<String> {
  event.field("session_id").and_then(EventFields::as_str_field).filter(|id| !id.is_empty()).map(str::to_string)
}
