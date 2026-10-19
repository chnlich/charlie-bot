//! Durable task nodes, fact-derived tree state, and delegation authorization.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use charliebot_core::{event_types, fsutil};
use serde_json::{Map, Value, json};

use crate::control_events::{self, ACTOR_AGENT, ACTOR_SYSTEM, ACTOR_USER};
use crate::metadata::{EventRef, SessionMetadata, SessionStatus, TaskProfile, TaskSpec, TaskType, UtcDateTime};
use crate::run_records::{RunDisplayState, stop_requested_in_events};
use crate::{ControlGuard, Store, StoreError, StoreResult};

const TREE_INDEX_TTL: Duration = Duration::from_secs(2);
const TASK_ANCESTOR_HOP_LIMIT: usize = 1000;
const SUCCESSOR_HOP_LIMIT: usize = 100;
/// The refusal for an agent create outside its own open manager task (`task_sessions.AGENT_CREATE_SCOPE_REFUSAL`).
pub const AGENT_CREATE_SCOPE_REFUSAL: &str = "an agent may only create a task directly under its own open manager task";
/// The refusal for a task create without a request id (`task_sessions.TASK_CREATE_REQUEST_ID_REQUIRED`).
pub const TASK_CREATE_REQUEST_ID_REQUIRED: &str = "request_id is required for task creation";

/// One create request, including its stable identity inputs.
#[derive(Clone, Debug)]
pub struct CreateTask {
  pub request_id: String,
  pub task_parent_id: Option<String>,
  pub profile: TaskProfile,
  pub task: Option<TaskSpec>,
  pub name: Option<String>,
  pub backend: Option<String>,
  pub group: Option<String>,
  pub session_id: Option<String>,
}

/// The verified source of a task create request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskCaller {
  Operator,
  System,
  Agent { session_id: String },
}

/// The task fields of one PATCH body. A `None` field is absent from the body and changes nothing; `Some(None)` is an
/// explicit null, which clears the field where the Python model allows it. `name` has no explicit null: a `None` name
/// changes nothing.
#[derive(Clone, Debug, Default)]
pub struct PatchTask {
  pub name: Option<String>,
  pub task_parent_id: Option<Option<String>>,
  pub profile: Option<Option<TaskProfile>>,
  pub task: Option<Option<TaskSpec>>,
  pub subtree_prompt: Option<Option<String>>,
  pub node_prompt: Option<Option<String>>,
}

/// The structural guards that the task dispatcher answers for a node (`TaskTreeManager.pending_input_blockers` and
/// `TaskInputDispatcher.undelivered_report_blockers`). The store does not own them: the dispatcher reads the input
/// boundary and the report deliveries through the store, so the dispatcher implements this trait and the caller passes
/// it in.
pub trait StructuralBlockers: Send + Sync {
  /// The blockers of the node's unprocessed task inputs; empty when it has none.
  fn pending_input_blockers(&self, session_id: &str) -> StoreResult<Vec<String>>;
  /// One blocker for each close report that the node still owes its fixed recipient.
  fn undelivered_report_blockers(&self, session_id: &str) -> StoreResult<Vec<String>>;
}

/// The task lifecycle folded from a node's durable event history.
pub type TaskState = String;

/// A fact snapshot used by tree, gate, and pending-input readers.
#[derive(Clone, Debug)]
pub struct TaskFacts {
  pub task_state: TaskState,
  pub run_outcomes: HashMap<String, String>,
  pub run_finish_positions: HashMap<String, usize>,
  pub confirmed_input_ids: HashSet<String>,
  pub input_candidates: Vec<Value>,
  pub imported_pending_ids: HashSet<String>,
  pub boundary_index: Option<usize>,
  pub boundary_open: bool,
  pub close_events: Vec<Value>,
  pub close_requests: Vec<Value>,
  pub delivered_reports: HashSet<(String, String)>,
  pub event_ids: HashSet<String>,
  pub event_order: Vec<String>,
  pub events_by_id: HashMap<String, Value>,
  pub covered: usize,
}

impl Default for TaskFacts {
  fn default() -> Self {
    Self {
      task_state: "open".to_string(),
      run_outcomes: HashMap::new(),
      run_finish_positions: HashMap::new(),
      confirmed_input_ids: HashSet::new(),
      input_candidates: Vec::new(),
      imported_pending_ids: HashSet::new(),
      boundary_index: None,
      boundary_open: true,
      close_events: Vec::new(),
      close_requests: Vec::new(),
      delivered_reports: HashSet::new(),
      event_ids: HashSet::new(),
      event_order: Vec::new(),
      events_by_id: HashMap::new(),
      covered: 0,
    }
  }
}

/// The node's current running and queued work verdict.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskTreeActivity {
  pub has_running_tasks: bool,
  pub work_state: String,
}

/// The prompt rule scope whose fact is being reconciled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PromptScope {
  Subtree,
  Node,
}

impl PromptScope {
  fn as_str(self) -> &'static str {
    match self {
      Self::Subtree => "subtree",
      Self::Node => "node",
    }
  }

  fn metadata_ref(self, meta: &SessionMetadata) -> Option<&str> {
    match self {
      Self::Subtree => meta.subtree_prompt_ref.as_deref(),
      Self::Node => meta.node_prompt_ref.as_deref(),
    }
  }

  fn set_metadata_ref(self, meta: &mut SessionMetadata, value: Option<String>) {
    match self {
      Self::Subtree => meta.subtree_prompt_ref = value,
      Self::Node => meta.node_prompt_ref = value,
    }
  }
}

#[derive(Clone)]
struct TreeIndex {
  metas: HashMap<String, SessionMetadata>,
  children: HashMap<Option<String>, Vec<String>>,
}

pub(crate) struct CachedTreeIndex {
  generation: u64,
  built_at: Instant,
  index: TreeIndex,
}

impl Store {
  /// Read one session through the metadata cache.
  pub async fn load_meta(&self, session_id: &str) -> StoreResult<Option<SessionMetadata>> {
    self.get_session(session_id).await
  }

  /// Read one required task node through the metadata cache.
  pub async fn load_task_meta(&self, session_id: &str) -> StoreResult<SessionMetadata> {
    self.load_meta(session_id).await?.ok_or_else(|| StoreError::not_found(format!("task {session_id} not found")))
  }

  /// Return the selected home’s immutable prompt body directory.
  pub fn prompt_bodies_dir(&self) -> PathBuf {
    self.cfg.charliebot_home.join("prompt_bodies")
  }

  /// Read all durable facts and fold the task lifecycle and input boundary.
  pub fn facts_of(&self, session_id: &str) -> StoreResult<TaskFacts> {
    let events = self.load_all_facts(session_id)?;
    fold_task_facts(&events)
  }

  /// Read the node’s fact-derived lifecycle state.
  pub fn task_state(&self, session_id: &str) -> StoreResult<TaskState> {
    Ok(self.facts_of(session_id)?.task_state)
  }

  /// Rebuild the cached task index after a node metadata write.
  pub fn invalidate_tree_index(&self) {
    self.task_tree_generation.fetch_add(1, Ordering::AcqRel);
    self.task_tree_index.lock().expect("task tree index mutex is not poisoned").take();
  }

  /// Return a node’s task ancestors, nearest first.
  pub fn ancestor_ids(&self, session_id: &str) -> StoreResult<Vec<String>> {
    let index = self.tree_index()?;
    ancestor_ids(&index, session_id)
  }

  /// Return one node’s direct task children in creation order.
  pub fn child_ids(&self, session_id: &str) -> StoreResult<Vec<String>> {
    let index = self.tree_index()?;
    if !index.metas.contains_key(session_id) {
      return Err(StoreError::not_found(format!("task {session_id} not found")));
    }
    Ok(index.children.get(&Some(session_id.to_string())).cloned().unwrap_or_default())
  }

  /// Return every descendant in the task index’s depth-first order.
  pub fn descendant_ids(&self, session_id: &str) -> StoreResult<Vec<String>> {
    let index = self.tree_index()?;
    descendant_ids(&index, session_id)
  }

  /// Return every task node whose effective archive state is inherited or fact-derived.
  pub fn derived_archived_ids(&self) -> StoreResult<Vec<String>> {
    let index = self.tree_index()?;
    let mut ids = Vec::new();
    for meta in index.metas.values() {
      if meta.status != SessionStatus::Archived && archived_of(self, &index, &meta.id)? {
        ids.push(meta.id.clone());
      }
    }
    ids.sort();
    Ok(ids)
  }

  /// Return a task’s effective archive state, including archived ancestors.
  pub fn archived_of(&self, session_id: &str) -> StoreResult<bool> {
    archived_of(self, &self.tree_index()?, session_id)
  }

  /// Require every ancestor of a node to remain open.
  pub fn require_open_ancestry(&self, session_id: &str) -> StoreResult<Vec<String>> {
    let index = self.tree_index()?;
    let ancestors = ancestor_ids(&index, session_id)?;
    let mut closed = Vec::new();
    for ancestor_id in &ancestors {
      if self.task_state(ancestor_id)? != "open" {
        closed.push(ancestor_id.clone());
      }
    }
    if !closed.is_empty() {
      return Err(StoreError::conflict(vec![closed_ancestors_blocker(&closed)]));
    }
    Ok(ancestors)
  }

  /// Return the nearest node on the open manager chain that holds a real user message. A refusal is
  /// `forbidden`: `takeoff_gate.DelegationBlockedError` is a 403 on every route.
  pub fn check_task_authorization(&self, session_id: &str) -> StoreResult<String> {
    let index = self.tree_index()?;
    let mut current = session_id.to_string();
    let mut user_node = None;
    for _ in 0..TASK_ANCESTOR_HOP_LIMIT {
      let metadata = index.metas.get(&current);
      let (parent_id, profile) =
        metadata.map_or((None, None), |meta| (meta.task_parent_id.as_deref(), Some(meta.profile)));
      let is_start = current == session_id;
      if is_start && profile != Some(TaskProfile::Manager) {
        return Err(StoreError::forbidden(format!(
          "task {current} is not a manager; agent calls run only under a manager task"
        )));
      }
      let state = if metadata.is_some() { self.task_state(&current)? } else { "open".to_string() };
      if state != "open" {
        let kind = if is_start { "task" } else { "ancestor task" };
        return Err(StoreError::forbidden(format!("Delegation blocked: {kind} {current} is {state}; open it first")));
      }
      if user_node.is_none() && holds_real_user_message(&self.load_all_facts(&current)?) {
        user_node = Some(current.clone());
      }
      let Some(parent_id) = parent_id else {
        return user_node.ok_or_else(|| no_user_message_error(session_id));
      };
      current = parent_id.to_string();
    }
    Err(StoreError::forbidden(format!(
      "Delegation blocked: the task chain from task {session_id} exceeds {TASK_ANCESTOR_HOP_LIMIT} hops without reaching a root"
    )))
  }

  /// Check the caller scope and task gate for an agent create request.
  pub fn runtime_gate(&self, request: &CreateTask, caller: &TaskCaller) -> StoreResult<()> {
    let TaskCaller::Agent { session_id } = caller else { return Ok(()) };
    if request.session_id.is_some() {
      return Err(StoreError::forbidden(AGENT_CREATE_SCOPE_REFUSAL));
    }
    if request.task_parent_id.as_deref() != Some(session_id.as_str()) {
      return Err(StoreError::forbidden(AGENT_CREATE_SCOPE_REFUSAL));
    }
    let parent = self.read_metadata_fresh(session_id)?;
    if parent.as_ref().is_none_or(|meta| meta.profile != TaskProfile::Manager) {
      return Err(StoreError::forbidden(AGENT_CREATE_SCOPE_REFUSAL));
    }
    if request.profile == TaskProfile::Worker && !is_read_only_exempt(request.task.as_ref()) {
      self.check_task_authorization(session_id)?;
    }
    Ok(())
  }

  /// Create one task node and replay the original product for a repeated request id.
  pub async fn create_task(
    &self,
    request: CreateTask,
    caller: TaskCaller,
    slot_values: Map<String, Value>,
  ) -> StoreResult<SessionMetadata> {
    if request.request_id.is_empty() {
      return Err(StoreError::invalid(TASK_CREATE_REQUEST_ID_REQUIRED));
    }
    if !matches!(request.profile, TaskProfile::Manager | TaskProfile::Worker) {
      return Err(StoreError::invalid("profile must be 'manager' or 'worker'"));
    }
    let task_id = request
      .session_id
      .clone()
      .unwrap_or_else(|| control_events::stable_task_id(request.task_parent_id.as_deref(), &request.request_id));
    let guard = self.control_lock().await;
    self.require_control_guard(&guard)?;

    if let Some(existing) = self.read_metadata_fresh(&task_id)? {
      if matches!(caller, TaskCaller::Agent { .. }) {
        let replay = CreateTask { profile: existing.profile, task: existing.task.clone(), ..request };
        self.runtime_gate(&replay, &caller)?;
      }
      return Ok(existing);
    }
    self.runtime_gate(&request, &caller)?;

    let parent_meta = match request.task_parent_id.as_deref() {
      None => None,
      Some(parent_id) => {
        let parent = self
          .read_metadata_fresh(parent_id)?
          .ok_or_else(|| StoreError::not_found(format!("task {parent_id} not found")))?;
        if parent.profile == TaskProfile::Worker {
          return Err(StoreError::invalid(format!("parent task {parent_id} is not a manager")));
        }
        let parent_state = self.task_state(parent_id)?;
        if parent_state != "open" {
          return Err(StoreError::conflict(vec![format!("parent task {parent_id} is {parent_state}")]));
        }
        self.require_open_ancestry(parent_id)?;
        Some(parent)
      }
    };

    let mut metadata = SessionMetadata::new(task_id.clone(), "".to_string(), request.profile);
    metadata.name = match request.name.as_deref().filter(|name| !name.is_empty()) {
      Some(name) => name.to_string(),
      None => match request.task.as_ref().and_then(|task| default_task_name(task, request.profile)) {
        Some(name) => name,
        None => self.next_session_name()?,
      },
    };
    metadata.task_parent_id = request.task_parent_id.clone();
    metadata.task = request.task.clone();
    metadata.group = request.group.clone();
    metadata.backend = request
      .backend
      .as_deref()
      .filter(|backend| !backend.is_empty())
      .or_else(|| parent_meta.as_ref().map(|parent| parent.backend.as_str()).filter(|backend| !backend.is_empty()))
      .map(str::to_string)
      .or_else(|| self.cfg.backends.options.first().map(|backend| backend.id.clone()))
      .ok_or_else(|| StoreError::new("task create requires a configured backend"))?;
    set_registered_slot_values(&self.slots, &mut metadata, slot_values)?;

    let created_event_id = uuid::Uuid::new_v4().to_string();
    let created_event = control_events::build_task_created_event(
      caller_actor(&caller),
      &task_id,
      &request.request_id,
      request.task_parent_id.as_deref(),
      request.task.as_ref().map(task_spec_hash).transpose()?.as_deref(),
      Some(&created_event_id),
      None,
    );
    metadata.created_by_event = Some(EventRef { session_id: task_id.clone(), event_id: Some(created_event_id) });

    self.publish_task_node(&metadata, &created_event)?;
    self.invalidate_cache(&task_id);
    self.invalidate_tree_index();
    let fresh = self
      .read_metadata_fresh(&task_id)?
      .ok_or_else(|| StoreError::new(format!("created task {task_id} is missing")))?;
    // The creation fact is published by the rename above, which appends no control fact. The signal follows the
    // publication and the index invalidation, so a receiver can read the node at once (`task_sessions.create_task`).
    self.notify_task_tree_changed(&task_id, Some(event_types::TASK_CREATED)).await;
    Ok(fresh)
  }

