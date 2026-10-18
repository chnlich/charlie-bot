use std::collections::{HashMap, VecDeque};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use charliebot_core::config::CharlieBotConfig;
use charliebot_core::fsutil;
use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot};
use serde_json::{Map, Value};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::aggregator::builtin_event_types;
use crate::control::ControlLock;
use crate::hooks::{EventRenderer, RuntimeEventHooks};
use crate::metadata::{SessionMetadata, SessionStatus, UtcDateTime};
use crate::session_events::SessionEventState;
use crate::streaming::StreamingManager;
use crate::thinking::ThinkingState;
use crate::{ControlGuard, StoreError, StoreResult};

const METADATA_CACHE_TTL: Duration = Duration::from_secs(30);

/// Backend lifecycle owner for the account label held by a session.
pub trait AccountLabelKeeper: Send + Sync {
  /// Record a label on the metadata fields this lifecycle owns.
  fn record_account_label(&self, meta: &mut SessionMetadata, label: Option<&str>) -> StoreResult<bool>;
  /// Read the label back from its owning fields.
  fn account_label(&self, meta: &SessionMetadata) -> Option<String>;
}

/// Reconcile resume-anchor fields against the on-disk metadata before ordinary saves.
pub trait AnchorFieldReconciler: Send + Sync {
  /// Restore protected anchor values from `disk` onto `attempted`.
  fn reconcile(&self, disk: &SessionMetadata, attempted: &mut SessionMetadata) -> StoreResult<()>;
}

/// Runtime sequence contributions used by session listings.
pub trait SequenceListingFields: Send + Sync {
  /// Whether a sequence controller owns this session.
  fn owns_session(&self, meta: &SessionMetadata) -> bool;
  /// Return the derived sequence fields for the requested ids.
  fn listing_fields(
    &self,
    session_ids: &[String],
    now: UtcDateTime,
  ) -> StoreResult<HashMap<String, Map<String, Value>>>;
  /// Return sequence subtree roots over a metadata snapshot.
  fn sequence_subtree_roots(&self, metas: &[SessionMetadata]) -> StoreResult<HashMap<String, String>>;
  /// Return view subtree roots over a metadata snapshot.
  fn view_subtree_roots(&self, metas: &[SessionMetadata]) -> StoreResult<HashMap<String, HashMap<String, String>>>;
  /// Task-tree ids whose effective archive state is derived from durable facts.
  fn derived_archived_ids(&self) -> StoreResult<Vec<String>>;
}

/// Required runtime hooks. Missing implementations fail store construction.
#[derive(Default)]
pub struct StoreHooks {
  /// The backend lifecycle that owns account labels.
  pub account_labels: Option<Arc<dyn AccountLabelKeeper>>,
  /// Backend lifecycle anchor reconciliation.
  pub anchor_reconciler: Option<Arc<dyn AnchorFieldReconciler>>,
  /// Sequence-controller listing contributions.
  pub sequence_listing: Option<Arc<dyn SequenceListingFields>>,
  /// Feature event renderers, after-turn contributions, and runtime-state callbacks.
  pub runtime_events: Option<Arc<dyn RuntimeEventHooks>>,
}

#[derive(Clone)]
pub(crate) struct CachedMetadata {
  pub(crate) value: SessionMetadata,
  pub(crate) at: Instant,
  pub(crate) signature: Option<(i64, i64)>,
}

/// Session metadata and event-log persistence for one configured CharlieBot home.
pub struct Store {
  pub(crate) cfg: CharlieBotConfig,
  pub(crate) slots: Vec<MetadataSlot>,
  pub(crate) account_labels: Arc<dyn AccountLabelKeeper>,
  pub(crate) anchor_reconciler: Arc<dyn AnchorFieldReconciler>,
  pub(crate) sequence_listing: Arc<dyn SequenceListingFields>,
  pub(crate) runtime_hooks: Arc<dyn RuntimeEventHooks>,
  pub(crate) event_renderers: HashMap<String, Arc<dyn EventRenderer>>,
  pub(crate) metadata_cache: StdMutex<HashMap<String, CachedMetadata>>,
  pub(crate) metadata_locks: StdMutex<HashMap<String, Arc<Mutex<()>>>>,
  pub(crate) events_cache: StdMutex<HashMap<String, Vec<Value>>>,
  pub(crate) control: ControlLock,
  pub(crate) records_generation: AtomicU64,
  pub(crate) control_event_ids: StdMutex<Option<VecDeque<String>>>,
  pub(crate) session_events: SessionEventState,
  pub(crate) streaming_manager: StreamingManager,
  pub(crate) thinking_state: ThinkingState,
}

