//! Durable task input admission, claims, finishes, and child reports.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use charliebot_core::event_types;
use charliebot_registry::caller::CallerIdentity;
use charliebot_registry::context::{FeatureContext, StateError};
use charliebot_registry::sequence::{SequenceCompletion, SequenceDispatch};
use charliebot_store::metadata::{SessionStatus, TaskProfile, UtcDateTime};
use charliebot_store::run_records::{RunKind, RunOutcome, RunOutcomeKind, RunRecord};
use charliebot_store::session_fork::CloseReportRedelivery;
use charliebot_store::{
  ControlGuard, SharedChatEvent, Store, StoreError, StoreHookFuture, StoreResult, StructuralBlockers,
};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};

const INPUT_EVENT_TYPES: [&str; 4] =
  [event_types::USER, event_types::AGENT_MESSAGE, event_types::SCHEDULED_TRIGGER, event_types::CHILD_REPORT];
const ADMIT_FIELDS: [&str; 8] = [
  "input_id",
  "request_id",
  "uploaded_files",
  "timestamp",
  "from_session",
  "from_session_name",
  "input_mode",
  "fields",
];

/// The executor that reserves a run and launches the claimed input batch.
pub trait DispatchExecutor: Send + Sync {
  fn launch<'a>(
    &'a self,
    session_id: &'a str,
    pending_inputs: &'a [Value],
    launch_run_id: Option<&'a str>,
  ) -> BoxFuture<'a, StoreResult<Option<String>>>;
}

/// The completion operation that follows a successful durable run finish.
pub trait RunFinishedCompletion: Send + Sync {
  fn after_run_finished<'a>(&'a self, session_id: &'a str, run_id: &'a str) -> BoxFuture<'a, StoreResult<()>>;
}

/// The task input, run claim, and child report owner over the shared store.
pub struct Dispatcher {
  store: Arc<Store>,
  features: Arc<FeatureContext>,
  sequence_completion: Arc<dyn SequenceCompletion>,
  run_completion: Arc<dyn RunFinishedCompletion>,
  executor: RwLock<Option<Arc<dyn DispatchExecutor>>>,
}

impl Dispatcher {
  /// Build a dispatcher whose executor can be installed by the execution stage.
  pub fn new(
    store: Arc<Store>,
    features: Arc<FeatureContext>,
    sequence_completion: Arc<dyn SequenceCompletion>,
    run_completion: Arc<dyn RunFinishedCompletion>,
  ) -> Dispatcher {
    Dispatcher { store, features, sequence_completion, run_completion, executor: RwLock::new(None) }
  }

  /// Install the execution adapter used by later dispatch decisions.
  pub fn set_executor(&self, executor: Option<Arc<dyn DispatchExecutor>>) {
    *self.executor.write().expect("dispatcher executor lock is not poisoned") = executor;
  }

  /// Persist one task input, replaying an existing event with the same id.
  pub async fn admit_input(
    &self,
    session_id: &str,
    event_type: &str,
    content: &str,
    actor: &str,
    fields: Value,
  ) -> StoreResult<Value> {
    if !INPUT_EVENT_TYPES.contains(&event_type) {
      return Err(StoreError::invalid(format!("{event_type} is not a task input type")));
    }
    let fields_object = fields.as_object().ok_or_else(|| StoreError::invalid("task input fields must be an object"))?;
    if fields_object.get("input_mode").is_some_and(|value| !value.is_null()) && event_type != event_types::USER {
      return Err(StoreError::invalid(format!("input_mode applies to a user message, not to {event_type}")));
    }
    if event_type == event_types::USER && actor != "user" {
      return Err(StoreError::forbidden("only verified operator input is a real user message"));
    }
    if event_type == event_types::SCHEDULED_TRIGGER && actor != "system" {
      return Err(StoreError::forbidden("only the server's own scheduler mints scheduled triggers"));
    }
    let fields = admit_fields(&fields)?;
    let collisions: Vec<_> = fields
      .extra_fields
      .keys()
      .filter(|key| charliebot_store::control_events::REQUEST_EVENT_CORE_FIELDS.contains(&key.as_str()))
      .cloned()
      .collect();
    if !collisions.is_empty() {
      return Err(StoreError::invalid(format!("fields may not replace core event fields: {}", collisions.join(", "))));
    }
    if fields.input_id.is_some() && fields.request_id.is_some() {
      return Err(StoreError::invalid("input_id and request_id are mutually exclusive"));
    }

    let epoch = self.store.prime_aggregator(session_id).await?;
    let mut restore_announcements = Vec::new();
    let event = {
      let guard = self.store.control_lock().await;
      let meta = self.store.load_task_meta(session_id).await?;
      let state = self.store.node_state(session_id)?;
      let task_state = state.task_state()?;
      if task_state != "open" && event_type != event_types::USER {
        return Err(StoreError::archived(session_id));
      }
      self.authorize_agent_message(&meta, event_type, fields.from_session.as_deref())?;
      let mut event_id = fields.input_id.clone();
      if let Some(request_id) = fields.request_id.as_deref() {
        let stable_id = charliebot_store::control_events::stable_request_event_id(session_id, request_id);
        let events = self.store.load_all_facts_view_sync(session_id)?;
        let extra_fields = Value::Object(fields.extra_fields.clone());
        if let Some(existing) = charliebot_store::control_events::replay_request_event(
          events.iter(),
          &stable_id,
          request_id,
          event_type,
          content,
          Some(&extra_fields),
        )? {
          return Ok(existing);
        }
        event_id = Some(stable_id);
      } else if let Some(input_id) = fields.input_id.as_deref() {
        let events = self.store.load_all_facts_view_sync(session_id)?;
        if let Some(existing) = events.iter().find(|event| event.get("id").and_then(Value::as_str) == Some(input_id)) {
          return Ok(existing.clone());
        }
      }
      let mut payload = vec![("content".to_string(), Value::String(content.to_string()))];
      if let Some(request_id) = fields.request_id.as_deref() {
        payload.push(("request_id".to_string(), Value::String(request_id.to_string())));
      }
      if let Some(uploaded_files) = fields.uploaded_files.as_ref().filter(|files| !files.is_empty()) {
        payload.push(("uploaded_files".to_string(), Value::Array(uploaded_files.clone())));
      }
      if let Some(from_session) = fields.from_session.as_deref() {
        payload.push(("from_session".to_string(), Value::String(from_session.to_string())));
      }
      if let Some(from_session_name) = fields.from_session_name.as_deref() {
        payload.push(("from_session_name".to_string(), Value::String(from_session_name.to_string())));
      }
      if let Some(input_mode) = fields.input_mode.as_deref() {
        payload.push(("input_mode".to_string(), Value::String(input_mode.to_string())));
      }
      payload.extend(fields.extra_fields.clone());
      let mut event = charliebot_store::control_events::build_control_event(
        event_type,
        actor,
        session_id,
        event_id.as_deref(),
        None,
        payload,
      );
      if let Some(timestamp) = fields.timestamp.as_deref() {
        event["timestamp"] = Value::String(timestamp.to_string());
      }
      if task_state != "open" {
        let request_id =
          event.get("id").and_then(Value::as_str).ok_or_else(|| StoreError::new("admitted input event has no id"))?;
        let (_restored, announcements) =
          self.sequence_completion.restore_chain_locked(&guard, session_id, request_id, "user message").await?;
        restore_announcements = announcements;
      }
      let event = self.store.append_control_fact(&guard, session_id, event).await?;
      if event_type == event_types::USER {
        self.lift_user_message_branch(session_id, &event).await?;
      }
      event
    };
    for (node_id, reopened, reopen_epoch) in restore_announcements {
      self.store.announce_appended_event(&node_id, &reopened, reopen_epoch).await?;
    }
    self.store.announce_appended_event(session_id, &event, epoch).await?;
    Ok(event)
  }