  /// Change registered slot values while the caller holds the tree control lock.
  pub async fn update_task_slot_fields_locked(
    &self,
    guard: &ControlGuard,
    session_id: &str,
    owner: &str,
    values: Map<String, Value>,
  ) -> StoreResult<SessionMetadata> {
    let metadata = self
      .update_slot_fields_locked(guard, session_id, owner, values)
      .await?
      .ok_or_else(|| StoreError::not_found(format!("session {session_id} not found")))?;
    self.invalidate_tree_index();
    Ok(metadata)
  }

  /// Change a node’s backend under the tree control lock without changing `updated_at`.
  pub async fn set_node_backend_locked(
    &self,
    guard: &ControlGuard,
    session_id: &str,
    backend: &str,
  ) -> StoreResult<()> {
    self.require_control_guard(guard)?;
    let backend = backend.to_string();
    let updated = self
      .mutate_fresh(session_id, false, true, move |meta| {
        meta.backend = backend;
        Ok(true)
      })
      .await?;
    if updated.is_none() {
      return Err(StoreError::not_found(format!("session {session_id} not found")));
    }
    self.invalidate_tree_index();
    Ok(())
  }

  /// Copy registered metadata fields from `old` into a fresh target metadata read.
  pub async fn adopt_metadata_slot_locked(
    &self,
    guard: &ControlGuard,
    session_id: &str,
    old: &SessionMetadata,
    owner: &str,
    fields: Option<&[&str]>,
  ) -> StoreResult<SessionMetadata> {
    self.require_control_guard(guard)?;
    let slot = self
      .slots
      .iter()
      .find(|slot| slot.owner == owner && slot.on == charliebot_core::metadata_slot::MetadataFile::Session)
      .ok_or_else(|| StoreError::new(format!("{owner:?} registered no fields on session metadata")))?;
    let selected: Vec<_> = match fields {
      Some(names) => names
        .iter()
        .map(|name| {
          slot
            .fields
            .iter()
            .find(|field| field.name == *name)
            .ok_or_else(|| StoreError::new(format!("{owner:?} has no field {name:?}")))
        })
        .collect::<StoreResult<Vec<_>>>()?,
      None => slot.fields.iter().collect(),
    };
    let old = old.clone();
    let mut copied = Vec::new();
    for field in selected {
      let value = old.extra.get(field.name).unwrap_or(&field.default).clone();
      copied.push((field.name, value));
    }
    let result = self
      .mutate_fresh(session_id, false, true, move |meta| {
        for (name, value) in copied {
          meta.set_extra(name, value);
        }
        meta.updated_at = UtcDateTime::now();
        Ok(true)
      })
      .await?
      .ok_or_else(|| StoreError::not_found(format!("session {session_id} not found")))?;
    self.invalidate_tree_index();
    Ok(result)
  }

  /// Persist the native anchor provenance under the tree control lock.
  pub async fn record_native_anchor(
    &self,
    session_id: &str,
    prompt_hash: &str,
    backend: &str,
    model: Option<&str>,
  ) -> StoreResult<()> {
    self.record_native_anchor_with_reset(session_id, prompt_hash, backend, model, false).await
  }

  /// Persist native anchor provenance and optionally clear a superseded native conversation.
  pub async fn record_native_anchor_with_reset(
    &self,
    session_id: &str,
    prompt_hash: &str,
    backend: &str,
    model: Option<&str>,
    reset_anchor: bool,
  ) -> StoreResult<()> {
    let _guard = self.control_lock().await;
    self.persist_native_anchor_provenance(session_id, prompt_hash, backend, model).await?;
    if reset_anchor {
      let fresh = self
        .read_metadata_fresh(session_id)?
        .ok_or_else(|| StoreError::new(format!("session {session_id} not found")))?;
      if fresh.cc_session_id.is_some() {
        self.clear_cc_session_anchor(session_id).await?;
      }
    }
    self.invalidate_tree_index();
    Ok(())
  }

  /// Store a prompt body once by its SHA-256 reference.
  pub fn store_prompt_body(&self, body: &str) -> StoreResult<String> {
    let digest = control_events::sha256_hex(body);
    let path = self.prompt_bodies_dir().join(format!("{digest}.md"));
    if path.exists() {
      let existing = fs::read_to_string(&path).map_err(|error| path_error(&path, error))?;
      if existing != body {
        return Err(StoreError::new(format!(
          "prompt body store corrupted at {}: fingerprint content mismatch",
          path.display()
        )));
      }
      return Ok(digest);
    }
    let parent = path.parent().expect("prompt body path has a parent");
    fs::create_dir_all(parent).map_err(|error| path_error(parent, error))?;
    fsutil::atomic_write_text(&path, body).map_err(|error| path_error(&path, error))?;
    Ok(digest)
  }

  /// Reconcile the prompt-change fact implied by one metadata snapshot.
  pub async fn ensure_prompt_changed_fact(
    &self,
    session_id: &str,
    meta: &SessionMetadata,
    scope: PromptScope,
  ) -> StoreResult<()> {
    let guard = self.control_lock().await;
    self.ensure_prompt_changed_fact_locked(&guard, session_id, meta, scope).await
  }

  /// Reconcile a prompt-change fact while the caller owns the control lock.
  pub async fn ensure_prompt_changed_fact_locked(
    &self,
    guard: &ControlGuard,
    session_id: &str,
    meta: &SessionMetadata,
    scope: PromptScope,
  ) -> StoreResult<()> {
    self.require_control_guard(guard)?;
    let events = self.load_chat_events_sync(session_id)?;
    let scope_name = scope.as_str();
    let last = events.iter().rev().find(|event| {
      event.get("type").and_then(Value::as_str) == Some("prompt_changed")
        && event.get("scope").and_then(Value::as_str) == Some(scope_name)
    });
    let current_ref = scope.metadata_ref(meta);
    if last.is_some_and(|event| event.get("new_ref").and_then(Value::as_str) == current_ref) {
      return Ok(());
    }
    if last.is_none() && current_ref.is_none() {
      return Ok(());
    }
    let previous_ref = last.and_then(|event| event.get("new_ref")).cloned().unwrap_or(Value::Null);
    let event = control_events::build_control_event(
      "prompt_changed",
      ACTOR_USER,
      session_id,
      None,
      None,
      [
        ("scope".to_string(), Value::String(scope_name.to_string())),
        ("previous_ref".to_string(), previous_ref),
        ("new_ref".to_string(), current_ref.map_or(Value::Null, |value| Value::String(value.to_string()))),
      ],
    );
    self.append_control_fact(guard, session_id, event).await?;
    Ok(())
  }

  /// Archive an unsuccessful worker after rechecking its durable blockers under the control lock.
  pub async fn archive_unsuccessful_worker(
    &self,
    guard: &ControlGuard,
    session_id: &str,
    run_id: &str,
    outcome: &str,
  ) -> StoreResult<bool> {
    self.require_control_guard(guard)?;
    let index = self.tree_index()?;
    let meta =
      index.metas.get(session_id).ok_or_else(|| StoreError::not_found(format!("task {session_id} not found")))?;
    if meta.profile != TaskProfile::Worker || self.task_state(session_id)? != "open" {
      return Ok(false);
    }
    if self.last_ended_run_id(session_id)?.as_deref() != Some(run_id) {
      return Ok(false);
    }
    if !self.run_blockers(session_id)?.is_empty() {
      return Ok(false);
    }
    for descendant in descendant_ids(&index, session_id)? {
      if self.task_state(&descendant)? == "open" {
        return Ok(false);
      }
    }
    if self.has_pending_trigger(session_id)? || self.has_pending_input(session_id)? {
      return Ok(false);
    }
    let request_id = format!("unsuccessful:{run_id}");
    let facts = self.load_all_facts(session_id)?;
    if facts.iter().any(|event| {
      event.get("type").and_then(Value::as_str) == Some("task_closed")
        && event.get("request_id").and_then(Value::as_str) == Some(request_id.as_str())
    }) {
      return Ok(false);
    }
    let summary = format!("run {run_id} ended {outcome}; archived by the server");
    let event = archived_close_event(session_id, &request_id, ACTOR_SYSTEM, &summary, &[run_id], None);
    self.append_control_fact(guard, session_id, event).await?;
    self.invalidate_tree_index();
    self.sequence_archive.on_archive(session_id).await?;
    Ok(true)
  }

  /// Archive a task subtree as one end state: one archived close fact per open node, parent before child.
  ///
  /// Operator scope only. Under the control lock, every unfinished run in the subtree is collected before any
  /// fact is written, so a refusal writes nothing. An already archived target returns an empty list. Each
  /// archived node's sequence binding then runs its archive duty, in the same order.
  pub async fn archive_subtree(&self, session_id: &str, caller: TaskCaller) -> StoreResult<Vec<String>> {
    if caller != TaskCaller::Operator {
      return Err(StoreError::forbidden("archiving a task requires operator credentials"));
    }
    let guard = self.control_lock().await;
    // Rebuild from disk under the lock: the cache can predate writes that another process made.
    self.invalidate_tree_index();
    let index = self.tree_index()?;
    if !index.metas.contains_key(session_id) {
      return Err(StoreError::not_found(format!("task {session_id} not found")));
    }
    if self.task_state(session_id)? != "open" {
      return Ok(Vec::new());
    }
    let mut subtree = vec![session_id.to_string()];
    subtree.extend(descendant_ids(&index, session_id)?);
    let mut blocked = Vec::new();
    for node in &subtree {
      for blocker in self.run_blockers(node)? {
        blocked.push(format!("{node}: {}", blocker.reason));
      }
    }
    if !blocked.is_empty() {
      blocked.sort();
      return Err(StoreError::conflict(blocked));
    }
    let request_id = format!("archive-{}", uuid::Uuid::new_v4());
    let mut archived = Vec::new();
    for node in &subtree {
      if self.task_state(node)? != "open" {
        continue;
      }
      let (summary, archived_with) =
        if node == session_id { ("archived by the user", None) } else { ("", Some(session_id)) };
      let event = archived_close_event(node, &request_id, ACTOR_USER, summary, &[], archived_with);
      self.append_control_fact(&guard, node, event).await?;
      archived.push(node.clone());
    }
    self.invalidate_tree_index();
    for node in &archived {
      self.sequence_archive.on_archive(node).await?;
    }
    Ok(archived)
  }

  /// Permanently delete an empty, unreferenced task while holding the tree control lock.
  pub async fn delete_permanently(&self, session_id: &str, caller: TaskCaller) -> StoreResult<bool> {
    if caller != TaskCaller::Operator {
      return Err(StoreError::forbidden("permanent delete requires operator credentials"));
    }
    let _guard = self.control_lock().await;
    self.invalidate_tree_index();
    let index = self.tree_index()?;
    if !index.metas.contains_key(session_id) {
      return Err(StoreError::not_found(format!("task {session_id} not found")));
    }

    let mut blockers = Vec::new();
    let children = index.children.get(&Some(session_id.to_string())).cloned().unwrap_or_default();
    if !children.is_empty() {
      blockers.push(format!("has child task(s): {}", children.join(", ")));
    }
    let runs = self.list_run_records_sync(session_id)?;
    if !runs.is_empty() {
      blockers
        .push(format!("has run record(s): {}", runs.iter().map(|run| run.id.as_str()).collect::<Vec<_>>().join(", ")));
    }
    let triggers_dir = self.session_dir(session_id).join("triggers");
    if triggers_dir.is_dir() {
      let mut has_trigger = false;
      for entry in fs::read_dir(&triggers_dir).map_err(|error| path_error(&triggers_dir, error))? {
        let entry = entry.map_err(|error| path_error(&triggers_dir, error))?;
        if entry.file_name().to_string_lossy().ends_with(".json") {
          has_trigger = true;
          break;
        }
      }
      if has_trigger {
        blockers.push("has saved trigger reference(s)".to_string());
      }
    }

    let facts = self.facts_of(session_id)?;
    let mut substance = facts
      .events_by_id
      .values()
      .filter(|event| event.get("type").and_then(Value::as_str) != Some(event_types::TASK_CREATED))
      .map(|event| event.get("type").and_then(Value::as_str).unwrap_or("None").to_string())
      .collect::<Vec<_>>();
    substance.sort();
    substance.dedup();
    if !substance.is_empty() {
      blockers.push(format!("has preserved conversation/evidence: {}", substance.join(", ")));
    }

    for (other_id, other) in &index.metas {
      if other_id == session_id {
        continue;
      }
      let mut refs = Vec::new();
      if other.parent_session_id.as_deref() == Some(session_id) {
        refs.push("parent_session_id");
      }
      if other.successor_session_id.as_deref() == Some(session_id) {
        refs.push("successor_session_id");
      }
      if other.origin_ref.as_ref().is_some_and(|reference| reference.session_id == session_id) {
        refs.push("origin_ref");
      }
      if other.created_by_event.as_ref().is_some_and(|reference| reference.session_id == session_id) {
        refs.push("created_by_event");
      }
      if !refs.is_empty() {
        blockers.push(format!("referenced by task {other_id} ({})", refs.join(", ")));
        continue;
      }
      let other_facts = self.facts_of(other_id)?;
      if other_facts.delivered_reports.iter().any(|(child_id, _)| child_id == session_id) {
        blockers.push(format!("referenced by a child report in task {other_id}"));
      }
    }

    blockers.sort();
    blockers.dedup();
    if !blockers.is_empty() {
      return Err(StoreError::conflict(blockers));
    }
    let deleted = self.delete_session_permanently(session_id).await?;
    if deleted {
      self.invalidate_tree_index();
    }
    Ok(deleted)
  }