impl Store {
  /// Build a store after package registration has supplied slots and every required hook.
  pub fn new(cfg: CharlieBotConfig, slots: Vec<MetadataSlot>, hooks: StoreHooks) -> StoreResult<Self> {
    let account_labels =
      hooks.account_labels.ok_or_else(|| StoreError::new("store requires an account label keeper"))?;
    let anchor_reconciler =
      hooks.anchor_reconciler.ok_or_else(|| StoreError::new("store requires an anchor field reconciler"))?;
    let sequence_listing =
      hooks.sequence_listing.ok_or_else(|| StoreError::new("store requires sequence listing fields"))?;
    let runtime_hooks = hooks.runtime_events.ok_or_else(|| StoreError::new("store requires runtime event hooks"))?;
    let mut event_renderers = HashMap::new();
    for (event_type, renderer) in runtime_hooks.event_renderers()? {
      if builtin_event_types().contains(&event_type.as_str()) {
        return Err(StoreError::new(format!("event type {event_type:?} already has a built-in renderer")));
      }
      if event_renderers.insert(event_type.clone(), renderer).is_some() {
        return Err(StoreError::new(format!("event type {event_type:?} already has a renderer")));
      }
    }
    validate_slots(&slots)?;
    let store_id = Uuid::new_v4();
    Ok(Self {
      cfg,
      slots,
      account_labels,
      anchor_reconciler,
      sequence_listing,
      runtime_hooks,
      event_renderers,
      metadata_cache: StdMutex::new(HashMap::new()),
      metadata_locks: StdMutex::new(HashMap::new()),
      events_cache: StdMutex::new(HashMap::new()),
      control: ControlLock::new(store_id),
      records_generation: AtomicU64::new(0),
      control_event_ids: StdMutex::new(None),
      session_events: SessionEventState::default(),
      streaming_manager: StreamingManager::new(),
      thinking_state: ThinkingState::default(),
    })
  }

  /// The session directory for `session_id`.
  pub fn session_dir(&self, session_id: &str) -> PathBuf {
    self.cfg.sessions_dir().join(session_id)
  }

  /// The metadata file for `session_id`.
  pub fn metadata_path(&self, session_id: &str) -> PathBuf {
    self.session_dir(session_id).join("metadata.json")
  }

  /// Return cached session metadata, revalidating expired active entries by `(mtime_ns, size)`.
  pub async fn get_session(&self, session_id: &str) -> StoreResult<Option<SessionMetadata>> {
    if let Some(meta) = self.fresh_cached_meta(session_id)? {
      return Ok(Some(meta));
    }
    let path = self.metadata_path(session_id);
    let signature = stat_signature(&path);
    let Some(meta) = self.read_metadata_path(&path)? else { return Ok(None) };
    self
      .metadata_cache
      .lock()
      .expect("metadata cache mutex is not poisoned")
      .insert(session_id.to_string(), CachedMetadata { value: meta.clone(), at: Instant::now(), signature });
    Ok(Some(meta))
  }

  /// Read `metadata.json` from disk without consulting or populating the cache.
  pub fn read_metadata_fresh(&self, session_id: &str) -> StoreResult<Option<SessionMetadata>> {
    self.read_metadata_path(&self.metadata_path(session_id))
  }

  /// Return the directory where the live event log is stored.
  pub fn chat_events_path(&self, session_id: &str) -> PathBuf {
    self.session_dir(session_id).join("data").join("chat_events.jsonl")
  }

  /// Path to a legacy thread's metadata file.
  pub fn thread_metadata_path(&self, session_id: &str, thread_id: &str) -> PathBuf {
    self.session_dir(session_id).join("threads").join(thread_id).join("metadata.json")
  }

  /// Read one thread metadata file from disk.
  pub fn read_thread_metadata(&self, path: &Path) -> StoreResult<Option<crate::metadata::ThreadMetadata>> {
    let raw = match fs::read_to_string(path) {
      Ok(raw) => raw,
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
      Err(error) => return Err(StoreError::new(format!("{}: {error}", path.display()))),
    };
    if raw.trim().is_empty() {
      return Ok(None);
    }
    serde_json::from_str(&raw)
      .map(Some)
      .map_err(|error| StoreError::new(format!("{}: invalid thread metadata: {error}", path.display())))
  }