  fn authorize_agent_message(
    &self,
    meta: &charliebot_store::metadata::SessionMetadata,
    event_type: &str,
    from_session: Option<&str>,
  ) -> StoreResult<()> {
    if event_type != event_types::AGENT_MESSAGE || meta.profile != TaskProfile::Worker {
      return Ok(());
    }
    if charliebot_store::is_read_only_exempt(meta.task.as_ref()) {
      return Ok(());
    }
    if from_session != meta.task_parent_id.as_deref() {
      return Err(StoreError::forbidden(format!(
        "agent messages to worker task {} must come from its parent task {}; this message names sender {}",
        meta.id,
        meta.task_parent_id.as_deref().unwrap_or("None"),
        from_session.unwrap_or("None")
      )));
    }
    self.store.check_task_authorization(from_session.unwrap_or(""))?;
    Ok(())
  }

  async fn lift_user_message_branch(&self, session_id: &str, event: &Value) -> StoreResult<()> {
    let timestamp = event
      .get("timestamp")
      .and_then(Value::as_str)
      .ok_or_else(|| StoreError::new("user input event has no timestamp"))?;
    let when = UtcDateTime::parse(timestamp).map_err(|error| StoreError::new(error.to_string()))?;
    let mut chain = vec![session_id.to_string()];
    for ancestor_id in self.store.ancestor_ids(session_id)? {
      let ancestor = self.store.load_task_meta(&ancestor_id).await?;
      if ancestor.status == SessionStatus::Archived || self.store.archived_of(&ancestor_id)? {
        break;
      }
      chain.push(ancestor_id);
    }
    for node_id in chain {
      let current = self
        .store
        .get_session(&node_id)
        .await?
        .ok_or_else(|| StoreError::not_found(format!("task {node_id} not found")))?;
      if when > current.updated_at {
        self.store.update_thinking_state(&node_id, when).await?;
      }
    }
    self.store.invalidate_tree_index();
    Ok(())
  }

  /// Return the task's currently pending input facts from the durable boundary and run records.
  pub fn pending_inputs(&self, session_id: &str) -> StoreResult<Vec<Value>> {
    let facts = self.store.node_state(session_id)?;
    let task = facts.task()?;
    if task.boundary_index.is_none() && task.imported_pending_ids.is_empty() {
      return Err(StoreError::invalid(format!(
        "task {session_id} has no creation or import boundary; it is not a task-tree node"
      )));
    }
    // Claimed inputs only remove candidates, so with none the run listing can matter only through an unknown outcome.
    if task.input_candidates.is_empty() && !facts.runs.has_unknown_outcome() {
      return Ok(Vec::new());
    }
    let run_records = self.store.list_run_records_sync(session_id)?;
    let mut claimed = HashSet::new();
    for run in run_records {
      let launched = run.pid.is_some() || facts.runs.terminal_outcome(&run.id)?.is_some();
      if !launched && facts.runs.stop_requested(&run.id, None) {
        continue;
      }
      claimed.extend(run.input_event_ids);
    }
    Ok(deduplicate_pending_inputs(task.input_candidates.clone(), &task.confirmed_input_ids, &claimed))
  }

  /// Return the structural blocker for this task's pending inputs.
  pub fn pending_input_blockers(&self, session_id: &str) -> StoreResult<Vec<String>> {
    let pending = self.pending_inputs(session_id)?;
    if pending.is_empty() {
      return Ok(Vec::new());
    }
    Ok(vec![unprocessed_input_blocker(&pending)])
  }

  /// Bind an exact pending batch to a queued run while the caller holds the control lock.
  pub fn claim_input_batch_locked(
    &self,
    guard: &ControlGuard,
    session_id: &str,
    run_id: &str,
    input_ids: Option<&[String]>,
  ) -> StoreResult<Vec<String>> {
    let run = self
      .store
      .read_run_sync(session_id, run_id)?
      .ok_or_else(|| StoreError::not_found(format!("run {run_id} not found in session {session_id}")))?;
    let state = self.store.node_state(session_id)?;
    if state.runs.terminal_outcome(run_id)?.is_some() {
      return Err(StoreError::conflict(vec![format!("run {run_id} already finished; it claims no new inputs")]));
    }
    if run.pid.is_some() {
      return Err(StoreError::conflict(vec![format!("run {run_id} already launched; it owns its bound batch")]));
    }
    if state.runs.stop_requested(run_id, None) {
      return Err(StoreError::conflict(vec![format!("run {run_id} has a durable stop request; it never launches")]));
    }
    let pending_ids =
      self.pending_inputs(session_id)?.iter().map(|event| python_string(event.get("id"))).collect::<Vec<_>>();
    let batch = match input_ids {
      None => pending_ids,
      Some(input_ids) => {
        let unknown = input_ids.iter().filter(|id| !pending_ids.contains(id)).cloned().collect::<Vec<_>>();
        if !unknown.is_empty() {
          return Err(StoreError::conflict(vec![inputs_not_pending_conflict(session_id, &unknown)]));
        }
        input_ids.to_vec()
      }
    };
    self.store.claim_input_events_locked(guard, session_id, run_id, &batch)?;
    Ok(batch)
  }