  /// Apply one operator PATCH of a task node with its structural guards (`TaskTreeManager.patch_task`).
  ///
  /// Operator scope only. Under the control lock the guards of every node that the change moves or restructures are
  /// collected before any write, so a refusal writes nothing. The field write is a fresh read-modify-write, so a
  /// rename that landed meanwhile is kept. Each prompt scope then swaps its reference and lands its fact.
  pub async fn patch_task(
    &self,
    session_id: &str,
    patch: PatchTask,
    caller: TaskCaller,
    guards: &dyn StructuralBlockers,
  ) -> StoreResult<SessionMetadata> {
    if caller != TaskCaller::Operator {
      return Err(StoreError::forbidden("task metadata mutations require operator credentials"));
    }
    let guard = self.control_lock().await;
    // Rebuild from disk under the lock: the cache can predate writes that another process made.
    self.invalidate_tree_index();
    let index = self.tree_index()?;
    let meta = self.load_task_meta(session_id).await?;
    let PatchTask { name, task_parent_id, profile, task, subtree_prompt, node_prompt } = patch;
    let prompt_changed = subtree_prompt.is_some() || node_prompt.is_some();
    let structural = task.is_some() || profile.is_some() || task_parent_id.is_some();
    let mut blockers = if structural { self.structural_blockers(session_id, guards)? } else { Vec::new() };
    let children = index.children.get(&Some(session_id.to_string())).map_or(0, Vec::len);
    if profile == Some(Some(TaskProfile::Worker)) && meta.profile != TaskProfile::Worker && children > 0 {
      blockers.push("demotion to worker requires a task with no child tasks".to_string());
    }
    if let Some(parent) = task_parent_id.as_ref().filter(|parent| parent.as_deref() != meta.task_parent_id.as_deref()) {
      blockers.extend(self.reparent_blockers(&index, session_id, parent.as_deref(), guards)?);
    }
    if !blockers.is_empty() {
      blockers.sort();
      blockers.dedup();
      return Err(StoreError::conflict(blockers));
    }

    self
      .mutate_fresh(session_id, false, true, move |meta| {
        if let Some(name) = name {
          meta.name = name;
        }
        if let Some(Some(profile)) = profile {
          meta.profile = profile;
        }
        if let Some(task) = task {
          meta.task = task;
        }
        if let Some(parent) = task_parent_id {
          meta.task_parent_id = parent;
        }
        meta.schema_version = 2;
        meta.updated_at = UtcDateTime::now();
        Ok(true)
      })
      .await?
      .ok_or_else(|| StoreError::not_found(format!("task {session_id} not found")))?;
    self.invalidate_tree_index();

    if let Some(body) = subtree_prompt {
      self.apply_prompt_change(&guard, session_id, PromptScope::Subtree, body.as_deref()).await?;
    }
    if let Some(body) = node_prompt {
      self.apply_prompt_change(&guard, session_id, PromptScope::Node, body.as_deref()).await?;
    }
    self.invalidate_tree_index();
    let event_type = if prompt_changed { event_types::PROMPT_CHANGED } else { "task_updated" };
    self.notify_task_tree_changed(session_id, Some(event_type)).await;
    self.read_metadata_fresh(session_id)?.ok_or_else(|| StoreError::not_found(format!("task {session_id} not found")))
  }

  /// Swap one prompt scope's reference and land its fact. An interrupted earlier edit (metadata ahead of its fact) is
  /// landed first, so a retry or a later edit never folds two transitions into one fact. Bodies are content-addressed,
  /// so a retried PATCH stores the same bytes under the same reference.
  async fn apply_prompt_change(
    &self,
    guard: &ControlGuard,
    session_id: &str,
    scope: PromptScope,
    body: Option<&str>,
  ) -> StoreResult<()> {
    let current = self.load_task_meta(session_id).await?;
    self.ensure_prompt_changed_fact_locked(guard, session_id, &current, scope).await?;
    let new_ref = body.map(|text| self.store_prompt_body(text)).transpose()?;
    if new_ref.as_deref() == scope.metadata_ref(&current) {
      return Ok(());
    }
    let fresh = self
      .mutate_fresh(session_id, false, true, move |meta| {
        scope.set_metadata_ref(meta, new_ref);
        meta.updated_at = UtcDateTime::now();
        Ok(true)
      })
      .await?
      .ok_or_else(|| StoreError::not_found(format!("task {session_id} not found")))?;
    self.invalidate_tree_index();
    self.ensure_prompt_changed_fact_locked(guard, session_id, &fresh, scope).await
  }

  /// The structural blockers of one node: its unprocessed task inputs, then its unfinished runs.
  fn structural_blockers(&self, session_id: &str, guards: &dyn StructuralBlockers) -> StoreResult<Vec<String>> {
    let mut blockers = guards.pending_input_blockers(session_id)?;
    blockers.extend(self.run_blockers(session_id)?.into_iter().map(|blocker| blocker.reason));
    Ok(blockers)
  }

  /// The refusals for moving a node under `new_parent`: the target is an open manager, the move does not enter the
  /// node's own subtree, and the target's ancestors are open. Every node in the moving subtree must also be idle, with
  /// no undelivered close report.
  fn reparent_blockers(
    &self,
    index: &TreeIndex,
    session_id: &str,
    new_parent: Option<&str>,
    guards: &dyn StructuralBlockers,
  ) -> StoreResult<Vec<String>> {
    let mut blockers = Vec::new();
    if let Some(parent_id) = new_parent {
      let target = index_meta(index, parent_id)?;
      if target.profile != TaskProfile::Manager {
        blockers.push(format!("reparent target {parent_id} is not a manager task"));
      }
      let target_state = self.task_state(parent_id)?;
      if target_state != "open" {
        blockers.push(format!("reparent target {parent_id} is {target_state}"));
      }
      let chain = ancestor_ids(index, parent_id)?;
      if session_id == parent_id || chain.iter().any(|id| id == session_id) {
        blockers.push(format!("reparent target {parent_id} is inside {session_id}'s own subtree"));
      } else {
        let mut closed = Vec::new();
        for ancestor in &chain {
          if self.task_state(ancestor)? != "open" {
            closed.push(ancestor.clone());
          }
        }
        if !closed.is_empty() {
          blockers.push(closed_ancestors_blocker(&closed));
        }
      }
    }
    let subtree = std::iter::once(session_id.to_string()).chain(descendant_ids(index, session_id)?);
    for node in subtree {
      for blocker in self.structural_blockers(&node, guards)? {
        blockers.push(format!("{node}: {blocker}"));
      }
      for blocker in guards.undelivered_report_blockers(&node)? {
        blockers.push(format!("{node}: {blocker}"));
      }
    }
    Ok(blockers)
  }

  /// Move sequence bindings from an elone source and copy its scheduled-run bookkeeping.
  pub async fn rebind_elone_bindings_locked(
    &self,
    guard: &ControlGuard,
    source_session_id: &str,
    new_session_id: &str,
  ) -> StoreResult<bool> {
    self.require_control_guard(guard)?;
    let moved = self.sequence_binding_mover.move_bindings(source_session_id, new_session_id).await?;
    if !moved {
      return Ok(false);
    }
    let source = self
      .read_metadata_fresh(source_session_id)?
      .ok_or_else(|| StoreError::not_found(format!("session {source_session_id} not found")))?;
    let slot = self
      .slots
      .iter()
      .find(|slot| slot.owner == "cron" && slot.on == charliebot_core::metadata_slot::MetadataFile::Session)
      .ok_or_else(|| StoreError::new("cron registered no fields on session metadata"))?;
    let names = ["last_scheduled_run", "last_scheduled_cron", "last_run_status"];
    let selected = names
      .iter()
      .map(|name| {
        let field = slot
          .fields
          .iter()
          .find(|field| field.name == *name)
          .ok_or_else(|| StoreError::new(format!("cron has no field {name:?}")))?;
        Ok((field.name, source.extra.get(field.name).unwrap_or(&field.default).clone()))
      })
      .collect::<StoreResult<Vec<_>>>()?;
    let updated = self
      .mutate_fresh(new_session_id, false, true, move |meta| {
        for (name, value) in selected {
          meta.set_extra(name, value);
        }
        meta.updated_at = UtcDateTime::now();
        Ok(true)
      })
      .await?;
    if updated.is_none() {
      return Err(StoreError::not_found(format!("session {new_session_id} not found")));
    }
    self.invalidate_tree_index();
    Ok(true)
  }

  /// Resolve every existing metadata row from a session through its successor chain.
  pub fn resolve_successor_chain(&self, session_id: &str) -> StoreResult<Vec<SessionMetadata>> {
    let mut chain = Vec::new();
    let Some(mut current) = self.read_metadata_fresh(session_id)? else { return Ok(chain) };
    chain.push(current.clone());
    for _ in 0..SUCCESSOR_HOP_LIMIT {
      let Some(successor_id) = current.successor_session_id.as_deref() else { return Ok(chain) };
      let Some(successor) = self.read_metadata_fresh(successor_id)? else {
        tracing::error!(session_id, missing_session_id = successor_id, "successor_chain_broken");
        return Ok(chain);
      };
      chain.push(successor.clone());
      current = successor;
    }
    if current.successor_session_id.is_some() {
      return Err(StoreError::new(format!(
        "successor chain from {session_id} exceeds {SUCCESSOR_HOP_LIMIT} hops; aborting to avoid a cycle"
      )));
    }
    Ok(chain)
  }

  /// Append an event to the current successor-chain tail while holding that session’s lock.
  pub async fn deliver_to_successor(&self, session_id: &str, mut event: Value) -> StoreResult<Option<String>> {
    for _ in 0..=SUCCESSOR_HOP_LIMIT {
      let Some(tail) = self.resolve_successor_chain(session_id)?.last().cloned() else {
        tracing::error!(session_id, "deliver_to_successor_origin_missing");
        return Ok(None);
      };
      let _session_guard = self.lock_for(&tail.id).await;
      let Some(fresh_tail) = self.read_metadata_fresh(&tail.id)? else {
        tracing::error!(session_id, tail_id = %tail.id, "deliver_to_successor_tail_missing");
        return Ok(None);
      };
      if fresh_tail.successor_session_id.is_some() {
        continue;
      }
      if !self.metadata_path(&fresh_tail.id).is_file() {
        tracing::error!(session_id, tail_id = %fresh_tail.id, "deliver_to_successor_metadata_missing");
        return Ok(None);
      }
      if fresh_tail.id != session_id {
        event
          .as_object_mut()
          .ok_or_else(|| StoreError::new("successor event must be a JSON object"))?
          .insert("origin_session_id".to_string(), Value::String(session_id.to_string()));
      }
      self.append_chat_event(&fresh_tail.id, event).await?;
      return Ok(Some(fresh_tail.id));
    }
    Err(StoreError::new(format!(
      "deliver_to_successor from {session_id} retried {SUCCESSOR_HOP_LIMIT} times; aborting to avoid a loop"
    )))
  }

  /// Derive live and waiting work for one task node.
  pub fn task_tree_activity(&self, session_id: &str) -> StoreResult<TaskTreeActivity> {
    let task_open = self.task_state(session_id)? == "open";
    let mut running = false;
    let mut waiting = false;
    for run in self.list_run_records_sync(session_id)? {
      match self.run_display_state(session_id, &run.id)? {
        RunDisplayState::Running => running = true,
        RunDisplayState::Queued | RunDisplayState::Withheld if task_open => waiting = true,
        RunDisplayState::Queued
        | RunDisplayState::Withheld
        | RunDisplayState::Attention
        | RunDisplayState::Stopped
        | RunDisplayState::Success
        | RunDisplayState::Failed
        | RunDisplayState::Interrupted => {}
      }
    }
    Ok(TaskTreeActivity {
      has_running_tasks: running,
      work_state: if running {
        "running"
      } else if waiting {
        "waiting"
      } else {
        "idle"
      }
      .to_string(),
    })
  }