  /// Atomically write a thread metadata file using Python model field and slot order.
  pub fn write_thread_metadata(&self, path: &Path, metadata: &crate::metadata::ThreadMetadata) -> StoreResult<()> {
    let parent = path.parent().expect("thread metadata path has a parent");
    fs::create_dir_all(parent).map_err(|error| StoreError::new(format!("{}: {error}", parent.display())))?;
    fsutil::atomic_write_text(path, &metadata.to_json_string(&self.slots))
      .map_err(|error| StoreError::new(format!("{}: {error}", path.display())))
  }

  /// Invalidate cached metadata and listing snapshots after an external write or deletion.
  pub fn invalidate_cache(&self, session_id: &str) {
    self.metadata_cache.lock().expect("metadata cache mutex is not poisoned").remove(session_id);
  }

  pub(crate) async fn lock_for(&self, session_id: &str) -> tokio::sync::OwnedMutexGuard<()> {
    let lock = {
      let mut locks = self.metadata_locks.lock().expect("metadata lock map mutex is not poisoned");
      Arc::clone(locks.entry(session_id.to_string()).or_insert_with(|| Arc::new(Mutex::new(()))))
    };
    lock.lock_owned().await
  }

  pub(crate) fn fresh_cached_meta(&self, session_id: &str) -> StoreResult<Option<SessionMetadata>> {
    let mut cache = self.metadata_cache.lock().expect("metadata cache mutex is not poisoned");
    let Some(entry) = cache.get_mut(session_id) else { return Ok(None) };
    if entry.value.status == SessionStatus::Archived || entry.at.elapsed() < METADATA_CACHE_TTL {
      return Ok(Some(entry.value.clone()));
    }
    if let Some(signature) = entry.signature
      && stat_signature(&self.metadata_path(session_id)) == Some(signature)
    {
      entry.at = Instant::now();
      return Ok(Some(entry.value.clone()));
    }
    cache.remove(session_id);
    Ok(None)
  }

  pub(crate) fn fresh_cached_metas(&self) -> StoreResult<HashMap<String, SessionMetadata>> {
    let ids: Vec<String> =
      self.metadata_cache.lock().expect("metadata cache mutex is not poisoned").keys().cloned().collect();
    let mut fresh = HashMap::new();
    for id in ids {
      if let Some(meta) = self.fresh_cached_meta(&id)? {
        fresh.insert(id, meta);
      }
    }
    Ok(fresh)
  }

  pub(crate) fn read_metadata_path(&self, path: &Path) -> StoreResult<Option<SessionMetadata>> {
    let raw = match fs::read_to_string(path) {
      Ok(raw) => raw,
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
      Err(error) => return Err(StoreError::new(format!("{}: {error}", path.display()))),
    };
    if raw.trim().is_empty() {
      return Ok(None);
    }
    SessionMetadata::from_json(&raw, Some(&path.display().to_string())).map(Some)
  }

  pub(crate) async fn mutate_fresh<F>(
    &self,
    session_id: &str,
    anchor_write: bool,
    always_write: bool,
    mutate: F,
  ) -> StoreResult<Option<SessionMetadata>>
  where
    F: FnOnce(&mut SessionMetadata) -> StoreResult<bool>,
  {
    let _guard = self.lock_for(session_id).await;
    let Some(mut meta) = self.read_metadata_fresh(session_id)? else { return Ok(None) };
    let changed = mutate(&mut meta)?;
    if changed || always_write {
      self.save_metadata_locked(&mut meta, anchor_write)?;
    }
    self.read_metadata_fresh(session_id)
  }

  pub(crate) fn save_metadata_locked(&self, meta: &mut SessionMetadata, anchor_write: bool) -> StoreResult<()> {
    if !anchor_write && let Some(disk) = self.read_metadata_fresh(&meta.id)? {
      self.anchor_reconciler.reconcile(&disk, meta)?;
    }
    let path = self.metadata_path(&meta.id);
    let parent = path.parent().expect("metadata file path has a parent");
    fs::create_dir_all(parent).map_err(|error| StoreError::new(format!("{}: {error}", parent.display())))?;
    let serialized = meta.to_json_string(&self.slots, true);
    fsutil::atomic_write_text(&path, &serialized)
      .map_err(|error| StoreError::new(format!("{}: {error}", path.display())))?;
    let parsed = SessionMetadata::from_json(&serialized, Some(&path.display().to_string()))?;
    let signature = stat_signature(&path);
    self
      .metadata_cache
      .lock()
      .expect("metadata cache mutex is not poisoned")
      .insert(meta.id.clone(), CachedMetadata { value: parsed, at: Instant::now(), signature });
    Ok(())
  }