  /// Append the still-pending USER events that this launched manager Run echoed (`claim_echoed_user_inputs`).
  pub async fn claim_echoed_user_inputs(
    &self,
    session_id: &str,
    run_id: &str,
    echoed_ids: &HashSet<String>,
  ) -> StoreResult<Vec<String>> {
    todo!("the echoed-input claim lands with the feature");

  }

  /// Decide whether pending inputs should start a run, and ask the executor when one is available.
  pub async fn dispatch_pending(&self, session_id: &str) -> StoreResult<Value> {
    let meta = self.store.load_task_meta(session_id).await?;
    let pending = self.pending_inputs(session_id)?;
    let mut decision = Map::from_iter([
      ("session_id".to_string(), Value::String(session_id.to_string())),
      ("pending".to_string(), json!(pending.len())),
    ]);
    if self.store.node_facts(session_id)?.task()?.task_state != "open" {
      decision.insert("launch".to_string(), Value::Bool(false));
      decision.insert("reason".to_string(), json!("task is closed; input retained as history"));
      return Ok(Value::Object(decision));
    }
    let queued = {
      let facts = self.store.node_state(session_id)?;
      let mut queued = Vec::new();
      for run in self.store.list_run_records_sync(session_id)? {
        if facts.runs.terminal_outcome(&run.id)?.is_some() {
          continue;
        }
        if run.pid.is_some() {
          crate::midturn_delivery::offer(&self.features, &meta, &run, &pending)
            .await
            .map_err(|error| StoreError::new(error.to_string()))?;
          decision.insert("launch".to_string(), Value::Bool(false));
          decision.insert("reason".to_string(), json!(format!("run {} already consumes this node's inputs", run.id)));
          return Ok(Value::Object(decision));
        }
        if facts.runs.stop_requested(&run.id, None) {
          continue;
        }
        if run.sequence_ref.is_some() {
          continue;
        }
        if run.kind == RunKind::ManagerTurn && run.input_event_ids.is_empty() && pending.is_empty() {
          continue;
        }
        queued.push(run);
      }
      queued
    };
    let executor = self.executor.read().expect("dispatcher executor lock is not poisoned").clone();
    if let Some(run) = queued.first() {
      let Some(executor) = executor else {
        return Ok(self.executor_pending(decision, session_id, pending.len()));
      };
      let launched = executor.launch(session_id, &pending, Some(&run.id)).await?;
      let Some(run_id) = launched else {
        decision.insert("launch".to_string(), Value::Bool(false));
        decision.insert("reason".to_string(), json!(format!("queued run {} was not launched by this call", run.id)));
        return Ok(Value::Object(decision));
      };
      decision.insert("launch".to_string(), Value::Bool(true));
      decision.insert("run_id".to_string(), Value::String(run_id));
      return Ok(Value::Object(decision));
    }
    if pending.is_empty() {
      decision.insert("launch".to_string(), Value::Bool(false));
      decision.insert("reason".to_string(), json!("no pending inputs"));
      return Ok(Value::Object(decision));
    }
    let Some(executor) = executor else {
      return Ok(self.executor_pending(decision, session_id, pending.len()));
    };
    let launched = executor.launch(session_id, &pending, None).await?;
    let Some(run_id) = launched else {
      decision.insert("launch".to_string(), Value::Bool(false));
      decision.insert("reason".to_string(), json!("another dispatch reserved this batch"));
      return Ok(Value::Object(decision));
    };
    decision.insert("launch".to_string(), Value::Bool(true));
    decision.insert("run_id".to_string(), Value::String(run_id));
    Ok(Value::Object(decision))
  }

  fn executor_pending(&self, mut decision: Map<String, Value>, session_id: &str, pending: usize) -> Value {
    decision.insert("launch".to_string(), Value::Bool(false));
    decision.insert("reason".to_string(), json!("execution adapters register in the next stage"));
    tracing::info!(target: "charliebot_tasks", session_id, pending, "task_input_executor_pending");
    Value::Object(decision)
  }

  /// Dispatch the parent's newly delivered report through its durable input queue.
  pub async fn wake_parent(&self, parent_id: &str, _report: &Value) -> StoreResult<bool> {
    self.dispatch_pending(parent_id).await?;
    Ok(false)
  }

  /// Persist a run's terminal fact, then run its liveness and successful completion follow-ups.
  pub async fn finish_run(&self, session_id: &str, run_id: &str, outcome: RunOutcome) -> StoreResult<RunRecord> {
    if !outcome.input_event_ids.is_empty()
      && let Some(run) = self.store.read_run_sync(session_id, run_id)?
      && run.input_event_ids.is_empty()
    {
      let pending =
        self.pending_inputs(session_id)?.iter().map(|event| python_string(event.get("id"))).collect::<HashSet<_>>();
      let foreign = outcome
        .input_event_ids
        .iter()
        .filter(|input_id| !pending.contains(input_id.as_str()))
        .cloned()
        .collect::<Vec<_>>();
      if !foreign.is_empty() {
        return Err(StoreError::conflict(vec![inputs_not_pending_conflict(session_id, &foreign)]));
      }
    }
    let run = {
      let guard = self.store.control_lock().await;
      self.store.record_finish_locked(&guard, session_id, run_id, outcome).await?
    };
    note_run_liveness(&self.store, session_id, &run, false).await;
    let terminal = self.store.node_state(session_id)?.runs.terminal_outcome(run_id)?;
    if terminal == Some(RunOutcomeKind::Success) {
      match self.run_completion.after_run_finished(session_id, run_id).await {
        Err(StoreError::Conflict(blockers)) => {
          tracing::info!(target: "charliebot_tasks", session_id, run_id, blockers = ?blockers, "post_finish_completion_blocked");
        }
        Err(error) => return Err(error),
        Ok(()) => {}
      }
    }
    Ok(run)
  }