  /// Build the session detail projection used by the session read and completion routes.
  pub fn session_detail(&self, session_id: &str) -> StoreResult<Value> {
    let index = self.tree_index()?;
    let meta =
      index.metas.get(session_id).ok_or_else(|| StoreError::not_found(format!("task {session_id} not found")))?;
    let mut detail = serde_json::to_value(meta)?;
    let object = detail.as_object_mut().expect("session metadata serializes to an object");
    object.insert("task_state".to_string(), Value::String(self.task_state(session_id)?));
    object.insert("work_state".to_string(), Value::String(self.task_tree_activity(session_id)?.work_state));
    object.insert("archived".to_string(), Value::Bool(archived_of(self, &index, session_id)?));

    let mut ancestors = Vec::new();
    for ancestor_id in ancestor_ids(&index, session_id)? {
      let ancestor =
        index.metas.get(&ancestor_id).ok_or_else(|| StoreError::not_found(format!("task {ancestor_id} not found")))?;
      ancestors.push(json!({"id": ancestor.id, "name": ancestor.name}));
    }
    object.insert("ancestors".to_string(), Value::Array(ancestors));

    let descendants = descendant_ids(&index, session_id)?.len();
    let prompt_rules = json!({
      "subtree": self.prompt_rule_summary(meta.subtree_prompt_ref.as_deref())?,
      "node": self.prompt_rule_summary(meta.node_prompt_ref.as_deref())?,
      "affected_descendants": descendants,
    });
    object.insert("prompt_rules".to_string(), prompt_rules);
    Ok(detail)
  }

  fn prompt_rule_summary(&self, prompt_ref: Option<&str>) -> StoreResult<Value> {
    let Some(prompt_ref) = prompt_ref else {
      return Ok(json!({"ref": null, "source": null, "chars": 0, "text": null}));
    };
    let path = self.prompt_bodies_dir().join(format!("{prompt_ref}.md"));
    let body = fs::read_to_string(&path).map_err(|error| path_error(&path, error))?;
    Ok(json!({
      "ref": prompt_ref,
      "source": path.to_string_lossy(),
      "chars": body.chars().count(),
      "text": body,
    }))
  }

  fn tree_index(&self) -> StoreResult<TreeIndex> {
    let generation = self.task_tree_generation.load(Ordering::Acquire);
    if let Some(cached) = self.task_tree_index.lock().expect("task tree index mutex is not poisoned").as_ref()
      && cached.generation == generation
      && cached.built_at.elapsed() < TREE_INDEX_TTL
    {
      return Ok(cached.index.clone());
    }
    let index = build_tree_index(self)?;
    if self.task_tree_generation.load(Ordering::Acquire) == generation {
      *self.task_tree_index.lock().expect("task tree index mutex is not poisoned") =
        Some(CachedTreeIndex { generation, built_at: Instant::now(), index: index.clone() });
    }
    Ok(index)
  }

  fn publish_task_node(&self, meta: &SessionMetadata, event: &Value) -> StoreResult<()> {
    let sessions_dir = self.cfg.sessions_dir();
    fs::create_dir_all(&sessions_dir).map_err(|error| path_error(&sessions_dir, error))?;
    let final_dir = sessions_dir.join(&meta.id);
    let temp_dir = sessions_dir.join(format!(".task-{}-{}.tmp", meta.id, uuid::Uuid::new_v4().simple()));
    fs::create_dir(&temp_dir).map_err(|error| path_error(&temp_dir, error))?;
    let operation = (|| {
      let data_dir = temp_dir.join("data");
      fs::create_dir(&data_dir).map_err(|error| path_error(&data_dir, error))?;
      let events_path = data_dir.join("chat_events.jsonl");
      #[cfg(any(test, feature = "write-faults"))]
      self.fail_chat_append(&meta.id, event, &events_path)?;
      crate::ndjson::append(&events_path, event)?;
      let metadata_path = temp_dir.join("metadata.json");
      fsutil::atomic_write_text(&metadata_path, &meta.to_json_string(&self.slots, true))
        .map_err(|error| path_error(&metadata_path, error))?;
      match fs::rename(&temp_dir, &final_dir) {
        Ok(()) => Ok(()),
        Err(rename_error) => {
          if let Some(existing) = self.read_metadata_path(&final_dir.join("metadata.json"))? {
            return if existing.id == meta.id {
              Ok(())
            } else {
              Err(StoreError::new(format!("task create target {} contains another id", final_dir.display())))
            };
          }
          Err(path_error(&final_dir, rename_error))
        }
      }
    })();
    if temp_dir.exists() {
      fs::remove_dir_all(&temp_dir).map_err(|error| path_error(&temp_dir, error))?;
    }
    operation
  }

  /// Name the next session from `sessions/.counter`. An unreadable or unparsable counter takes the
  /// session-directory count, as Python's `_next_session_name` does, and the counter is rewritten after it.
  fn next_session_name(&self) -> StoreResult<String> {
    let sessions_dir = self.cfg.sessions_dir();
    fs::create_dir_all(&sessions_dir).map_err(|error| path_error(&sessions_dir, error))?;
    let counter = sessions_dir.join(".counter");
    let number = match fs::read_to_string(&counter) {
      Ok(value) => match value.trim().parse::<i64>() {
        Ok(number) => number,
        Err(error) => {
          tracing::warn!(path = %counter.display(), %error, "session_counter_unparsable");
          count_session_dirs(&sessions_dir)?
        }
      },
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => count_session_dirs(&sessions_dir)?,
      Err(error) => {
        tracing::warn!(path = %counter.display(), %error, "session_counter_unreadable");
        count_session_dirs(&sessions_dir)?
      }
    };
    fs::write(&counter, (number + 1).to_string()).map_err(|error| path_error(&counter, error))?;
    Ok(format!("Session {number}"))
  }

  fn has_pending_trigger(&self, session_id: &str) -> StoreResult<bool> {
    let triggers_dir = self.session_dir(session_id).join("triggers");
    let entries = match fs::read_dir(&triggers_dir) {
      Ok(entries) => entries,
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
      Err(error) => return Err(path_error(&triggers_dir, error)),
    };
    for entry in entries {
      let entry = entry.map_err(|error| path_error(&triggers_dir, error))?;
      if !entry.file_type().map_err(|error| path_error(&entry.path(), error))?.is_file()
        || entry.path().extension().is_none_or(|extension| extension != "json")
      {
        continue;
      }
      let raw = fs::read_to_string(entry.path()).map_err(|error| path_error(&entry.path(), error))?;
      let value: Value = serde_json::from_str(&raw)
        .map_err(|error| StoreError::new(format!("{}: invalid trigger metadata: {error}", entry.path().display())))?;
      if value.get("status").and_then(Value::as_str) == Some("pending") {
        return Ok(true);
      }
    }
    Ok(false)
  }

  fn has_pending_input(&self, session_id: &str) -> StoreResult<bool> {
    let facts = self.facts_of(session_id)?;
    if facts.input_candidates.is_empty() {
      return Ok(false);
    }
    let events = self.load_all_facts(session_id)?;
    let records = self.list_run_records_sync(session_id)?;
    let confirmed = facts.confirmed_input_ids;
    let mut claimed = HashSet::new();
    for run in records {
      let stopped_before_launch = run.pid.is_none() && stop_requested_in_events(&events, &run.id, None);
      if stopped_before_launch {
        continue;
      }
      claimed.extend(run.input_event_ids);
    }
    for candidate in facts.input_candidates {
      let input_id = candidate.get("id").and_then(Value::as_str).unwrap_or_default();
      if !confirmed.contains(input_id) && !claimed.contains(input_id) {
        return Ok(true);
      }
    }
    Ok(false)
  }
}

fn build_tree_index(store: &Store) -> StoreResult<TreeIndex> {
  let sessions_dir = store.cfg.sessions_dir();
  let entries = match fs::read_dir(&sessions_dir) {
    Ok(entries) => entries,
    Err(error) => return Err(path_error(&sessions_dir, error)),
  };
  let mut names = Vec::new();
  for entry in entries {
    let entry = entry.map_err(|error| path_error(&sessions_dir, error))?;
    if !entry.file_type().map_err(|error| path_error(&entry.path(), error))?.is_dir() {
      continue;
    }
    let name = entry.file_name().to_string_lossy().into_owned();
    if name.starts_with(".task-") && name.ends_with(".tmp") {
      continue;
    }
    names.push(name);
  }
  names.sort();
  let mut metas = HashMap::new();
  for name in names {
    if let Some(meta) = store.read_metadata_fresh(&name)? {
      metas.insert(name, meta);
    }
  }
  let mut children: HashMap<Option<String>, Vec<String>> = HashMap::new();
  for (session_id, meta) in &metas {
    children.entry(meta.task_parent_id.clone()).or_default().push(session_id.clone());
  }
  for child_ids in children.values_mut() {
    child_ids.sort_by(|left, right| {
      let left_meta = &metas[left];
      let right_meta = &metas[right];
      left_meta.created_at.cmp(&right_meta.created_at).then_with(|| left.cmp(right))
    });
  }
  Ok(TreeIndex { metas, children })
}

fn index_meta<'a>(index: &'a TreeIndex, session_id: &str) -> StoreResult<&'a SessionMetadata> {
  index.metas.get(session_id).ok_or_else(|| StoreError::not_found(format!("task {session_id} not found")))
}

/// The 409 blocker sentence naming a node's closed ancestor tasks.
fn closed_ancestors_blocker(closed: &[String]) -> String {
  format!("closed ancestor task(s): {}", closed.join(", "))
}

fn ancestor_ids(index: &TreeIndex, session_id: &str) -> StoreResult<Vec<String>> {
  let mut chain = Vec::new();
  let mut seen = HashSet::from([session_id.to_string()]);
  let mut current =
    index.metas.get(session_id).ok_or_else(|| StoreError::not_found(format!("task {session_id} not found")))?;
  while let Some(parent_id) = current.task_parent_id.as_deref() {
    if !seen.insert(parent_id.to_string()) {
      return Err(StoreError::conflict(vec![format!("task relation cycle through {parent_id}")]));
    }
    current = index.metas.get(parent_id).ok_or_else(|| StoreError::not_found(format!("task {parent_id} not found")))?;
    chain.push(parent_id.to_string());
    if chain.len() > TASK_ANCESTOR_HOP_LIMIT {
      return Err(StoreError::conflict(vec![format!(
        "ancestor chain of {session_id} exceeds {TASK_ANCESTOR_HOP_LIMIT} hops"
      )]));
    }
  }
  Ok(chain)
}

fn descendant_ids(index: &TreeIndex, session_id: &str) -> StoreResult<Vec<String>> {
  if !index.metas.contains_key(session_id) {
    return Err(StoreError::not_found(format!("task {session_id} not found")));
  }
  let children_of = |parent: &str| index.children.get(&Some(parent.to_string())).cloned().unwrap_or_default();
  let mut output = Vec::new();
  let mut stack = children_of(session_id);
  let mut seen = HashSet::from([session_id.to_string()]);
  while let Some(next) = stack.pop() {
    if !seen.insert(next.clone()) {
      return Err(StoreError::conflict(vec![format!("task relation cycle through {next}")]));
    }
    output.push(next.clone());
    stack.extend(children_of(&next));
    if output.len() > TASK_ANCESTOR_HOP_LIMIT {
      return Err(StoreError::conflict(vec![format!(
        "subtree of {session_id} exceeds {TASK_ANCESTOR_HOP_LIMIT} nodes"
      )]));
    }
  }
  Ok(output)
}

fn archived_of(store: &Store, index: &TreeIndex, session_id: &str) -> StoreResult<bool> {
  let mut current_id = session_id;
  let mut seen = HashSet::new();
  loop {
    if !seen.insert(current_id.to_string()) || seen.len() > TASK_ANCESTOR_HOP_LIMIT + 1 {
      return Err(StoreError::conflict(vec![format!("task relation cycle through {current_id}")]));
    }
    let meta =
      index.metas.get(current_id).ok_or_else(|| StoreError::not_found(format!("task {current_id} not found")))?;
    if store.task_state(current_id)? != "open" {
      return Ok(true);
    }
    if meta.task_parent_id.is_none() {
      return Ok(false);
    }
    current_id = meta.task_parent_id.as_deref().expect("parent id was checked");
  }
}

fn fold_task_facts(events: &[Value]) -> StoreResult<TaskFacts> {
  let mut facts = TaskFacts::default();
  for (position, event) in events.iter().enumerate() {
    if let Some(event_id) = event.get("id").and_then(Value::as_str)
      && !facts.event_ids.insert(event_id.to_string())
    {
      continue;
    }
    if let Some(event_id) = event.get("id").and_then(Value::as_str) {
      facts.events_by_id.insert(event_id.to_string(), event.clone());
      facts.event_order.push(event_id.to_string());
    }
    let event_type = event.get("type").and_then(Value::as_str).unwrap_or_default();
    match event_type {
      "clone_start" => {
        facts.task_state = "open".to_string();
        facts.boundary_index = None;
        facts.boundary_open = true;
        facts.input_candidates.clear();
        facts.imported_pending_ids.clear();
        facts.close_events.clear();
        facts.close_requests.clear();
        facts.delivered_reports.clear();
      }
      "task_created" => {
        if facts.boundary_index.is_none() {
          facts.boundary_index = Some(position);
          facts.input_candidates.clear();
        }
      }
      "task_imported" => {
        facts.boundary_index = Some(position);
        facts.imported_pending_ids.clear();
        let pending = match event.get("pending_inputs") {
          None | Some(Value::Null) => Vec::new(),
          Some(Value::Array(pending)) => pending.clone(),
          Some(_) => return Err(StoreError::new("task_imported pending_inputs must be an array")),
        };
        for entry in pending {
          let Some(input_id) = entry.get("input_id").and_then(Value::as_str).filter(|value| !value.is_empty()) else {
            return Err(StoreError::new(format!(
              "task_imported event {:?} lists a pending input without a stable input_id",
              event.get("id")
            )));
          };
          facts.imported_pending_ids.insert(input_id.to_string());
        }
        facts.input_candidates = facts
          .event_order
          .iter()
          .filter_map(|event_id| facts.events_by_id.get(event_id))
          .filter(|prior| admits_input(prior))
          .filter(|prior| {
            prior.get("id").and_then(Value::as_str).is_some_and(|id| facts.imported_pending_ids.contains(id))
          })
          .cloned()
          .collect();
      }
      "task_closed" => {
        facts.task_state = event
          .get("outcome")
          .and_then(Value::as_str)
          .filter(|value| !value.is_empty())
          .unwrap_or("completed")
          .to_string();
        facts.close_events.push(event.clone());
        facts.boundary_index = Some(position);
        facts.boundary_open = false;
        facts.imported_pending_ids.clear();
        facts.input_candidates.clear();
      }
      "task_reopened" => {
        facts.task_state = "open".to_string();
        facts.boundary_index = Some(position);
        facts.boundary_open = true;
      }
      "task_close_requested" => {
        facts.close_requests.push(event.clone());
      }
      "run_finished" => {
        if let Some(run_id) = event.get("run_id").and_then(Value::as_str) {
          let outcome = event.get("outcome").and_then(Value::as_str).unwrap_or("None").to_string();
          facts.run_outcomes.insert(run_id.to_string(), outcome.clone());
          facts.run_finish_positions.entry(run_id.to_string()).or_insert(position);
          if outcome == "success"
            && let Some(input_ids) = event.get("input_event_ids").and_then(Value::as_array)
          {
            facts.confirmed_input_ids.extend(input_ids.iter().filter_map(Value::as_str).map(str::to_string));
          }
        }
      }
      "task_input_acknowledged" => {
        if let Some(input_ids) = event.get("input_ids").and_then(Value::as_array) {
          facts.confirmed_input_ids.extend(input_ids.iter().filter_map(Value::as_str).map(str::to_string));
        }
      }
      "child_report" => {
        if let (Some(child_id), Some(child_event_id)) =
          (event.get("child_session_id").and_then(Value::as_str), event.get("child_event_id").and_then(Value::as_str))
        {
          facts.delivered_reports.insert((child_id.to_string(), child_event_id.to_string()));
        }
      }
      _ => {}
    }
    let imported =
      event.get("id").and_then(Value::as_str).is_some_and(|event_id| facts.imported_pending_ids.contains(event_id));
    if admits_input(event)
      && facts.boundary_open
      && (facts.boundary_index.is_none_or(|boundary| position > boundary) || imported)
    {
      facts.input_candidates.push(event.clone());
    }
  }
  facts.covered = events.len();
  Ok(facts)
}