  /// Persist the native session id and return the value read back from disk.
  pub async fn persist_cc_session_id(
    &self,
    session_id: &str,
    cc_session_id: &str,
    native_backend: Option<&str>,
  ) -> StoreResult<Option<String>> {
    self.persist_cc_session_id_at(session_id, cc_session_id, native_backend, UtcDateTime::now()).await
  }

  /// Persist the native session id with an explicit clock value for deterministic replay.
  pub async fn persist_cc_session_id_at(
    &self,
    session_id: &str,
    cc_session_id: &str,
    native_backend: Option<&str>,
    now: UtcDateTime,
  ) -> StoreResult<Option<String>> {
    let saved = self
      .mutate_fresh(session_id, true, true, |meta| {
        if meta.cc_session_id.as_deref() != Some(cc_session_id) {
          meta.cc_session_id = Some(cc_session_id.to_string());
          meta.cc_session_started_at = Some(now);
        }
        if let Some(backend) = native_backend {
          meta.native_backend = Some(backend.to_string());
        }
        Ok(true)
      })
      .await?;
    Ok(saved.and_then(|meta| meta.cc_session_id))
  }

  /// Persist the backend that produced the native conversation.
  pub async fn persist_native_backend(&self, session_id: &str, native_backend: &str) -> StoreResult<Option<String>> {
    let saved = self
      .mutate_fresh(session_id, true, false, |meta| {
        if meta.native_backend.as_deref() == Some(native_backend) {
          return Ok(false);
        }
        meta.native_backend = Some(native_backend.to_string());
        Ok(true)
      })
      .await?;
    Ok(saved.and_then(|meta| meta.native_backend))
  }

  /// Persist provenance for the held native conversation.
  pub async fn persist_native_anchor_provenance(
    &self,
    session_id: &str,
    prompt_hash: &str,
    native_backend: &str,
    model: Option<&str>,
  ) -> StoreResult<()> {
    self
      .mutate_fresh(session_id, true, true, |meta| {
        meta.native_prompt_hash = Some(prompt_hash.to_string());
        meta.native_backend = Some(native_backend.to_string());
        meta.native_model = model.map(str::to_string);
        Ok(true)
      })
      .await?;
    Ok(())
  }

  /// Clear the held conversation id and its start timestamp.
  pub async fn clear_cc_session_anchor(&self, session_id: &str) -> StoreResult<()> {
    self
      .mutate_fresh(session_id, true, true, |meta| {
        meta.cc_session_id = None;
        meta.cc_session_started_at = None;
        Ok(true)
      })
      .await?;
    Ok(())
  }

  /// Persist an account label through its registered lifecycle and read it back.
  pub async fn persist_account_label(&self, session_id: &str, label: &str) -> StoreResult<Option<String>> {
    let saved = self
      .mutate_fresh(session_id, true, false, |meta| self.account_labels.record_account_label(meta, Some(label)))
      .await?;
    Ok(saved.and_then(|meta| self.account_labels.account_label(&meta)))
  }

  /// Update `updated_at` without saving a cached metadata object.
  pub async fn update_thinking_state(&self, session_id: &str, updated_at: UtcDateTime) -> StoreResult<()> {
    self
      .mutate_fresh(session_id, false, true, |meta| {
        meta.updated_at = updated_at;
        Ok(true)
      })
      .await?;
    Ok(())
  }

  /// Change one registered slot's fields and read the metadata back from disk.
  pub async fn update_slot_fields(
    &self,
    session_id: &str,
    owner: &str,
    values: Map<String, Value>,
  ) -> StoreResult<Option<SessionMetadata>> {
    self.update_slot_fields_fresh(session_id, owner, values).await
  }

  /// Change one registered slot's fields under the tree-wide control lock and session lock.
  pub async fn update_slot_fields_locked(
    &self,
    guard: &ControlGuard,
    session_id: &str,
    owner: &str,
    values: Map<String, Value>,
  ) -> StoreResult<Option<SessionMetadata>> {
    self.require_control_guard(guard)?;
    self.update_slot_fields_fresh(session_id, owner, values).await
  }