  /// Return the newest durable finish or creation fact that can source a child report.
  pub fn report_source_event(&self, child_session_id: &str, child_kind: &str) -> StoreResult<Value> {
    let events = self.store.load_all_facts_view_sync(child_session_id)?;
    events
      .iter()
      .rev()
      .find(|event| {
        matches!(event.get("type").and_then(Value::as_str), Some(event_types::RUN_FINISHED | event_types::TASK_CREATED))
      })
      .cloned()
      .ok_or_else(|| {
        StoreError::new(format!("{child_kind} {child_session_id} has no durable fact to source its report from"))
      })
  }

  /// Return the fork or elone provenance used by one child report header.
  pub async fn child_report_provenance(&self, child_session_id: &str) -> StoreResult<Option<String>> {
    let Some(child) = self.store.load_meta(child_session_id).await? else { return Ok(None) };
    let Some(parent_session_id) = child.parent_session_id.as_deref() else { return Ok(None) };
    let source = self.store.load_meta(parent_session_id).await?;
    if source.as_ref().is_some_and(|source| source.successor_session_id.as_deref() == Some(child.id.as_str())) {
      Ok(Some(format!("elone of {parent_session_id}")))
    } else {
      Ok(Some(format!("clone of {parent_session_id}")))
    }
  }

  /// Return report provenance once per distinct child in the input batch.
  pub async fn child_report_provenances(&self, events: &[Value]) -> StoreResult<Map<String, Value>> {
    let mut provenance = Map::new();
    for event in events {
      if event.get("type").and_then(Value::as_str) != Some(event_types::CHILD_REPORT) {
        continue;
      }
      let child_id = python_string(event.get("child_session_id"));
      if provenance.contains_key(&child_id) {
        continue;
      }
      if let Some(fragment) = self.child_report_provenance(&child_id).await? {
        provenance.insert(child_id, Value::String(fragment));
      }
    }
    Ok(provenance)
  }

  /// Persist and announce a report to its fixed recipient, then dispatch that recipient.
  pub async fn deliver_child_report(
    &self,
    child_session_id: &str,
    source_event: Value,
    outcome: &str,
    summary: &str,
    result_refs: Option<&[String]>,
    recipient: Option<&str>,
  ) -> StoreResult<(Value, bool)> {
    self
      .deliver_child_report_as(ChildReportInput {
        child_session_id,
        source_event,
        outcome,
        summary,
        result_refs,
        recipient,
        actor: "agent",
      })
      .await
  }

  /// Persist a report while the caller holds the shared control lock.
  #[allow(clippy::too_many_arguments)]
  pub async fn deliver_child_report_locked(
    &self,
    guard: &ControlGuard,
    child_session_id: &str,
    source_event: Value,
    outcome: &str,
    summary: &str,
    result_refs: Option<&[String]>,
    recipient: Option<&str>,
  ) -> StoreResult<(Value, bool)> {
    self
      .deliver_child_report_locked_as_actor(
        guard,
        child_session_id,
        source_event,
        outcome,
        summary,
        result_refs,
        recipient,
        "agent",
      )
      .await
  }

  /// Persist a report with an explicit actor while the caller holds the shared control lock.
  #[allow(clippy::too_many_arguments)]
  pub async fn deliver_child_report_locked_as_actor(
    &self,
    guard: &ControlGuard,
    child_session_id: &str,
    source_event: Value,
    outcome: &str,
    summary: &str,
    result_refs: Option<&[String]>,
    recipient: Option<&str>,
    actor: &str,
  ) -> StoreResult<(Value, bool)> {
    if recipient.is_none() {
      return Ok((Value::Object(Map::new()), false));
    }
    self
      .deliver_child_report_locked_as(
        guard,
        ChildReportInput { child_session_id, source_event, outcome, summary, result_refs, recipient, actor },
      )
      .await
  }

  async fn deliver_child_report_as(&self, report_input: ChildReportInput<'_>) -> StoreResult<(Value, bool)> {
    let Some(recipient) = report_input.recipient else { return Ok((Value::Null, false)) };
    let child_session_id = report_input.child_session_id;
    let epoch = self.store.prime_aggregator(recipient).await?;
    let (report, created) = {
      let guard = self.store.control_lock().await;
      let result = self.deliver_child_report_locked_as(&guard, report_input).await;
      drop(guard);
      result?
    };
    if created {
      self.store.announce_appended_event(recipient, &report, epoch).await?;
      if self.store.task_state(recipient)? != "open" {
        tracing::info!(
          target: "charliebot_tasks",
          parent = recipient,
          child = child_session_id,
          report = %python_string(report.get("id")),
          "child_report_into_archived_parent_kept_as_history"
        );
        return Ok((report, true));
      }
      self.wake_parent(recipient, &report).await?;
    }
    Ok((report, created))
  }

  async fn deliver_child_report_locked_as(
    &self,
    guard: &ControlGuard,
    report_input: ChildReportInput<'_>,
  ) -> StoreResult<(Value, bool)> {
    let ChildReportInput { child_session_id, source_event, outcome, summary, result_refs, recipient, actor } =
      report_input;
    let Some(recipient) = recipient else { return Ok((Value::Null, false)) };
    if self.store.load_meta(recipient).await?.is_none() {
      return Err(StoreError::not_found(format!("report recipient task {recipient} not found")));
    }
    let source_event_id = python_string(source_event.get("id"));
    let report_id =
      charliebot_store::control_events::stable_child_report_id(child_session_id, &source_event_id, recipient);
    let recipient_events = self.store.load_all_facts_view_sync(recipient)?;
    if let Some(existing) =
      recipient_events.iter().find(|event| event.get("id").and_then(Value::as_str) == Some(report_id.as_str()))
    {
      return Ok((existing.clone(), false));
    }
    let report = charliebot_store::control_events::build_control_event(
      event_types::CHILD_REPORT,
      actor,
      child_session_id,
      Some(&report_id),
      None,
      [
        ("child_session_id".to_string(), Value::String(child_session_id.to_string())),
        ("child_event_id".to_string(), Value::String(source_event_id)),
        ("outcome".to_string(), Value::String(outcome.to_string())),
        ("summary".to_string(), Value::String(summary.to_string())),
        ("result_refs".to_string(), json!(result_refs.unwrap_or(&[]))),
      ],
    );
    let report = self.store.append_control_fact(guard, recipient, report).await?;
    Ok((report, true))
  }