fn admits_input(event: &Value) -> bool {
  match event.get("type").and_then(Value::as_str) {
    Some("user") => event.get("content").is_some_and(Value::is_string),
    Some("agent_message" | "scheduled_trigger" | "child_report") => true,
    _ => false,
  }
}

fn holds_real_user_message(events: &[Value]) -> bool {
  let mut holds = false;
  for event in events {
    match event.get("type").and_then(Value::as_str) {
      Some("clone_start") => holds = false,
      Some("user") if event.get("content").is_some_and(Value::is_string) => holds = true,
      _ => {}
    }
  }
  holds
}

fn no_user_message_error(session_id: &str) -> StoreError {
  StoreError::forbidden(format!(
    "Delegation blocked: no user request on the task chain from task {session_id} to its root. A delegation serves a line that a real user message started; agent messages, scheduled triggers and child reports do not count."
  ))
}

/// Whether a node's task carries the read-only exemption: verify and cold-read are read-only and repo-less, so the
/// runtime gate does not need a user message for them. The one owner of the judgment
/// (`takeoff_gate.is_read_only_exempt`); no task is not exempt.
pub fn is_read_only_exempt(task: Option<&TaskSpec>) -> bool {
  task
    .and_then(|task| task.task_type)
    .is_some_and(|task_type| matches!(task_type, TaskType::Verify | TaskType::ColdRead))
}

fn task_spec_hash(task: &TaskSpec) -> StoreResult<String> {
  let serialized = serde_json::to_string(task).map_err(|error| StoreError::new(error.to_string()))?;
  Ok(control_events::sha256_hex(&serialized))
}

/// One archive-format close fact: outcome `archived` and no report target, so an archive wakes nobody.
pub(crate) fn archived_close_event(
  session_id: &str,
  request_id: &str,
  actor: &str,
  summary: &str,
  run_ids: &[&str],
  archived_with: Option<&str>,
) -> Value {
  let mut payload = vec![
    ("request_id".to_string(), Value::String(request_id.to_string())),
    ("outcome".to_string(), Value::String("archived".to_string())),
    ("summary".to_string(), Value::String(summary.to_string())),
    ("result_refs".to_string(), json!([])),
    ("run_ids".to_string(), json!(run_ids)),
    ("report_to".to_string(), Value::Null),
  ];
  if let Some(archived_with) = archived_with {
    payload.push(("archived_with".to_string(), Value::String(archived_with.to_string())));
  }
  control_events::build_control_event(
    "task_closed",
    actor,
    session_id,
    Some(&control_events::stable_close_event_id(session_id, request_id)),
    None,
    payload,
  )
}

fn caller_actor(caller: &TaskCaller) -> &'static str {
  match caller {
    TaskCaller::Operator => ACTOR_USER,
    TaskCaller::System => ACTOR_SYSTEM,
    TaskCaller::Agent { .. } => ACTOR_AGENT,
  }
}

fn set_registered_slot_values(
  slots: &[charliebot_core::metadata_slot::MetadataSlot],
  meta: &mut SessionMetadata,
  values: Map<String, Value>,
) -> StoreResult<()> {
  for (name, value) in values {
    let matches = slots
      .iter()
      .filter(|slot| slot.on == charliebot_core::metadata_slot::MetadataFile::Session)
      .flat_map(|slot| slot.fields.iter())
      .filter(|field| field.name == name)
      .count();
    if matches == 0 {
      return Err(StoreError::new(format!("no owner registered the key {name:?}")));
    }
    if matches > 1 {
      return Err(StoreError::new(format!("metadata slot key {name:?} is registered more than once")));
    }
    meta.set_extra(name, value);
  }
  Ok(())
}

fn default_task_name(task: &TaskSpec, profile: TaskProfile) -> Option<String> {
  let lines: Vec<&str> = task.goal.lines().filter(|line| !line.trim().is_empty()).collect();
  let content = lines.iter().copied().find(|line| !is_markdown_heading(line));
  let selected = content.or_else(|| lines.first().copied().map(strip_heading))?;
  let selected = selected.trim();
  if selected.is_empty() {
    return Some(format!("New {} task", profile_name(profile)));
  }
  Some(selected.chars().take(80).collect())
}

fn is_markdown_heading(line: &str) -> bool {
  let leading_spaces = line.chars().take_while(|value| *value == ' ').count();
  if leading_spaces > 3 {
    return false;
  }
  let suffix = &line[leading_spaces..];
  let marks = suffix.chars().take_while(|value| *value == '#').count();
  (1..=6).contains(&marks) && suffix[marks..].chars().next().is_none_or(char::is_whitespace)
}

fn strip_heading(line: &str) -> &str {
  let trimmed = line.trim_start();
  trimmed.trim_start_matches('#').trim_start()
}

fn profile_name(profile: TaskProfile) -> &'static str {
  match profile {
    TaskProfile::Manager => "manager",
    TaskProfile::Worker => "worker",
  }
}

fn count_session_dirs(sessions_dir: &Path) -> StoreResult<i64> {
  let mut count = 0;
  for entry in fs::read_dir(sessions_dir).map_err(|error| path_error(sessions_dir, error))? {
    // Path::is_dir follows symlinks, as Python's Path.is_dir does; DirEntry::file_type would not.
    if entry.map_err(|error| path_error(sessions_dir, error))?.path().is_dir() {
      count += 1;
    }
  }
  Ok(count)
}

fn path_error(path: &Path, error: impl std::fmt::Display) -> StoreError {
  StoreError::new(format!("{}: {error}", path.display()))
}

#[cfg(test)]
pub(crate) mod tests {
  use std::collections::HashMap;
  use std::sync::{Arc, Mutex};