  async fn update_slot_fields_fresh(
    &self,
    session_id: &str,
    owner: &str,
    values: Map<String, Value>,
  ) -> StoreResult<Option<SessionMetadata>> {
    if values.is_empty() {
      return Err(StoreError::new("update_slot_fields requires at least one field"));
    }
    let slot = self
      .slots
      .iter()
      .find(|slot| slot.owner == owner && slot.on == MetadataFile::Session)
      .ok_or_else(|| StoreError::new(format!("{owner:?} registered no session metadata fields")))?;
    for key in values.keys() {
      if !slot.fields.iter().any(|field| field.name == key) {
        return Err(StoreError::new(format!("{owner:?} has no field {key:?}")));
      }
    }
    self
      .mutate_fresh(session_id, false, true, move |meta| {
        for (key, value) in values {
          meta.set_extra(key, value);
        }
        Ok(true)
      })
      .await
  }

  /// Rename a session and update its activity timestamp.
  pub async fn rename_session(&self, session_id: &str, name: &str) -> StoreResult<Option<SessionMetadata>> {
    self.rename_session_at(session_id, name, UtcDateTime::now()).await
  }

  /// Rename a session with an explicit activity timestamp for deterministic replay.
  pub async fn rename_session_at(
    &self,
    session_id: &str,
    name: &str,
    updated_at: UtcDateTime,
  ) -> StoreResult<Option<SessionMetadata>> {
    self
      .update_field(
        session_id,
        |meta| {
          meta.name = name.to_string();
          Ok(())
        },
        updated_at,
      )
      .await
  }

  /// Assign the session to a group and update its activity timestamp.
  pub async fn set_group(&self, session_id: &str, group: Option<&str>) -> StoreResult<Option<SessionMetadata>> {
    self.set_group_at(session_id, group, UtcDateTime::now()).await
  }

  /// Set a group with an explicit activity timestamp for deterministic replay.
  pub async fn set_group_at(
    &self,
    session_id: &str,
    group: Option<&str>,
    updated_at: UtcDateTime,
  ) -> StoreResult<Option<SessionMetadata>> {
    self
      .update_field(
        session_id,
        |meta| {
          meta.group = group.map(str::to_string);
          Ok(())
        },
        updated_at,
      )
      .await
  }

  /// Switch the session's selected backend and update its activity timestamp.
  pub async fn switch_backend(&self, session_id: &str, backend: &str) -> StoreResult<Option<SessionMetadata>> {
    self.switch_backend_at(session_id, backend, UtcDateTime::now()).await
  }

  /// Switch backend with an explicit activity timestamp for deterministic replay.
  pub async fn switch_backend_at(
    &self,
    session_id: &str,
    backend: &str,
    updated_at: UtcDateTime,
  ) -> StoreResult<Option<SessionMetadata>> {
    self
      .update_field(
        session_id,
        |meta| {
          meta.backend = backend.to_string();
          Ok(())
        },
        updated_at,
      )
      .await
  }

  /// Archive a session without removing its files.
  pub async fn archive_session(&self, session_id: &str) -> StoreResult<Option<SessionMetadata>> {
    self.archive_session_at(session_id, UtcDateTime::now()).await
  }

  /// Archive a session with an explicit activity timestamp for deterministic replay.
  pub async fn archive_session_at(
    &self,
    session_id: &str,
    updated_at: UtcDateTime,
  ) -> StoreResult<Option<SessionMetadata>> {
    self
      .update_field(
        session_id,
        |meta| {
          meta.status = SessionStatus::Archived;
          Ok(())
        },
        updated_at,
      )
      .await
  }

  /// Restore an archived session.
  pub async fn unarchive_session(&self, session_id: &str) -> StoreResult<Option<SessionMetadata>> {
    self.unarchive_session_at(session_id, UtcDateTime::now()).await
  }

  /// Restore a session with an explicit activity timestamp for deterministic replay.
  pub async fn unarchive_session_at(
    &self,
    session_id: &str,
    updated_at: UtcDateTime,
  ) -> StoreResult<Option<SessionMetadata>> {
    self
      .update_field(
        session_id,
        |meta| {
          meta.status = SessionStatus::Active;
          Ok(())
        },
        updated_at,
      )
      .await
  }

  /// Mark a session unread without changing its activity timestamp.
  pub async fn mark_unread(&self, session_id: &str) -> StoreResult<Option<SessionMetadata>> {
    self
      .mutate_fresh(session_id, false, false, |meta| {
        if meta.has_unread {
          return Ok(false);
        }
        meta.has_unread = true;
        Ok(true)
      })
      .await
  }

  /// Acquire the process-wide, non-reentrant task-tree control lock.
  pub async fn control_lock(&self) -> ControlGuard {
    self.control.lock().await
  }