  /// Redeliver every close report absent from its recipient's durable history.
  pub async fn recover_pending_reports(&self, session_id: &str) -> StoreResult<Vec<Value>> {
    self.store.load_task_meta(session_id).await?;
    let mut delivered = Vec::new();
    for (close, recipient) in self.undelivered_close_recipients(session_id)? {
      let result_refs = close_result_refs(&close);
      let summary = truthy_string(close.get("summary"));
      let (report, created) = self
        .deliver_child_report_as(ChildReportInput {
          child_session_id: session_id,
          source_event: close.as_ref().clone(),
          outcome: close_report_outcome(&close),
          summary: &summary,
          result_refs: Some(&result_refs),
          recipient: Some(&recipient),
          actor: "system",
        })
        .await?;
      if created {
        delivered.push(report);
      }
    }
    Ok(delivered)
  }

  /// Redeliver every close report while the caller holds the shared control lock.
  pub async fn recover_pending_reports_locked(
    &self,
    guard: &ControlGuard,
    session_id: &str,
  ) -> StoreResult<Vec<Value>> {
    self.store.load_task_meta(session_id).await?;
    let mut delivered = Vec::new();
    for (close, recipient) in self.undelivered_close_recipients(session_id)? {
      let result_refs = close_result_refs(&close);
      let summary = truthy_string(close.get("summary"));
      let (report, created) = self
        .deliver_child_report_locked_as(
          guard,
          ChildReportInput {
            child_session_id: session_id,
            source_event: close.as_ref().clone(),
            outcome: close_report_outcome(&close),
            summary: &summary,
            result_refs: Some(&result_refs),
            recipient: Some(&recipient),
            actor: "system",
          },
        )
        .await?;
      if created {
        delivered.push(report);
      }
    }
    Ok(delivered)
  }

  /// Return one blocker for every close report still owed to its fixed recipient.
  pub fn undelivered_report_blockers(&self, session_id: &str) -> StoreResult<Vec<String>> {
    Ok(
      self
        .undelivered_close_recipients(session_id)?
        .iter()
        .map(|(close, _)| {
          format!("has an undelivered parent report for close event {}", python_string(close.get("id")))
        })
        .collect(),
    )
  }

  fn undelivered_close_recipients(&self, session_id: &str) -> StoreResult<Vec<(SharedChatEvent, String)>> {
    let close_events = self.store.node_facts(session_id)?.task()?.close_events.clone();
    let mut undelivered = Vec::new();
    let mut recipient_events = HashMap::new();
    for close in close_events {
      let recipient = close.get("report_to");
      if !python_truthy(recipient) {
        continue;
      }
      let recipient = python_string(recipient);
      let source_event_id = python_string(close.get("id"));
      let report_id =
        charliebot_store::control_events::stable_child_report_id(session_id, &source_event_id, &recipient);
      if !recipient_events.contains_key(&recipient) {
        recipient_events.insert(recipient.clone(), self.store.load_all_facts_view_sync(&recipient)?);
      }
      let events = recipient_events.get(&recipient).expect("recipient history is inserted before it is read");
      let already_delivered =
        events.iter().any(|event| event.get("id").and_then(Value::as_str) == Some(report_id.as_str()));
      if !already_delivered {
        undelivered.push((close, recipient));
      }
    }
    Ok(undelivered)
  }
}

impl StructuralBlockers for Dispatcher {
  fn pending_input_blockers(&self, session_id: &str) -> StoreResult<Vec<String>> {
    Dispatcher::pending_input_blockers(self, session_id)
  }

  fn undelivered_report_blockers(&self, session_id: &str) -> StoreResult<Vec<String>> {
    Dispatcher::undelivered_report_blockers(self, session_id)
  }
}

impl CloseReportRedelivery for Dispatcher {
  fn redeliver_close_reports_locked<'a>(
    &'a self,
    guard: &'a ControlGuard,
    session_id: &'a str,
  ) -> StoreHookFuture<'a, ()> {
    Box::pin(async move { Dispatcher::recover_pending_reports_locked(self, guard, session_id).await.map(|_| ()) })
  }
}

impl SequenceDispatch for Dispatcher {
  fn admit_input<'a>(
    &'a self,
    session_id: &'a str,
    event_type: &'a str,
    content: &'a str,
    actor: &'a str,
    fields: Value,
  ) -> BoxFuture<'a, StoreResult<Value>> {
    Box::pin(async move { Dispatcher::admit_input(self, session_id, event_type, content, actor, fields).await })
  }

  fn dispatch_pending<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, StoreResult<()>> {
    Box::pin(async move { Dispatcher::dispatch_pending(self, session_id).await.map(|_| ()) })
  }

  fn report_source_event(&self, child_session_id: &str, child_kind: &str) -> StoreResult<Value> {
    Dispatcher::report_source_event(self, child_session_id, child_kind)
  }

  fn deliver_child_report_locked<'a>(
    &'a self,
    guard: &'a ControlGuard,
    child_session_id: &'a str,
    source_event: Value,
    outcome: &'a str,
    summary: &'a str,
    result_refs: Option<&'a [String]>,
    recipient: Option<&'a str>,
  ) -> BoxFuture<'a, StoreResult<(Value, bool)>> {
    Box::pin(async move {
      Dispatcher::deliver_child_report_locked(
        self,
        guard,
        child_session_id,
        source_event,
        outcome,
        summary,
        result_refs,
        recipient,
      )
      .await
    })
  }

  fn deliver_child_report<'a>(
    &'a self,
    child_session_id: &'a str,
    source_event: Value,
    outcome: &'a str,
    summary: &'a str,
    result_refs: Option<&'a [String]>,
    recipient: Option<&'a str>,
  ) -> BoxFuture<'a, StoreResult<(Value, bool)>> {
    Box::pin(async move {
      Dispatcher::deliver_child_report(self, child_session_id, source_event, outcome, summary, result_refs, recipient)
        .await
    })
  }

  fn wake_parent<'a>(&'a self, parent_id: &'a str, report: Value) -> BoxFuture<'a, StoreResult<bool>> {
    Box::pin(async move { Dispatcher::wake_parent(self, parent_id, &report).await })
  }
}