  use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot, SlotField};
  use serde_json::{Map, Value, json};

  use super::*;
  use crate::metadata::{TaskProfile, TaskSpec};
  use crate::run_records::{RunKind, RunOutcome, RunRecord};
  use crate::store::{
    AccountLabelKeeper, AnchorFieldReconciler, SequenceArchiveHook, SequenceBindingMover, SequenceListingFields,
    StoreHookFuture, StoreHooks,
  };
  use crate::streaming::{SIDEBAR_CHANNEL, Subscription};

  struct TestAccountLabels;

  impl AccountLabelKeeper for TestAccountLabels {
    fn record_account_label(&self, meta: &mut SessionMetadata, label: Option<&str>) -> StoreResult<bool> {
      let value = label.map_or(Value::Null, |value| Value::String(value.to_string()));
      if meta.extra("account_label") == Some(&value) {
        return Ok(false);
      }
      meta.set_extra("account_label", value);
      Ok(true)
    }

    fn account_label(&self, meta: &SessionMetadata) -> Option<String> {
      meta.extra("account_label").and_then(Value::as_str).map(str::to_string)
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

    fn listing_fields(&self, _ids: &[String], _now: UtcDateTime) -> StoreResult<HashMap<String, Map<String, Value>>> {
      Ok(HashMap::new())
    }

    fn sequence_subtree_roots(&self, _metas: &[SessionMetadata]) -> StoreResult<HashMap<String, String>> {
      Ok(HashMap::new())
    }

    fn view_subtree_roots(&self, _metas: &[SessionMetadata]) -> StoreResult<HashMap<String, HashMap<String, String>>> {
      Ok(HashMap::new())
    }
  }

  struct TestSequenceBindingMover;

  impl SequenceBindingMover for TestSequenceBindingMover {
    fn move_bindings<'a>(&'a self, _source: &'a str, _new: &'a str) -> StoreHookFuture<'a, bool> {
      Box::pin(async { Ok(true) })
    }
  }

  fn slots() -> Vec<MetadataSlot> {
    vec![
      MetadataSlot {
        owner: "cron",
        on: MetadataFile::Session,
        after: Some("backend"),
        fields: vec![
          SlotField { name: "last_scheduled_run", default: Value::Null },
          SlotField { name: "last_scheduled_cron", default: Value::Null },
          SlotField { name: "last_run_status", default: Value::Null },
        ],
      },
      MetadataSlot {
        owner: "slack",
        on: MetadataFile::Session,
        after: Some("successor_session_id"),
        fields: vec![SlotField { name: "slack_watermark_ts", default: Value::Null }],
      },
    ]
  }

  pub(crate) fn make_store(home: &Path) -> Store {
    make_store_with_archive(home, Arc::new(crate::store::test_support::SequenceArchive))
  }

  fn make_store_with_archive(home: &Path, sequence_archive: Arc<dyn SequenceArchiveHook>) -> Store {
    fs::create_dir_all(home).unwrap();
    let cfg = charliebot_core::config::load_config(home, &charliebot_core::config::ConfigSchema::default()).unwrap();
    Store::new(
      cfg,
      slots(),
      StoreHooks {
        account_labels: Some(Arc::new(TestAccountLabels)),
        anchor_reconciler: Some(Arc::new(TestAnchorReconciler)),
        sequence_listing: Some(Arc::new(TestSequenceListing)),
        runtime_events: Some(Arc::new(crate::hooks::EmptyRuntimeEventHooks)),
        sequence_archive: Some(sequence_archive),
        sequence_binding_mover: Some(Arc::new(TestSequenceBindingMover)),
      },
    )
    .unwrap()
  }

  pub(crate) async fn create_node(
    store: &Store,
    request_id: &str,
    parent: Option<&str>,
    profile: TaskProfile,
  ) -> SessionMetadata {
    store
      .create_task(
        CreateTask {
          request_id: request_id.to_string(),
          task_parent_id: parent.map(str::to_string),
          profile,
          task: None,
          name: Some(request_id.to_string()),
          backend: Some("synthetic".to_string()),
          group: None,
          session_id: None,
        },
        TaskCaller::System,
        Map::new(),
      )
      .await
      .unwrap()
  }

  async fn create_unnamed_node(store: &Store, request_id: &str) -> SessionMetadata {
    store
      .create_task(
        CreateTask {
          request_id: request_id.to_string(),
          task_parent_id: None,
          profile: TaskProfile::Manager,
          task: None,
          name: None,
          backend: Some("synthetic".to_string()),
          group: None,
          session_id: None,
        },
        TaskCaller::System,
        Map::new(),
      )
      .await
      .unwrap()
  }

  fn user_event(content: Value) -> Value {
    json!({"type": "user", "content": content})
  }

  async fn append(store: &Store, session_id: &str, event: Value) {
    store.append_chat_event(session_id, event).await.unwrap();
  }

  #[tokio::test]
  async fn create_replay_and_slot_updates_preserve_the_original_node() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let root = create_node(&store, "root-request", None, TaskProfile::Manager).await;
    let mut values = Map::new();
    values.insert("slack_watermark_ts".to_string(), json!("before"));
    let child_request = CreateTask {
      request_id: "child-request".to_string(),
      task_parent_id: Some(root.id.clone()),
      profile: TaskProfile::Worker,
      task: Some(TaskSpec { goal: "child goal".to_string(), ..TaskSpec::default() }),
      name: Some("Child".to_string()),
      backend: Some("synthetic".to_string()),
      group: Some("group-a".to_string()),
      session_id: None,
    };
    let child = store.create_task(child_request.clone(), TaskCaller::System, values).await.unwrap();
    let event_count = store.load_chat_events_sync(&child.id).unwrap().len();
    let replay = store.create_task(child_request, TaskCaller::System, Map::new()).await.unwrap();
    assert_eq!(replay.created_by_event, child.created_by_event);
    assert_eq!(store.load_chat_events_sync(&child.id).unwrap().len(), event_count);

    let guard = store.control_lock().await;
    let mut update = Map::new();
    update.insert("slack_watermark_ts".to_string(), json!("after"));
    let updated = store.update_task_slot_fields_locked(&guard, &child.id, "slack", update).await.unwrap();
    assert_eq!(updated.extra("slack_watermark_ts"), Some(&json!("after")));
    assert_eq!(updated.backend, "synthetic");
    assert_eq!(updated.task_parent_id.as_deref(), Some(root.id.as_str()));
  }

  #[tokio::test]
  async fn record_native_anchor_reset_clears_only_the_held_conversation() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let node = create_node(&store, "native-anchor", None, TaskProfile::Manager).await;
    store.persist_cc_session_id(&node.id, "previous-conversation", None).await.unwrap();

    store.record_native_anchor_with_reset(&node.id, "prompt-hash", "synthetic", Some("model-a"), true).await.unwrap();

    let updated = store.read_metadata_fresh(&node.id).unwrap().unwrap();
    assert_eq!(updated.native_prompt_hash.as_deref(), Some("prompt-hash"));
    assert_eq!(updated.native_backend.as_deref(), Some("synthetic"));
    assert_eq!(updated.native_model.as_deref(), Some("model-a"));
    assert_eq!(updated.cc_session_id, None);
    assert_eq!(updated.cc_session_started_at, None);
  }

  #[tokio::test]
  async fn gate_matches_manager_chain_user_and_open_state_rules() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let root = create_node(&store, "root", None, TaskProfile::Manager).await;
    let child = create_node(&store, "child", Some(&root.id), TaskProfile::Manager).await;
    let grandchild = create_node(&store, "grandchild", Some(&child.id), TaskProfile::Manager).await;

    append(&store, &root.id, user_event(json!("Can you inspect this?"))).await;
    append(&store, &child.id, json!({"type": "agent_message", "content": "progress"})).await;
    assert_eq!(store.check_task_authorization(&grandchild.id).unwrap(), root.id);
    append(&store, &child.id, user_event(json!("Which file?"))).await;
    assert_eq!(store.check_task_authorization(&grandchild.id).unwrap(), child.id);

    for state in ["completed", "cancelled", "archived"] {
      let guard = store.control_lock().await;
      let event = control_events::build_control_event(
        "task_closed",
        ACTOR_SYSTEM,
        &root.id,
        None,
        None,
        [("outcome".to_string(), json!(state))],
      );
      store.append_control_fact(&guard, &root.id, event).await.unwrap();
      drop(guard);
      let error = store.check_task_authorization(&grandchild.id).unwrap_err();
      assert_eq!(error.to_string(), format!("Delegation blocked: ancestor task {} is {state}; open it first", root.id));
      let guard = store.control_lock().await;
      let reopen = control_events::build_control_event("task_reopened", ACTOR_SYSTEM, &root.id, None, None, []);
      store.append_control_fact(&guard, &root.id, reopen).await.unwrap();
    }

    let worker = create_node(&store, "worker", Some(&child.id), TaskProfile::Worker).await;
    let error = store.check_task_authorization(&worker.id).unwrap_err();
    assert_eq!(
      error.to_string(),
      format!("task {} is not a manager; agent calls run only under a manager task", worker.id)
    );
  }

  #[tokio::test]
  async fn gate_ignores_machine_inputs_and_resets_at_clone_markers() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let root = create_node(&store, "root", None, TaskProfile::Manager).await;
    let child = create_node(&store, "child", Some(&root.id), TaskProfile::Manager).await;
    let grandchild = create_node(&store, "grandchild", Some(&child.id), TaskProfile::Manager).await;
    for session_id in [&root.id, &child.id, &grandchild.id] {
      append(&store, session_id, json!({"type": "agent_message", "content": "take off"})).await;
      append(&store, session_id, json!({"type": "scheduled_trigger", "content": "take off"})).await;
      append(&store, session_id, json!({"type": "child_report", "content": "take off"})).await;
      append(&store, session_id, user_event(json!([{"type": "tool_result", "content": "take off"}]))).await;
    }
    let error = store.check_task_authorization(&grandchild.id).unwrap_err();
    assert!(error.to_string().contains("no user request on the task chain"));

    let clone = create_node(&store, "clone", Some(&root.id), TaskProfile::Manager).await;
    append(&store, &clone.id, user_event(json!("source request"))).await;
    append(&store, &clone.id, json!({"type": "clone_start"})).await;
    assert!(store.check_task_authorization(&clone.id).unwrap_err().to_string().contains("no user request"));
    append(&store, &clone.id, user_event(json!("new request"))).await;
    assert_eq!(store.check_task_authorization(&clone.id).unwrap(), clone.id);
    append(&store, &clone.id, json!({"type": "clone_start"})).await;
    assert!(store.check_task_authorization(&clone.id).unwrap_err().to_string().contains("no user request"));
  }

  #[tokio::test]
  async fn agent_create_runs_the_gate_except_for_coordination_and_read_only_work() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let root = create_node(&store, "agent-root", None, TaskProfile::Manager).await;
    let manager_request = CreateTask {
      request_id: "agent-manager-child".to_string(),
      task_parent_id: Some(root.id.clone()),
      profile: TaskProfile::Manager,
      task: None,
      name: Some("coordination child".to_string()),
      backend: Some("synthetic".to_string()),
      group: None,
      session_id: None,
    };
    assert!(
      store.create_task(manager_request, TaskCaller::Agent { session_id: root.id.clone() }, Map::new()).await.is_ok()
    );

    let worker_request = CreateTask {
      request_id: "agent-worker-child".to_string(),
      task_parent_id: Some(root.id.clone()),
      profile: TaskProfile::Worker,
      task: None,
      name: Some("implementation child".to_string()),
      backend: Some("synthetic".to_string()),
      group: None,
      session_id: None,
    };
    assert!(
      store
        .create_task(worker_request.clone(), TaskCaller::Agent { session_id: root.id.clone() }, Map::new())
        .await
        .unwrap_err()
        .to_string()
        .contains("no user request on the task chain")
    );

    let verify_request = CreateTask {
      task: Some(TaskSpec { task_type: Some(TaskType::Verify), ..TaskSpec::default() }),
      request_id: "agent-verify-child".to_string(),
      ..worker_request.clone()
    };
    assert!(
      store.create_task(verify_request, TaskCaller::Agent { session_id: root.id.clone() }, Map::new()).await.is_ok()
    );
    append(&store, &root.id, user_event(json!("Please make the change."))).await;
    assert!(
      store
        .create_task(
          CreateTask { request_id: "agent-worker-after-user".to_string(), ..worker_request },
          TaskCaller::Agent { session_id: root.id },
          Map::new(),
        )
        .await
        .is_ok()
    );
  }

  #[tokio::test]
  async fn task_index_archive_activity_and_successor_reads_follow_durable_state() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let root = create_node(&store, "root", None, TaskProfile::Manager).await;
    let child = create_node(&store, "child", Some(&root.id), TaskProfile::Manager).await;
    let leaf = create_node(&store, "leaf", Some(&child.id), TaskProfile::Worker).await;
    assert_eq!(store.ancestor_ids(&leaf.id).unwrap(), [child.id.clone(), root.id.clone()]);
    let child_ids = store.child_ids(&root.id).unwrap();
    assert_eq!(child_ids.len(), 1);
    assert_eq!(child_ids[0], child.id);
    assert_eq!(store.descendant_ids(&root.id).unwrap(), [child.id.clone(), leaf.id.clone()]);

    let run = RunRecord {
      id: "queued-run".to_string(),
      session_id: leaf.id.clone(),
      kind: RunKind::Work,
      input_event_ids: Vec::new(),
      task_spec_hash: None,
      task_spec_ref: None,
      prompt_snapshot_ref: None,
      retry_of_run_id: None,
      review_of_run_id: None,
      backend: Some("synthetic".to_string()),
      model: None,
      native_session_id: None,
      pid: None,
      pid_start: None,
      started_at: None,
      ended_at: None,
      exit_code: None,
      repo_path: None,
      base_branch: None,
      branch_name: None,
      worktree_path: None,
      sequence_ref: None,
      raw_log_ref: None,
      events_ref: None,
      result_ref: None,
    };
    let guard = store.control_lock().await;
    store.register_run_locked(&guard, &leaf.id, run, None).await.unwrap();
    assert_eq!(store.task_tree_activity(&leaf.id).unwrap().work_state, "waiting");
    let stopped = control_events::build_control_event(
      "run_stop_requested",
      ACTOR_SYSTEM,
      &leaf.id,
      None,
      None,
      [("request_id".to_string(), json!("cancel-queued")), ("run_id".to_string(), json!("queued-run"))],
    );
    store.append_control_fact(&guard, &leaf.id, stopped).await.unwrap();
    let close = control_events::build_control_event(
      "task_closed",
      ACTOR_SYSTEM,
      &root.id,
      None,
      None,
      [("outcome".to_string(), json!("archived"))],
    );
    store.append_control_fact(&guard, &root.id, close).await.unwrap();
    drop(guard);
    store.invalidate_tree_index();
    assert!(store.archived_of(&leaf.id).unwrap());
    let mut archived = store.derived_archived_ids().unwrap();
    archived.sort();
    let mut expected_archived = vec![root.id.clone(), child.id.clone(), leaf.id.clone()];
    expected_archived.sort();
    assert_eq!(archived, expected_archived);
    let active: Vec<String> = store
      .list_sessions(Some(SessionStatus::Active), None, None)
      .unwrap()
      .rows
      .into_iter()
      .map(|meta| meta.id)
      .collect();
    assert!(!active.contains(&leaf.id));
    let archived_page = store.list_archived_page(None, 10, None, None).unwrap();
    let archived_page_ids: Vec<String> = archived_page.sessions.into_iter().map(|meta| meta.id).collect();
    assert!(archived_page_ids.contains(&root.id));
    assert!(archived_page_ids.contains(&child.id));
    assert!(archived_page_ids.contains(&leaf.id));
    assert_eq!(store.read_metadata_fresh(&leaf.id).unwrap().unwrap().status, SessionStatus::Active);
    assert_eq!(store.task_tree_activity(&leaf.id).unwrap().work_state, "idle");

    let successor = create_node(&store, "successor", None, TaskProfile::Manager).await;
    let guard = store.control_lock().await;
    store
      .mutate_fresh(&root.id, false, true, |meta| {
        meta.successor_session_id = Some(successor.id.clone());
        Ok(true)
      })
      .await
      .unwrap();
    drop(guard);
    store.invalidate_tree_index();
    let chain = store.resolve_successor_chain(&root.id).unwrap();
    assert_eq!(
      chain.iter().map(|meta| meta.id.as_str()).collect::<Vec<_>>(),
      [root.id.as_str(), successor.id.as_str()]
    );
    assert_eq!(
      store.deliver_to_successor(&root.id, json!({"type": "agent_message", "content": "forwarded"})).await.unwrap(),
      Some(successor.id.clone())
    );
    let event = store.load_chat_events_sync(&successor.id).unwrap().pop().unwrap();
    assert_eq!(event["origin_session_id"], root.id);
  }

  #[tokio::test]
  async fn prompt_facts_are_idempotent_and_elone_rebind_copies_cron_fields() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let source = create_node(&store, "source", None, TaskProfile::Manager).await;
    let new = create_node(&store, "new", None, TaskProfile::Manager).await;
    let ref_a = store.store_prompt_body("synthetic rule\n").unwrap();
    let ref_b = store.store_prompt_body("replacement rule\n").unwrap();
    let mut metadata = source.clone();
    metadata.node_prompt_ref = Some(ref_a.clone());
    store.ensure_prompt_changed_fact(&source.id, &metadata, PromptScope::Node).await.unwrap();
    let first_len = store.load_chat_events_sync(&source.id).unwrap().len();
    store.ensure_prompt_changed_fact(&source.id, &metadata, PromptScope::Node).await.unwrap();
    assert_eq!(store.load_chat_events_sync(&source.id).unwrap().len(), first_len);
    metadata.node_prompt_ref = Some(ref_b.clone());
    store.ensure_prompt_changed_fact(&source.id, &metadata, PromptScope::Node).await.unwrap();
    let prompts: Vec<_> = store
      .load_chat_events_sync(&source.id)
      .unwrap()
      .into_iter()
      .filter(|event| event["type"] == "prompt_changed")
      .collect();
    assert_eq!(prompts[1]["previous_ref"], ref_a);
    assert_eq!(prompts[1]["new_ref"], ref_b);

    let mut source_slots = Map::new();
    source_slots.insert("last_scheduled_run".to_string(), json!("2025-02-03T04:05:06+00:00"));
    source_slots.insert("last_scheduled_cron".to_string(), json!("0 0 * * *"));
    source_slots.insert("last_run_status".to_string(), json!("success"));
    let source = {
      let guard = store.control_lock().await;
      store.update_slot_fields_locked(&guard, &source.id, "cron", source_slots).await.unwrap().unwrap()
    };
    let guard = store.control_lock().await;
    assert!(store.rebind_elone_bindings_locked(&guard, &source.id, &new.id).await.unwrap());
    let updated = store.read_metadata_fresh(&new.id).unwrap().unwrap();
    assert_eq!(updated.extra("last_scheduled_cron"), Some(&json!("0 0 * * *")));
    assert_eq!(updated.extra("last_run_status"), Some(&json!("success")));
  }

  #[tokio::test]
  async fn unsuccessful_worker_archives_once_after_a_failed_run() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let root = create_node(&store, "root", None, TaskProfile::Manager).await;
    let worker = create_node(&store, "worker", Some(&root.id), TaskProfile::Worker).await;
    let run = RunRecord {
      id: "failed-run".to_string(),
      session_id: worker.id.clone(),
      kind: RunKind::Work,
      input_event_ids: Vec::new(),
      task_spec_hash: None,
      task_spec_ref: None,
      prompt_snapshot_ref: None,
      retry_of_run_id: None,
      review_of_run_id: None,
      backend: Some("synthetic".to_string()),
      model: None,
      native_session_id: None,
      pid: None,
      pid_start: None,
      started_at: None,
      ended_at: None,
      exit_code: None,
      repo_path: None,
      base_branch: None,
      branch_name: None,
      worktree_path: None,
      sequence_ref: None,
      raw_log_ref: None,
      events_ref: None,
      result_ref: None,
    };
    let guard = store.control_lock().await;
    store.register_run_locked(&guard, &worker.id, run, None).await.unwrap();
    store
      .record_finish_locked(&guard, &worker.id, "failed-run", RunOutcome::new(crate::RunOutcomeKind::Failed))
      .await
      .unwrap();
    assert!(store.archive_unsuccessful_worker(&guard, &worker.id, "failed-run", "failed").await.unwrap());
    assert!(!store.archive_unsuccessful_worker(&guard, &worker.id, "failed-run", "failed").await.unwrap());
    assert_eq!(store.task_state(&worker.id).unwrap(), "archived");
  }

  #[derive(Default)]
  struct RecordingArchive {
    calls: Mutex<Vec<String>>,
  }

  impl SequenceArchiveHook for RecordingArchive {
    fn on_archive<'a>(&'a self, session_id: &'a str) -> StoreHookFuture<'a, ()> {
      self.calls.lock().unwrap().push(session_id.to_string());
      Box::pin(async { Ok(()) })
    }
  }

  pub(crate) fn queued_run(session_id: &str, run_id: &str) -> RunRecord {
    RunRecord {
      id: run_id.to_string(),
      session_id: session_id.to_string(),
      kind: RunKind::Work,
      input_event_ids: Vec::new(),
      task_spec_hash: None,
      task_spec_ref: None,
      prompt_snapshot_ref: None,
      retry_of_run_id: None,
      review_of_run_id: None,
      backend: Some("synthetic".to_string()),
      model: None,
      native_session_id: None,
      pid: None,
      pid_start: None,
      started_at: None,
      ended_at: None,
      exit_code: None,
      repo_path: None,
      base_branch: None,
      branch_name: None,
      worktree_path: None,
      sequence_ref: None,
      raw_log_ref: None,
      events_ref: None,
      result_ref: None,
    }
  }

  #[tokio::test]
  async fn archive_subtree_closes_each_open_node_once_and_calls_each_binding_parent_first() {
    let home = tempfile::tempdir().unwrap();
    let archive = Arc::new(RecordingArchive::default());
    let store = make_store_with_archive(home.path(), archive.clone());
    let root = create_node(&store, "root", None, TaskProfile::Manager).await;
    let mid = create_node(&store, "mid", Some(&root.id), TaskProfile::Manager).await;
    let leaf = create_node(&store, "leaf", Some(&mid.id), TaskProfile::Worker).await;
    let completed = create_node(&store, "done", Some(&root.id), TaskProfile::Worker).await;
    let guard = store.control_lock().await;
    let close = control_events::build_control_event(
      "task_closed",
      ACTOR_SYSTEM,
      &completed.id,
      None,
      None,
      [("outcome".to_string(), json!("completed"))],
    );
    store.append_control_fact(&guard, &completed.id, close).await.unwrap();
    drop(guard);

    let archived = store.archive_subtree(&root.id, TaskCaller::Operator).await.unwrap();

    assert_eq!(archived, [root.id.clone(), mid.id.clone(), leaf.id.clone()]);
    assert_eq!(archive.calls.lock().unwrap().clone(), archived);
    let closes = |session_id: &str| -> Vec<Value> {
      store
        .load_chat_events_sync(session_id)
        .unwrap()
        .into_iter()
        .filter(|event| event["type"] == "task_closed")
        .collect()
    };
    for node in [&root.id, &mid.id, &leaf.id] {
      assert_eq!(store.task_state(node).unwrap(), "archived");
    }
    let root_closes = closes(&root.id);
    assert_eq!(root_closes.len(), 1);
    assert_eq!(root_closes[0]["outcome"], "archived");
    assert_eq!(root_closes[0]["actor"], "user");
    assert_eq!(root_closes[0]["summary"], "archived by the user");
    assert_eq!(root_closes[0]["run_ids"], json!([]));
    assert_eq!(root_closes[0]["report_to"], Value::Null);
    assert!(root_closes[0].get("archived_with").is_none());
    for descendant in [&mid.id, &leaf.id] {
      let descendant_closes = closes(descendant);
      assert_eq!(descendant_closes.len(), 1);
      assert_eq!(descendant_closes[0]["outcome"], "archived");
      assert_eq!(descendant_closes[0]["summary"], "");
      assert_eq!(descendant_closes[0]["archived_with"], root.id);
    }
    assert_eq!(store.task_state(&completed.id).unwrap(), "completed");
    assert!(closes(&completed.id).iter().all(|fact| fact["outcome"] != "archived"));

    assert!(store.archive_subtree(&root.id, TaskCaller::Operator).await.unwrap().is_empty());
    assert_eq!(closes(&root.id).len(), 1);
    assert_eq!(archive.calls.lock().unwrap().len(), 3);
    let error = store.archive_subtree("missing-task", TaskCaller::Operator).await.unwrap_err();
    assert_eq!(error.to_string(), "task missing-task not found");
  }

  #[tokio::test]
  async fn archive_subtree_refuses_an_unfinished_run_anywhere_in_the_subtree_and_writes_nothing() {
    let home = tempfile::tempdir().unwrap();
    let archive = Arc::new(RecordingArchive::default());
    let store = make_store_with_archive(home.path(), archive.clone());
    let root = create_node(&store, "root", None, TaskProfile::Manager).await;
    let mid = create_node(&store, "mid", Some(&root.id), TaskProfile::Manager).await;
    let leaf = create_node(&store, "leaf", Some(&mid.id), TaskProfile::Worker).await;
    let guard = store.control_lock().await;
    store.register_run_locked(&guard, &leaf.id, queued_run(&leaf.id, "queued-run"), None).await.unwrap();
    drop(guard);

    let error = store.archive_subtree(&root.id, TaskCaller::Operator).await.unwrap_err();

    let blocker = format!("{}: run queued-run is queued (pending dispatch)", leaf.id);
    assert_eq!(error.to_string(), blocker);
    assert_eq!(error.status_code(), Some(409));
    assert_eq!(error.blockers(), Some(std::slice::from_ref(&blocker)));
    for node in [&root.id, &mid.id, &leaf.id] {
      assert_eq!(store.task_state(node).unwrap(), "open");
      assert!(store.load_chat_events_sync(node).unwrap().iter().all(|event| event["type"] != "task_closed"));
    }
    assert!(archive.calls.lock().unwrap().is_empty());
  }

  #[tokio::test]
  async fn archive_subtree_is_operator_only_and_writes_nothing_for_other_callers() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let root = create_node(&store, "root", None, TaskProfile::Manager).await;

    for caller in [TaskCaller::System, TaskCaller::Agent { session_id: root.id.clone() }] {
      let error = store.archive_subtree(&root.id, caller).await.unwrap_err();
      assert_eq!(error.to_string(), "archiving a task requires operator credentials");
      assert_eq!(error.status_code(), Some(403));
    }
    assert_eq!(store.task_state(&root.id).unwrap(), "open");
    assert!(store.load_chat_events_sync(&root.id).unwrap().iter().all(|event| event["type"] != "task_closed"));
  }

  /// Structural guards keyed by node: the dispatcher answers these in production.
  #[derive(Default)]
  struct StubGuards {
    pending: HashMap<String, Vec<String>>,
  }

  impl StructuralBlockers for StubGuards {
    fn pending_input_blockers(&self, session_id: &str) -> StoreResult<Vec<String>> {
      Ok(self.pending.get(session_id).cloned().unwrap_or_default())
    }

    fn undelivered_report_blockers(&self, _session_id: &str) -> StoreResult<Vec<String>> {
      Ok(Vec::new())
    }
  }

  fn prompt_facts(store: &Store, session_id: &str) -> Vec<Value> {
    store
      .load_chat_events_sync(session_id)
      .unwrap()
      .into_iter()
      .filter(|event| event["type"] == event_types::PROMPT_CHANGED)
      .collect()
  }

  #[tokio::test]
  async fn patch_task_refuses_structural_changes_under_guards_and_writes_nothing() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let root = create_node(&store, "root", None, TaskProfile::Manager).await;
    let other = create_node(&store, "other", None, TaskProfile::Manager).await;
    let worker = create_node(&store, "worker", Some(&root.id), TaskProfile::Worker).await;
    let pending = format!("{} has an unprocessed input", worker.id);
    let guards = StubGuards { pending: HashMap::from([(worker.id.clone(), vec![pending.clone()])]) };

    let demotion = PatchTask { profile: Some(Some(TaskProfile::Worker)), ..Default::default() };
    let error = store.patch_task(&root.id, demotion, TaskCaller::Operator, &guards).await.unwrap_err();
    let blocker = "demotion to worker requires a task with no child tasks".to_string();
    assert_eq!(error.status_code(), Some(409));
    assert_eq!(error.blockers(), Some(std::slice::from_ref(&blocker)));

    // The node's own guard counts once for the structural change and once for the moving subtree, as Python's does.
    let reparent = PatchTask { task_parent_id: Some(Some(other.id.clone())), ..Default::default() };
    let error = store.patch_task(&worker.id, reparent, TaskCaller::Operator, &guards).await.unwrap_err();
    let mut expected = vec![pending.clone(), format!("{}: {pending}", worker.id)];
    expected.sort();
    assert_eq!(error.blockers(), Some(expected.as_slice()));

    let ghost = PatchTask { task_parent_id: Some(Some("ghost".to_string())), ..Default::default() };
    let error = store.patch_task(&worker.id, ghost, TaskCaller::Operator, &guards).await.unwrap_err();
    assert_eq!(error.status_code(), Some(404));
    assert_eq!(error.to_string(), "task ghost not found");

    let renamed = PatchTask { name: Some("renamed".to_string()), ..Default::default() };
    let error = store.patch_task(&root.id, renamed, TaskCaller::System, &guards).await.unwrap_err();
    assert_eq!(error.to_string(), "task metadata mutations require operator credentials");
    assert_eq!(error.status_code(), Some(403));

    let root_after = store.read_metadata_fresh(&root.id).unwrap().unwrap();
    let worker_after = store.read_metadata_fresh(&worker.id).unwrap().unwrap();
    assert_eq!(root_after.profile, TaskProfile::Manager);
    assert_eq!(root_after.name, root.name);
    assert_eq!(worker_after.task_parent_id.as_deref(), Some(root.id.as_str()));
    assert!(prompt_facts(&store, &root.id).is_empty());
  }

  #[tokio::test]
  async fn patch_task_lands_one_prompt_fact_per_scope_edit_and_is_idempotent() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let root = create_node(&store, "root", None, TaskProfile::Manager).await;
    let subtree_ref = store.store_prompt_body("subtree rule\n").unwrap();
    let node_ref = store.store_prompt_body("node rule\n").unwrap();
    let edit = || PatchTask {
      name: Some("renamed".to_string()),
      subtree_prompt: Some(Some("subtree rule\n".to_string())),
      node_prompt: Some(Some("node rule\n".to_string())),
      ..Default::default()
    };

    let guards = StubGuards::default();
    let patched = store.patch_task(&root.id, edit(), TaskCaller::Operator, &guards).await.unwrap();
    assert_eq!(patched.name, "renamed");
    assert_eq!(patched.subtree_prompt_ref.as_deref(), Some(subtree_ref.as_str()));
    assert_eq!(patched.node_prompt_ref.as_deref(), Some(node_ref.as_str()));
    assert_eq!(prompt_facts(&store, &root.id).len(), 2);

    store.patch_task(&root.id, edit(), TaskCaller::Operator, &guards).await.unwrap();
    assert_eq!(prompt_facts(&store, &root.id).len(), 2);

    let cleared = PatchTask { node_prompt: Some(None), ..Default::default() };
    let patched = store.patch_task(&root.id, cleared, TaskCaller::Operator, &guards).await.unwrap();
    assert_eq!(patched.node_prompt_ref, None);
    let facts = prompt_facts(&store, &root.id);
    assert_eq!(facts.len(), 3);
    assert_eq!(facts[2]["scope"], "node");
    assert_eq!(facts[2]["previous_ref"], node_ref);
    assert_eq!(facts[2]["new_ref"], Value::Null);
  }

  #[tokio::test]
  async fn patch_task_lands_an_interrupted_prompt_edit_before_the_next_one() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let root = create_node(&store, "root", None, TaskProfile::Manager).await;
    let interrupted_ref = store.store_prompt_body("interrupted rule\n").unwrap();
    // A crash after the metadata swap and before its fact: the metadata is ahead of the fact stream.
    store
      .mutate_fresh(&root.id, false, true, |meta| {
        meta.node_prompt_ref = Some(interrupted_ref.clone());
        Ok(true)
      })
      .await
      .unwrap();
    assert!(prompt_facts(&store, &root.id).is_empty());

    let next = PatchTask { node_prompt: Some(Some("next rule\n".to_string())), ..Default::default() };
    store.patch_task(&root.id, next, TaskCaller::Operator, &StubGuards::default()).await.unwrap();

    let facts = prompt_facts(&store, &root.id);
    assert_eq!(facts.len(), 2);
    assert_eq!(facts[0]["previous_ref"], Value::Null);
    assert_eq!(facts[0]["new_ref"], interrupted_ref);
    assert_eq!(facts[1]["previous_ref"], interrupted_ref);
  }

  #[tokio::test]
  async fn delete_permanently_refuses_every_saved_task_reference_and_evidence() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());

    let parent = create_node(&store, "delete-parent", None, TaskProfile::Manager).await;
    let child = create_node(&store, "delete-child", Some(&parent.id), TaskProfile::Worker).await;
    let error = store.delete_permanently(&parent.id, TaskCaller::Operator).await.unwrap_err();
    assert_eq!(error.to_string(), format!("has child task(s): {}", child.id));

    let with_run = create_node(&store, "delete-with-run", None, TaskProfile::Manager).await;
    let guard = store.control_lock().await;
    store.register_run_locked(&guard, &with_run.id, queued_run(&with_run.id, "delete-run"), None).await.unwrap();
    drop(guard);
    let error = store.delete_permanently(&with_run.id, TaskCaller::Operator).await.unwrap_err();
    assert_eq!(error.to_string(), "has run record(s): delete-run");

    let with_trigger = create_node(&store, "delete-with-trigger", None, TaskProfile::Manager).await;
    let triggers_dir = store.session_dir(&with_trigger.id).join("triggers");
    fs::create_dir_all(&triggers_dir).unwrap();
    fs::write(triggers_dir.join("trigger-a.json"), "{}\n").unwrap();
    let error = store.delete_permanently(&with_trigger.id, TaskCaller::Operator).await.unwrap_err();
    assert_eq!(error.to_string(), "has saved trigger reference(s)");

    let with_evidence = create_node(&store, "delete-with-evidence", None, TaskProfile::Manager).await;
    append(&store, &with_evidence.id, json!({"type": "user", "content": "preserve this"})).await;
    let error = store.delete_permanently(&with_evidence.id, TaskCaller::Operator).await.unwrap_err();
    assert_eq!(error.to_string(), "has preserved conversation/evidence: user");

    for (request_id, reference_kind) in [
      ("delete-parent-reference", "parent_session_id"),
      ("delete-successor-reference", "successor_session_id"),
      ("delete-origin-reference", "origin_ref"),
      ("delete-created-reference", "created_by_event"),
    ] {
      let target = create_node(&store, request_id, None, TaskProfile::Manager).await;
      let reference = create_node(&store, &format!("{request_id}-other"), None, TaskProfile::Manager).await;
      let _guard = store.lock_for(&reference.id).await;
      let mut meta = store.read_metadata_fresh(&reference.id).unwrap().unwrap();
      match reference_kind {
        "parent_session_id" => meta.parent_session_id = Some(target.id.clone()),
        "successor_session_id" => meta.successor_session_id = Some(target.id.clone()),
        "origin_ref" => {
          meta.origin_ref =
            Some(EventRef { session_id: target.id.clone(), event_id: Some("origin-event".to_string()) });
        }
        "created_by_event" => {
          meta.created_by_event =
            Some(EventRef { session_id: target.id.clone(), event_id: Some("created-event".to_string()) });
        }
        _ => unreachable!(),
      }
      store.save_metadata_locked(&mut meta, false).unwrap();
      drop(_guard);

      let error = store.delete_permanently(&target.id, TaskCaller::Operator).await.unwrap_err();
      assert_eq!(error.to_string(), format!("referenced by task {} ({reference_kind})", reference.id));
    }

    let report_target = create_node(&store, "delete-report-target", None, TaskProfile::Manager).await;
    let reporter = create_node(&store, "delete-report-source", None, TaskProfile::Manager).await;
    append(
      &store,
      &reporter.id,
      json!({
        "type": "child_report",
        "child_session_id": report_target.id,
        "child_event_id": "reported-event",
      }),
    )
    .await;
    let error = store.delete_permanently(&report_target.id, TaskCaller::Operator).await.unwrap_err();
    assert_eq!(error.to_string(), format!("referenced by a child report in task {}", reporter.id));
  }

  #[tokio::test]
  async fn delete_permanently_is_operator_only_reports_missing_tasks_and_removes_an_empty_task() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let root = create_node(&store, "delete-empty", None, TaskProfile::Manager).await;

    let error =
      store.delete_permanently(&root.id, TaskCaller::Agent { session_id: root.id.clone() }).await.unwrap_err();
    assert_eq!(error.to_string(), "permanent delete requires operator credentials");
    assert_eq!(error.status_code(), Some(403));
    let error = store.delete_permanently("missing-task", TaskCaller::Operator).await.unwrap_err();
    assert_eq!(error.to_string(), "task missing-task not found");
    assert_eq!(error.status_code(), Some(404));

    assert!(store.delete_permanently(&root.id, TaskCaller::Operator).await.unwrap());
    assert!(!store.session_dir(&root.id).exists());
    assert!(store.read_metadata_fresh(&root.id).unwrap().is_none());
  }

  #[tokio::test]
  async fn agent_create_refusals_are_forbidden_with_python_text() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let root = create_node(&store, "agent-root", None, TaskProfile::Manager).await;
    let other = create_node(&store, "other-root", None, TaskProfile::Manager).await;
    let caller = TaskCaller::Agent { session_id: root.id.clone() };

    // `task_sessions.AGENT_CREATE_SCOPE_REFUSAL`: a create outside the caller's own manager task.
    let foreign = CreateTask {
      request_id: "foreign-child".to_string(),
      task_parent_id: Some(other.id.clone()),
      profile: TaskProfile::Manager,
      task: None,
      name: None,
      backend: Some("synthetic".to_string()),
      group: None,
      session_id: None,
    };
    let error = store.create_task(foreign, caller.clone(), Map::new()).await.unwrap_err();
    assert_eq!(error.to_string(), "an agent may only create a task directly under its own open manager task");
    assert_eq!(error.status_code(), Some(403));

    // `takeoff_gate.DelegationBlockedError`: an implementation worker with no user request on its chain.
    let gated = CreateTask {
      request_id: "gated-worker".to_string(),
      task_parent_id: Some(root.id.clone()),
      profile: TaskProfile::Worker,
      task: None,
      name: None,
      backend: Some("synthetic".to_string()),
      group: None,
      session_id: None,
    };
    let error = store.create_task(gated, caller, Map::new()).await.unwrap_err();
    assert!(error.to_string().starts_with("Delegation blocked: no user request on the task chain"));
    assert_eq!(error.status_code(), Some(403));
  }

  #[tokio::test]
  async fn create_under_an_archived_parent_is_a_conflict_naming_the_parent() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let root = create_node(&store, "root", None, TaskProfile::Manager).await;
    store.archive_subtree(&root.id, TaskCaller::Operator).await.unwrap();

    // `task_sessions.create_task`: a closed parent is a `TaskConflictError` whose one blocker names the state.
    let late = CreateTask {
      request_id: "late-child".to_string(),
      task_parent_id: Some(root.id.clone()),
      profile: TaskProfile::Worker,
      task: None,
      name: None,
      backend: Some("synthetic".to_string()),
      group: None,
      session_id: None,
    };
    let error = store.create_task(late, TaskCaller::System, Map::new()).await.unwrap_err();

    let blocker = format!("parent task {} is archived", root.id);
    assert_eq!(error.to_string(), blocker);
    assert_eq!(error.status_code(), Some(409));
    assert_eq!(error.blockers(), Some(std::slice::from_ref(&blocker)));
  }

  #[test]
  fn archived_refusal_is_python_s_plain_sentence_without_blockers() {
    // `task_errors.TaskArchivedError`, raised for machine input to an archived node such as a report to an
    // archived parent: the 409 detail is this sentence alone.
    let error = StoreError::archived("node-a");

    assert_eq!(error.to_string(), "task node-a is archived");
    assert_eq!(error.status_code(), Some(409));
    assert_eq!(error.blockers(), None);
  }

  #[tokio::test]
  async fn invalid_create_requests_are_400_with_python_text() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let worker = create_node(&store, "worker", None, TaskProfile::Worker).await;

    let empty_id = CreateTask {
      request_id: String::new(),
      task_parent_id: None,
      profile: TaskProfile::Manager,
      task: None,
      name: None,
      backend: Some("synthetic".to_string()),
      group: None,
      session_id: None,
    };
    let error = store.create_task(empty_id, TaskCaller::System, Map::new()).await.unwrap_err();
    assert_eq!(error.to_string(), "request_id is required for task creation");
    assert_eq!(error.status_code(), Some(400));

    // `TaskInvalidError("parent task <id> is not a manager")`: a worker cannot parent a task.
    let under_worker = CreateTask {
      request_id: "under-worker".to_string(),
      task_parent_id: Some(worker.id.clone()),
      profile: TaskProfile::Worker,
      task: None,
      name: None,
      backend: Some("synthetic".to_string()),
      group: None,
      session_id: None,
    };
    let error = store.create_task(under_worker, TaskCaller::System, Map::new()).await.unwrap_err();
    assert_eq!(error.to_string(), format!("parent task {} is not a manager", worker.id));
    assert_eq!(error.status_code(), Some(400));
  }

  #[tokio::test]
  async fn absent_task_is_a_404_with_python_text() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());

    let error = store.load_task_meta("missing").await.unwrap_err();

    assert_eq!(error.to_string(), "task missing not found");
    assert_eq!(error.status_code(), Some(404));
  }

  async fn next_sidebar_frame(sidebar: &mut Subscription) -> Value {
    let frame = tokio::time::timeout(Duration::from_secs(5), sidebar.recv())
      .await
      .expect("a sidebar frame arrives")
      .expect("the sidebar channel stays open");
    serde_json::from_str(&frame).unwrap()
  }

  #[tokio::test]
  async fn control_facts_and_task_creation_notify_the_sidebar_with_the_fact_type() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    let mut sidebar = store.subscribe(SIDEBAR_CHANNEL);
    let root = create_node(&store, "root", None, TaskProfile::Manager).await;
    let child = create_node(&store, "child", Some(&root.id), TaskProfile::Worker).await;
    for node in [&root.id, &child.id] {
      assert_eq!(
        next_sidebar_frame(&mut sidebar).await,
        json!({"type": "task_tree_changed", "session_id": node, "fact_type": "task_created"})
      );
    }

    store.archive_subtree(&root.id, TaskCaller::Operator).await.unwrap();

    for node in [&root.id, &child.id] {
      assert_eq!(
        next_sidebar_frame(&mut sidebar).await,
        json!({"type": "task_tree_changed", "session_id": node, "fact_type": "task_closed"})
      );
    }
  }

  #[tokio::test]
  async fn unnamed_sessions_follow_the_counter_and_recover_from_the_directory_count() {
    let home = tempfile::tempdir().unwrap();
    let store = make_store(home.path());
    create_node(&store, "named-a", None, TaskProfile::Manager).await;
    create_node(&store, "named-b", None, TaskProfile::Manager).await;
    let counter = home.path().join("sessions").join(".counter");

    fs::write(&counter, "not-a-number").unwrap();
    assert_eq!(create_unnamed_node(&store, "unnamed-a").await.name, "Session 2");
    assert_eq!(fs::read_to_string(&counter).unwrap(), "3");

    fs::write(&counter, " 7\n").unwrap();
    assert_eq!(create_unnamed_node(&store, "unnamed-b").await.name, "Session 7");
    assert_eq!(fs::read_to_string(&counter).unwrap(), "8");
  }

  #[test]
  fn fact_fold_tracks_boundaries_closes_reports_and_run_positions() {
    let events = vec![
      json!({"id": "created", "type": "task_created"}),
      json!({"id": "input-a", "type": "user", "content": "first"}),
      json!({"id": "imported", "type": "task_imported", "pending_inputs": [{"input_id": "input-a"}]}),
      json!({"id": "closed", "type": "task_closed", "outcome": "cancelled"}),
      json!({"id": "input-b", "type": "user", "content": "during close"}),
      json!({"id": "reopened", "type": "task_reopened"}),
      json!({"id": "input-c", "type": "user", "content": "after reopen"}),
      json!({"id": "finished", "type": "run_finished", "run_id": "run-a", "outcome": "success", "input_event_ids": ["input-c"]}),
      json!({"id": "close-request", "type": "task_close_requested"}),
      json!({"id": "report", "type": "child_report", "child_session_id": "child-a", "child_event_id": "event-a"}),
      json!({"id": "report", "type": "child_report", "child_session_id": "child-a", "child_event_id": "duplicate"}),
    ];
    let facts = fold_task_facts(&events).unwrap();
    assert_eq!(facts.task_state, "open");
    assert_eq!(
      facts.input_candidates.iter().map(|event| event["id"].as_str().unwrap()).collect::<Vec<_>>(),
      ["input-c", "report"]
    );
    assert_eq!(facts.confirmed_input_ids, HashSet::from(["input-c".to_string()]));
    assert_eq!(facts.run_outcomes["run-a"], "success");
    assert_eq!(facts.run_finish_positions["run-a"], 7);
    assert_eq!(facts.close_events.len(), 1);
    assert_eq!(facts.close_requests.len(), 1);
    assert_eq!(facts.delivered_reports, HashSet::from([("child-a".to_string(), "event-a".to_string())]));
    assert_eq!(facts.covered, events.len());
  }

  #[test]
  fn imported_pending_inputs_require_stable_input_ids() {
    let error =
      fold_task_facts(&[json!({"id": "bad-import", "type": "task_imported", "pending_inputs": [{}]})]).unwrap_err();
    assert!(error.to_string().contains("without a stable input_id"));
  }

  #[test]
  fn task_spec_hash_matches_python_canonical_text() {
    let implement = TaskSpec {
      goal: "Ship the é change".to_string(),
      acceptance: vec!["tests pass".to_string()],
      context_refs: vec!["docs/a.md".to_string()],
      repo_path: Some("/home/synthetic/repo".to_string()),
      base_branch: Some("main".to_string()),
      task_type: Some(TaskType::Implement),
      keep_worktree: true,
      cold_read: None,
    };
    let text = serde_json::to_string(&implement).unwrap();
    assert_eq!(
      text,
      r#"{"goal":"Ship the é change","acceptance":["tests pass"],"context_refs":["docs/a.md"],"repo_path":"/home/synthetic/repo","base_branch":"main","task_type":"implement","keep_worktree":true,"cold_read":null}"#
    );
    assert_eq!(task_spec_hash(&implement).unwrap(), "c5ddb0177e12fd9b6e51707f3c41a0285c830bff33280ac66ebd0121cdd3a7c1");

    let verify = TaskSpec { goal: "check".to_string(), task_type: Some(TaskType::Verify), ..TaskSpec::default() };
    assert_eq!(
      serde_json::to_string(&verify).unwrap(),
      r#"{"goal":"check","acceptance":[],"context_refs":[],"repo_path":null,"base_branch":null,"task_type":"verify","keep_worktree":false,"cold_read":null}"#
    );
    assert_eq!(task_spec_hash(&verify).unwrap(), "9e765c73c0cf10b2fbaef99cb808d77a04b4451cc5924cdaa17b7ddf4c6f106b");
  }
}