  /// Check that `guard` proves ownership of this store's control lock.
  pub(crate) fn require_control_guard(&self, guard: &ControlGuard) -> StoreResult<()> {
    self.control.validate(guard).map_err(StoreError::new)
  }

  async fn update_field<F>(
    &self,
    session_id: &str,
    mutate: F,
    updated_at: UtcDateTime,
  ) -> StoreResult<Option<SessionMetadata>>
  where
    F: FnOnce(&mut SessionMetadata) -> StoreResult<()>,
  {
    self
      .mutate_fresh(session_id, false, true, |meta| {
        mutate(meta)?;
        meta.updated_at = updated_at;
        Ok(true)
      })
      .await
  }
}

fn stat_signature(path: &Path) -> Option<(i64, i64)> {
  let metadata = fs::metadata(path).ok()?;
  Some((metadata.mtime_nsec() + metadata.mtime() * 1_000_000_000, metadata.size() as i64))
}

fn validate_slots(slots: &[MetadataSlot]) -> StoreResult<()> {
  let mut owners = Vec::new();
  let mut names = Vec::new();
  for slot in slots {
    if owners.contains(&(slot.owner, slot.on)) {
      return Err(StoreError::new(format!("metadata owner {:?} is already registered for {:?}", slot.owner, slot.on)));
    }
    owners.push((slot.owner, slot.on));
    let declared: &[&str] = match slot.on {
      MetadataFile::Session => &[
        "id",
        "name",
        "status",
        "has_unread",
        "has_running_tasks",
        "work_state",
        "has_pending_trigger",
        "pending_trigger_count",
        "next_trigger_at",
        "has_pending_plan_approval",
        "starred",
        "thinking_since",
        "run_backend",
        "created_at",
        "updated_at",
        "cc_session_id",
        "cc_session_started_at",
        "backend",
        "schedule_cron",
        "schedule_enabled",
        "schedule_next_run",
        "schedule_timezone",
        "schedule_project",
        "schedule_allow_failure",
        "parent_session_id",
        "successor_session_id",
        "schema_version",
        "task_parent_id",
        "profile",
        "task",
        "created_by_event",
        "origin_ref",
        "subtree_prompt_ref",
        "node_prompt_ref",
        "native_prompt_hash",
        "native_backend",
        "native_model",
        "round_ratings",
        "group",
        "archive_offset",
      ],
      MetadataFile::Thread => &[
        "id",
        "session_id",
        "description",
        "status",
        "created_at",
        "started_at",
        "completed_at",
        "pid",
        "pid_start",
        "exit_code",
        "branch_name",
        "base_branch",
        "repo_path",
        "worktree_path",
        "context",
        "backend",
        "model",
        "keep_worktree",
        "tried_backends",
        "task_type",
      ],
    };
    if let Some(after) = slot.after
      && !declared.contains(&after)
    {
      return Err(StoreError::new(format!("{after:?} is not a declared {:?} metadata field", slot.on)));
    }
    for field in &slot.fields {
      if declared.contains(&field.name) || names.contains(&(slot.on, field.name)) {
        return Err(StoreError::new(format!("metadata slot field {:?} collides with another field", field.name)));
      }
      names.push((slot.on, field.name));
    }
  }
  Ok(())
}

#[cfg(test)]
pub(crate) mod test_support {
  use super::*;
  use serde_json::json;

  use crate::hooks::EmptyRuntimeEventHooks;

  /// An assistant text event with an explicit id (`conftest.assistant_event`).
  pub(crate) fn assistant_event(text: &str, event_id: &str) -> Value {
    json!({"id": event_id, "type": "assistant", "message": {"content": [{"type": "text", "text": text}]}})
  }

  /// An assistant text event without id or timestamp (`conftest.assistant_text_event`).
  pub(crate) fn assistant_text_event(text: &str) -> Value {
    json!({"type": "assistant", "message": {"content": [{"type": "text", "text": text}]}})
  }

  /// A text block then a tool_use block, stamped (`conftest.assistant_text_tool_use_event`).
  pub(crate) fn assistant_text_tool_use_event(
    text: &str,
    tool_name: &str,
    tool_input: Value,
    timestamp: &str,
  ) -> Value {
    json!({
      "type": "assistant",
      "message": {"content": [
        {"type": "text", "text": text},
        {"type": "tool_use", "name": tool_name, "input": tool_input},
      ]},
      "timestamp": timestamp,
    })
  }