/// Record or clear the worker busy interval associated with one run.
pub async fn note_run_liveness(store: &Store, session_id: &str, run: &RunRecord, launched: bool) {
  let result = note_run_liveness_inner(store, session_id, run, launched).await;
  if let Err(error) = result {
    tracing::error!(
      target: "charliebot_tasks",
      session_id,
      run_id = %run.id,
      launched,
      error = %error,
      "run_liveness_notify_failed"
    );
  }
}

async fn note_run_liveness_inner(store: &Store, session_id: &str, run: &RunRecord, launched: bool) -> StoreResult<()> {
  if !launched {
    store.clear_run_busy(session_id, &run.id);
    return Ok(());
  }
  if let Some(backend) = run.backend.as_deref() {
    store.note_run_backend(session_id, backend);
  }
  let Some(meta) = store.get_session(session_id).await? else { return Ok(()) };
  if meta.profile == TaskProfile::Worker {
    let started_at = run.started_at.unwrap_or_else(UtcDateTime::now);
    store.mark_run_busy(session_id, &run.id, started_at);
  }
  Ok(())
}

/// The task-tree blocker sentence for a non-empty pending batch.
pub fn unprocessed_input_blocker(pending: &[Value]) -> String {
  let ids = pending.iter().take(8).map(|event| python_string(event.get("id"))).collect::<Vec<_>>().join(", ");
  let more = if pending.len() <= 8 { String::new() } else { format!(" (+{} more)", pending.len() - 8) };
  format!("has unprocessed input: {ids}{more}")
}

/// The conflict sentence for input ids outside one task's pending set.
pub fn inputs_not_pending_conflict(session_id: &str, unknown: &[String]) -> String {
  format!("input(s) not pending for {session_id}: {}", unknown.join(", "))
}

/// Format a child report as the parent's typed turn input (`task_input_prompt.child_report_text` owns the format).
pub use crate::task_input_prompt::child_report_text;

/// Read the dispatcher inserted into typed startup state.
pub fn dispatcher(features: &FeatureContext) -> Result<Arc<Dispatcher>, StateError> {
  features.startup.require::<Dispatcher>()
}

/// Return the task input type assigned to a verified operator or agent caller.
pub fn input_event_type_for_caller(caller: &CallerIdentity) -> &'static str {
  match caller {
    CallerIdentity::Operator { .. } => event_types::USER,
    CallerIdentity::Agent { .. } => event_types::AGENT_MESSAGE,
  }
}

/// Return the source provenance for agent-relayed input, or no provenance for the operator.
pub fn agent_provenance(caller: &CallerIdentity) -> (Option<String>, Option<String>) {
  match caller {
    CallerIdentity::Operator { .. } => (None, None),
    CallerIdentity::Agent { claims } => (Some(claims.session_id.clone()), None),
  }
}

fn deduplicate_pending_inputs<E: AsRef<Value>>(
  candidates: Vec<E>,
  confirmed: &HashSet<String>,
  claimed: &HashSet<String>,
) -> Vec<Value> {
  let mut positions = HashMap::new();
  let mut pending = Vec::new();
  for candidate in candidates {
    let event = candidate.as_ref();
    let input_id = python_string(event.get("id"));
    if confirmed.contains(&input_id) || claimed.contains(&input_id) {
      continue;
    }
    if let Some(position) = positions.get(&input_id).copied() {
      pending[position] = event.clone();
    } else {
      positions.insert(input_id, pending.len());
      pending.push(event.clone());
    }
  }
  pending
}

struct AdmitFields {
  input_id: Option<String>,
  request_id: Option<String>,
  uploaded_files: Option<Vec<Value>>,
  timestamp: Option<String>,
  from_session: Option<String>,
  from_session_name: Option<String>,
  input_mode: Option<String>,
  extra_fields: Map<String, Value>,
}

struct ChildReportInput<'a> {
  child_session_id: &'a str,
  source_event: Value,
  outcome: &'a str,
  summary: &'a str,
  result_refs: Option<&'a [String]>,
  recipient: Option<&'a str>,
  actor: &'a str,
}

fn admit_fields(fields: &Value) -> StoreResult<AdmitFields> {
  let object = fields.as_object().ok_or_else(|| StoreError::invalid("task input fields must be an object"))?;
  if let Some(unknown) = object.keys().find(|key| !ADMIT_FIELDS.contains(&key.as_str())) {
    return Err(StoreError::invalid(format!("unexpected task input field {unknown}")));
  }
  let optional_string = |name: &str| -> StoreResult<Option<String>> {
    match object.get(name) {
      None | Some(Value::Null) => Ok(None),
      Some(Value::String(value)) => Ok(Some(value.clone())),
      Some(_) => Err(StoreError::invalid(format!("task input field {name} must be a string or null"))),
    }
  };
  let uploaded_files = match object.get("uploaded_files") {
    None | Some(Value::Null) => None,
    Some(Value::Array(files)) => Some(files.clone()),
    Some(_) => return Err(StoreError::invalid("task input field uploaded_files must be a list or null")),
  };
  let extra_fields = match object.get("fields") {
    None | Some(Value::Null) => Map::new(),
    Some(Value::Object(fields)) => fields.clone(),
    Some(_) => return Err(StoreError::invalid("task input field fields must be an object or null")),
  };
  Ok(AdmitFields {
    input_id: optional_string("input_id")?,
    request_id: optional_string("request_id")?,
    uploaded_files,
    timestamp: optional_string("timestamp")?,
    from_session: optional_string("from_session")?,
    from_session_name: optional_string("from_session_name")?,
    input_mode: optional_string("input_mode")?,
    extra_fields,
  })
}

fn close_report_outcome(close: &Value) -> &str {
  close
    .get("sequence_outcome")
    .and_then(Value::as_str)
    .filter(|value| !value.is_empty())
    .or_else(|| close.get("outcome").and_then(Value::as_str).filter(|value| !value.is_empty()))
    .unwrap_or("completed")
}

fn close_result_refs(close: &Value) -> Vec<String> {
  close
    .get("result_refs")
    .and_then(Value::as_array)
    .map(|values| values.iter().map(|value| python_string(Some(value))).collect())
    .unwrap_or_default()
}

pub(crate) fn truthy_string(value: Option<&Value>) -> String {
  if python_truthy(value) { python_string(value) } else { String::new() }
}

fn python_truthy(value: Option<&Value>) -> bool {
  match value {
    None | Some(Value::Null) => false,
    Some(Value::Bool(value)) => *value,
    Some(Value::Number(value)) => value.as_f64().is_some_and(|value| value != 0.0),
    Some(Value::String(value)) => !value.is_empty(),
    Some(Value::Array(value)) => !value.is_empty(),
    Some(Value::Object(value)) => !value.is_empty(),
  }
}

pub(crate) fn python_string(value: Option<&Value>) -> String {
  match value {
    None | Some(Value::Null) => "None".to_string(),
    Some(Value::String(value)) => value.clone(),
    Some(Value::Bool(true)) => "True".to_string(),
    Some(Value::Bool(false)) => "False".to_string(),
    Some(value) => value.to_string(),
  }
}

#[cfg(test)]
mod tests {
  use super::{
    Dispatcher, RunFinishedCompletion, agent_provenance, deduplicate_pending_inputs, input_event_type_for_caller,
  };
  use crate::completion::TaskCompletion;
  use crate::test_fixtures;
  use charliebot_core::event_types;
  use charliebot_core::run_token::RunTokenClaims;
  use charliebot_registry::caller::CallerIdentity;
  use charliebot_registry::context::FeatureContext;
  use charliebot_registry::sequence::SequenceCompletion;
  use charliebot_store::metadata::TaskProfile;
  use charliebot_store::run_records::{RunKind, RunRecord};
  use charliebot_store::{CreateTask, StoreError, TaskCaller};
  use serde_json::{Map, json};
  use std::collections::HashSet;
  use std::sync::Arc;

  async fn features() -> (tempfile::TempDir, Arc<FeatureContext>) {
    let home = tempfile::tempdir().unwrap();
    let features = test_fixtures::claude_features(&home.path().join("home"), &test_fixtures::claude_config(None));
    (home, features)
  }

  fn dispatcher(features: &Arc<FeatureContext>) -> Dispatcher {
    let completion = Arc::new(TaskCompletion::new(Arc::clone(features)));
    let sequence_completion: Arc<dyn SequenceCompletion> = completion.clone();
    let run_completion: Arc<dyn RunFinishedCompletion> = completion;
    Dispatcher::new(Arc::clone(&features.store), Arc::clone(features), sequence_completion, run_completion)
  }