  /// Two runs on one session with a queued user inside the first (`conftest.queued_user_reorder_events`).
  pub(crate) fn queued_user_reorder_events() -> Vec<Value> {
    vec![
      json!({"session_id": "opencode-session"}),
      json!({"id": "thinking-1", "type": "thinking", "content": "final thought"}),
      json!({"id": "tool-1", "type": "tool_use", "name": "Read", "input": {"file_path": "report.txt"}}),
      json!({"id": "queued-user", "type": "user", "content": "second question"}),
      json!({"id": "tool-result-1", "type": "tool_result", "content": "report contents"}),
      assistant_event("first conclusion", "assistant-1"),
      json!({"id": "done-1", "type": "master_done", "thinking_seconds": 4}),
      json!({"session_id": "opencode-session"}),
      assistant_event("second answer", "assistant-2"),
      json!({"id": "done-2", "type": "master_done", "thinking_seconds": 2}),
    ]
  }

  struct AccountLabels;

  impl AccountLabelKeeper for AccountLabels {
    fn record_account_label(&self, meta: &mut SessionMetadata, label: Option<&str>) -> StoreResult<bool> {
      let value = label.map_or(Value::Null, |label| Value::String(label.to_string()));
      if meta.extra("label") == Some(&value) {
        return Ok(false);
      }
      meta.set_extra("label", value);
      Ok(true)
    }

    fn account_label(&self, meta: &SessionMetadata) -> Option<String> {
      meta.extra("label").and_then(Value::as_str).map(str::to_string)
    }
  }

  struct AnchorReconciler;

  impl AnchorFieldReconciler for AnchorReconciler {
    fn reconcile(&self, disk: &SessionMetadata, attempted: &mut SessionMetadata) -> StoreResult<()> {
      attempted.cc_session_id.clone_from(&disk.cc_session_id);
      attempted.native_backend.clone_from(&disk.native_backend);
      Ok(())
    }
  }

  struct SequenceListing;

  impl SequenceListingFields for SequenceListing {
    fn owns_session(&self, _meta: &SessionMetadata) -> bool {
      false
    }

    fn listing_fields(
      &self,
      _session_ids: &[String],
      _now: UtcDateTime,
    ) -> StoreResult<HashMap<String, Map<String, Value>>> {
      Ok(HashMap::new())
    }

    fn sequence_subtree_roots(&self, _metas: &[SessionMetadata]) -> StoreResult<HashMap<String, String>> {
      Ok(HashMap::new())
    }

    fn view_subtree_roots(&self, _metas: &[SessionMetadata]) -> StoreResult<HashMap<String, HashMap<String, String>>> {
      Ok(HashMap::new())
    }

    fn derived_archived_ids(&self) -> StoreResult<Vec<String>> {
      Ok(Vec::new())
    }
  }

  pub(crate) fn make_test_store() -> (tempfile::TempDir, Store) {
    make_test_store_with_runtime(Arc::new(EmptyRuntimeEventHooks))
  }

  pub(crate) fn make_test_store_with_runtime(runtime_events: Arc<dyn RuntimeEventHooks>) -> (tempfile::TempDir, Store) {
    let home = tempfile::tempdir().unwrap();
    let store = open_test_store(home.path(), runtime_events).unwrap();
    (home, store)
  }