  fn run_record(session_id: &str, run_id: &str) -> RunRecord {
    RunRecord {
      id: run_id.to_string(),
      session_id: session_id.to_string(),
      kind: RunKind::ManagerTurn,
      input_event_ids: Vec::new(),
      task_spec_hash: None,
      task_spec_ref: None,
      prompt_snapshot_ref: None,
      retry_of_run_id: None,
      review_of_run_id: None,
      backend: Some("fable".to_string()),
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

  #[test]
  fn pending_input_ids_keep_the_first_position_and_last_event() {
    let pending = deduplicate_pending_inputs(
      vec![
        Arc::new(json!({"id":"same","content":"first"})),
        Arc::new(json!({"id":"other","content":"other"})),
        Arc::new(json!({"id":"same","content":"last"})),
      ],
      &HashSet::new(),
      &HashSet::new(),
    );
    assert_eq!(pending, [json!({"id":"same","content":"last"}), json!({"id":"other","content":"other"})]);
  }

  #[test]
  fn caller_input_type_and_provenance_follow_the_verified_identity() {
    let operator = CallerIdentity::Operator { session_id: Some("synthetic-caller".to_string()) };
    assert_eq!(input_event_type_for_caller(&operator), event_types::USER);
    assert_eq!(agent_provenance(&operator), (None, None));

    let agent = CallerIdentity::Agent {
      claims: RunTokenClaims {
        session_id: "synthetic-agent".to_string(),
        run_id: "synthetic-run".to_string(),
        agent: "synthetic-manager".to_string(),
      },
    };
    assert_eq!(input_event_type_for_caller(&agent), event_types::AGENT_MESSAGE);
    assert_eq!(agent_provenance(&agent), (Some("synthetic-agent".to_string()), None));
  }

  #[tokio::test]
  async fn reservation_claim_uses_memoized_facts_without_owned_history_copy() {
    let (_home, features) = features().await;
    let meta =
      test_fixtures::manager_session(&features, "reservation-count", "fable", None, "claude-reservation").await;
    features.store.append_chat_event(&meta.id, json!({"id":"input-a","type":"user","content":"input"})).await.unwrap();
    let guard = features.store.control_lock().await;
    features.store.register_run_locked(&guard, &meta.id, run_record(&meta.id, "queued-a"), None).await.unwrap();
    let before = features.store.history_work();

    let claimed = dispatcher(&features).claim_input_batch_locked(&guard, &meta.id, "queued-a", None).unwrap();

    let after = features.store.history_work();
    assert_eq!(claimed, ["input-a"]);
    assert_eq!(after.owned_event_clones - before.owned_event_clones, 0);
    let node_state_folds = after.node_state_full_folds - before.node_state_full_folds;
    assert_eq!(node_state_folds, 1, "the terminal and stop checks share one node-state fold");
    assert_eq!(
      after.full_folds - before.full_folds - node_state_folds,
      0,
      "pending inputs reuse the node-state facts fold"
    );
  }

  #[tokio::test]
  async fn input_replay_search_uses_shared_history_without_owned_copy() {
    let (_home, features) = features().await;
    let meta =
      test_fixtures::manager_session(&features, "input-replay-count", "fable", None, "claude-input-replay").await;
    let existing = features
      .store
      .append_chat_event(&meta.id, json!({"id":"input-replay","type":"user","content":"same"}))
      .await
      .unwrap();
    features.store.node_state(&meta.id).unwrap();
    features.store.prime_aggregator(&meta.id).await.unwrap();
    let before = features.store.history_work();

    let replay = dispatcher(&features)
      .admit_input(&meta.id, event_types::USER, "ignored", "user", json!({"input_id":"input-replay"}))
      .await
      .unwrap();

    let after = features.store.history_work();
    assert_eq!(replay, existing);
    assert_eq!(after.owned_event_clones - before.owned_event_clones, 0);
    assert_eq!(after.event_history_views - before.event_history_views, 1);
  }

  #[tokio::test]
  async fn request_id_replay_returns_one_event_and_conflicts_on_changed_payload() {
    let (_home, features) = features().await;
    let meta =
      test_fixtures::manager_session(&features, "request-replay", "fable", None, "claude-request-replay").await;
    let dispatcher = dispatcher(&features);
    let first = dispatcher
      .admit_input(&meta.id, event_types::USER, "same", "user", json!({"request_id": "browser-send-1"}))
      .await;
    assert!(first.is_ok(), "request_id admission failed: {first:?}");
    let first = first.unwrap();
    let replay = dispatcher
      .admit_input(&meta.id, event_types::USER, "same", "user", json!({"request_id": "browser-send-1"}))
      .await;
    assert!(replay.as_ref().is_ok_and(|event| event == &first), "replay did not return the stored event: {replay:?}");
    assert_eq!(first["request_id"], "browser-send-1");
    assert_eq!(
      features
        .store
        .load_all_facts_view_sync(&meta.id)
        .unwrap()
        .iter()
        .filter(|event| { event.get("request_id").and_then(serde_json::Value::as_str) == Some("browser-send-1") })
        .count(),
      1
    );

    let changed = dispatcher
      .admit_input(&meta.id, event_types::USER, "changed", "user", json!({"request_id": "browser-send-1"}))
      .await;
    assert!(changed.unwrap_err().to_string().contains("different type or content"));
  }

  #[tokio::test]
  async fn request_id_and_input_id_are_mutually_exclusive() {
    let (_home, features) = features().await;
    let meta =
      test_fixtures::manager_session(&features, "request-input-exclusive", "fable", None, "claude-exclusive").await;
    let error = dispatcher(&features)
      .admit_input(
        &meta.id,
        event_types::USER,
        "same",
        "user",
        json!({"input_id": "manual-id", "request_id": "browser-send-1"}),
      )
      .await
      .unwrap_err();

    assert!(error.to_string().contains("mutually exclusive"), "{error}");
  }

  #[tokio::test]
  async fn concurrent_request_id_admission_appends_one_event() {
    let (_home, features) = features().await;
    let meta =
      test_fixtures::manager_session(&features, "request-concurrent", "fable", None, "claude-concurrent").await;
    let dispatcher = dispatcher(&features);
    let (first, second) = tokio::join!(
      dispatcher.admit_input(&meta.id, event_types::USER, "same", "user", json!({"request_id": "browser-race"}),),
      dispatcher.admit_input(&meta.id, event_types::USER, "same", "user", json!({"request_id": "browser-race"}),),
    );
    assert!(first.is_ok() && second.is_ok(), "admission failed: {first:?} {second:?}");
    assert_eq!(first.unwrap(), second.unwrap());
    assert_eq!(
      features
        .store
        .load_all_facts_view_sync(&meta.id)
        .unwrap()
        .iter()
        .filter(|event| event.get("request_id").and_then(serde_json::Value::as_str) == Some("browser-race"))
        .count(),
      1
    );
  }

  #[tokio::test]
  async fn request_event_replay_compares_extra_fields_and_rejects_core_collisions() {
    let (_home, features) = features().await;
    let meta =
      test_fixtures::manager_session(&features, "request-fields", "fable", None, "claude-request-fields").await;
    let dispatcher = dispatcher(&features);

    let collision = dispatcher
      .admit_input(
        &meta.id,
        event_types::USER,
        "message",
        "user",
        json!({"request_id": "collision", "fields": {"content": "replacement"}}),
      )
      .await;
    assert!(
      matches!(&collision, Err(StoreError::Invalid(detail)) if detail == "fields may not replace core event fields: content"),
      "core event field collision was not refused: {collision:?}"
    );

    let fields = json!({"request_id": "extra-fields", "fields": {"summon": {"mention_id": "m1"}}});
    let event = dispatcher.admit_input(&meta.id, event_types::USER, "message", "user", fields.clone()).await;
    assert!(event.is_ok(), "extra event fields were refused: {event:?}");
    let event = event.unwrap();
    let replay = dispatcher.admit_input(&meta.id, event_types::USER, "message", "user", fields).await;
    assert!(replay.as_ref().is_ok_and(|replay| replay == &event), "matching extra fields did not replay: {replay:?}");
    assert_eq!(event["request_id"], "extra-fields");
    assert_eq!(event["summon"], json!({"mention_id": "m1"}));

    let omitted = dispatcher
      .admit_input(&meta.id, event_types::USER, "message", "user", json!({"request_id": "extra-fields"}))
      .await;
    assert!(omitted.is_err(), "omitting stored extra fields did not conflict");
    let changed = dispatcher
      .admit_input(
        &meta.id,
        event_types::USER,
        "message",
        "user",
        json!({"request_id": "extra-fields", "fields": {"summon": {"mention_id": "m2"}}}),
      )
      .await;
    assert!(changed.is_err(), "changed extra fields did not conflict");
    let events = features.store.load_all_facts_view_sync(&meta.id).unwrap();
    assert_eq!(events.iter().filter(|event| event.get("request_id") == Some(&json!("extra-fields"))).count(), 1);
  }

  #[tokio::test]
  async fn close_report_scan_reuses_one_shared_recipient_history() {
    let (_home, features) = features().await;
    let parent =
      test_fixtures::manager_session(&features, "report-parent-count", "fable", None, "claude-report-parent").await;
    let child = features
      .store
      .create_task(
        CreateTask {
          request_id: "report-child-count".to_string(),
          task_parent_id: Some(parent.id.clone()),
          profile: TaskProfile::Manager,
          task: None,
          name: Some("Synthetic report child".to_string()),
          backend: Some("fable".to_string()),
          group: None,
          session_id: None,
        },
        TaskCaller::System,
        Map::new(),
      )
      .await
      .unwrap();
    for index in 0..8 {
      features
        .store
        .append_chat_event(
          &child.id,
          json!({"id":format!("close-{index}"),"type":"task_closed","outcome":"completed","report_to":parent.id}),
        )
        .await
        .unwrap();
    }
    let dispatcher = dispatcher(&features);
    let before = features.store.history_work();

    let blockers = dispatcher.undelivered_report_blockers(&child.id).unwrap();

    let after = features.store.history_work();
    assert_eq!(blockers.len(), 8);
    assert_eq!(after.owned_event_clones - before.owned_event_clones, 0);
    assert_eq!(after.file_reads - before.file_reads, 1, "one parent history load serves all eight close events");
    assert_eq!(after.event_history_views - before.event_history_views, 1);
  }
}