  /// Open a store on an existing test home, the way a restarted process does.
  pub(crate) fn open_test_store(
    home: &std::path::Path,
    runtime_events: Arc<dyn RuntimeEventHooks>,
  ) -> StoreResult<Store> {
    let cfg = charliebot_core::config::load_config(home, &charliebot_core::config::ConfigSchema::default()).unwrap();
    Store::new(
      cfg,
      Vec::new(),
      StoreHooks {
        account_labels: Some(Arc::new(AccountLabels)),
        anchor_reconciler: Some(Arc::new(AnchorReconciler)),
        sequence_listing: Some(Arc::new(SequenceListing)),
        runtime_events: Some(runtime_events),
      },
    )
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  struct TestAccountLabels;

  impl AccountLabelKeeper for TestAccountLabels {
    fn record_account_label(&self, meta: &mut SessionMetadata, label: Option<&str>) -> StoreResult<bool> {
      let value = label.map_or(Value::Null, |label| Value::String(label.to_string()));
      if meta.extra("label") == Some(&value) {
        return Ok(false);
      }
      meta.set_extra("label", value);
      Ok(true)
    }

    fn account_label(&self, meta: &SessionMetadata) -> Option<String> {
      meta.extra("label").and_then(Value::as_str).map(str::to_string)
    }
  }

  struct TestAnchorReconciler;

  impl AnchorFieldReconciler for TestAnchorReconciler {
    fn reconcile(&self, disk: &SessionMetadata, attempted: &mut SessionMetadata) -> StoreResult<()> {
      attempted.cc_session_id.clone_from(&disk.cc_session_id);
      attempted.native_backend.clone_from(&disk.native_backend);
      Ok(())
    }
  }

  struct TestSequenceListing;

  impl SequenceListingFields for TestSequenceListing {
    fn owns_session(&self, _meta: &SessionMetadata) -> bool {
      false
    }

    fn listing_fields(
      &self,
      _session_ids: &[String],
      _now: UtcDateTime,
    ) -> StoreResult<HashMap<String, Map<String, Value>>> {
      Ok(HashMap::new())
    }

    fn sequence_subtree_roots(&self, _metas: &[SessionMetadata]) -> StoreResult<HashMap<String, String>> {
      Ok(HashMap::new())
    }

    fn view_subtree_roots(&self, _metas: &[SessionMetadata]) -> StoreResult<HashMap<String, HashMap<String, String>>> {
      Ok(HashMap::new())
    }

    fn derived_archived_ids(&self) -> StoreResult<Vec<String>> {
      Ok(Vec::new())
    }
  }

  fn explicit_test_hooks() -> StoreHooks {
    StoreHooks {
      account_labels: Some(Arc::new(TestAccountLabels)),
      anchor_reconciler: Some(Arc::new(TestAnchorReconciler)),
      sequence_listing: Some(Arc::new(TestSequenceListing)),
      runtime_events: Some(Arc::new(crate::hooks::EmptyRuntimeEventHooks)),
    }
  }

  #[test]
  fn construction_fails_when_required_hooks_are_missing() {
    let home = tempfile::tempdir().unwrap();
    let cfg =
      charliebot_core::config::load_config(home.path(), &charliebot_core::config::ConfigSchema::default()).unwrap();
    let error = Store::new(cfg, Vec::new(), StoreHooks::default()).err().unwrap();
    assert!(error.to_string().contains("account label keeper"));
  }

  #[tokio::test]
  async fn expired_cache_entries_revalidate_the_disk_signature() {
    let home = tempfile::tempdir().unwrap();
    let cfg =
      charliebot_core::config::load_config(home.path(), &charliebot_core::config::ConfigSchema::default()).unwrap();
    let store = Store::new(cfg, Vec::new(), explicit_test_hooks()).unwrap();
    let mut meta = SessionMetadata::new("synthetic-id", "seed", crate::metadata::TaskProfile::Manager);
    let path = store.metadata_path(&meta.id);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, meta.to_json_string(&[], true)).unwrap();
    let signature = stat_signature(&path).unwrap();
    store.metadata_cache.lock().unwrap().insert(
      meta.id.clone(),
      CachedMetadata {
        value: meta.clone(),
        at: Instant::now() - METADATA_CACHE_TTL - Duration::from_secs(1),
        signature: Some(signature),
      },
    );
    assert_eq!(store.get_session(&meta.id).await.unwrap().unwrap().name, "seed");

    meta.name = "fresh metadata".to_string();
    fs::write(&path, meta.to_json_string(&[], true)).unwrap();
    store.metadata_cache.lock().unwrap().get_mut(&meta.id).unwrap().at =
      Instant::now() - METADATA_CACHE_TTL - Duration::from_secs(1);
    assert_eq!(store.get_session(&meta.id).await.unwrap().unwrap().name, "fresh metadata");
  }

  #[test]
  fn a_compact_boundary_without_a_count_keeps_the_earlier_count() {
    use serde_json::json;

    let home = tempfile::tempdir().unwrap();
    let cfg =
      charliebot_core::config::load_config(home.path(), &charliebot_core::config::ConfigSchema::default()).unwrap();
    let store = Store::new(cfg, Vec::new(), explicit_test_hooks()).unwrap();
    let session_id = "synthetic-events-session";
    let path = store.chat_events_path(session_id);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let lines = [
      json!({"type": "assistant", "message": {"usage": {"input_tokens": 500}}}),
      json!({"type": "system", "subtype": "compact_boundary", "compact_metadata": {"post_tokens": 120}}),
      json!({"type": "system", "subtype": "compact_boundary", "compact_metadata": {}}),
    ];
    let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
    fs::write(&path, text).unwrap();
    assert_eq!(store.context_state(session_id).unwrap().0, Some(120));
  }
}
