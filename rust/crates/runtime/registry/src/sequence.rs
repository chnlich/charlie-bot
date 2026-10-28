//! Sequence hooks (`src/runtime/hooks/sequence_controllers.py`): a package that owns durable sequences registers a
//! controller. The controller owns every run whose sequence kind it declares, and it answers for its sessions in the
//! listings and in the bindings the runtime asks for.
//!
//! The run lifecycle methods of the Python controller (`redrive`, `launch_context`, `after_run`, `recover_run`,
//! `reconcile_interrupted`) run over the store's run records and task tree through `SequenceTree`.

use std::collections::HashMap;
use std::sync::Arc;

use charliebot_core::config::CharlieBotConfig;
use charliebot_core::event_types;
use charliebot_store::metadata::{SessionMetadata, UtcDateTime};
use charliebot_store::{ChatEvent, ControlGuard, RunRecord, SharedChatEvent, StopRequest, Store, StoreResult};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value};

use charliebot_store::compact::CompactValue;

use crate::startup_stop::StartupStop;

/// A node's binding to a sequence (`sequence_controllers.SequenceBinding`). A scheduled task's binding is the loaded
/// task whose session id names the node.
pub trait SequenceBinding: Send + Sync {
  fn name(&self) -> &str;
  /// True when the sequence gives the node a backend of its own.
  fn dedicated_backend(&self) -> bool;
  /// The error detail when a backend switch to `target_backend` is refused.
  fn backend_lock_detail(&self, target_backend: &str) -> String;
  /// Runs the node's wake duties and returns an optional turn-input prefix.
  fn on_wake<'a>(
    &'a self,
    meta: &'a SessionMetadata,
    input_events: &'a [Value],
  ) -> BoxFuture<'a, anyhow::Result<Option<String>>>;
  /// Runs the sequence's archive duty for this node.
  fn on_archive<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<()>>;
}

/// A package's runtime interface for one kind of durable sequence (`sequence_controllers.SequenceController`).
pub trait SequenceController: Send + Sync {
  /// The sequence kind this controller owns: the `sequence_ref.kind` of its runs.
  fn sequence_kind(&self) -> &'static str;

  /// Redrive the sequence that owns a durable run reference.
  fn redrive<'a>(&'a self, session_id: &'a str, tree: &'a dyn SequenceTree) -> BoxFuture<'a, anyhow::Result<()>>;

  /// Compose one sequence run's task and input context.
  fn launch_context<'a>(
    &'a self,
    meta: &'a SessionMetadata,
    run: &'a RunRecord,
    launch_text: &'a str,
  ) -> BoxFuture<'a, anyhow::Result<Option<String>>>;

  /// Apply the owning sequence's durable finish duty.
  fn after_run<'a>(
    &'a self,
    session_id: &'a str,
    run: &'a RunRecord,
    tree: &'a dyn SequenceTree,
  ) -> BoxFuture<'a, anyhow::Result<()>>;

  /// Recover one terminal or unlaunched sequence run.
  fn recover_run<'a>(
    &'a self,
    session_id: &'a str,
    run: &'a RunRecord,
    outcome: Option<&'a str>,
    tree: &'a dyn SequenceTree,
  ) -> BoxFuture<'a, anyhow::Result<bool>>;

  /// Reconcile sequence state before ordinary run recovery.
  fn reconcile_interrupted<'a>(&'a self, tree: &'a dyn SequenceTree) -> BoxFuture<'a, anyhow::Result<()>>;

  /// The binding of `session_id`, if this controller binds it.
  fn binding(&self, session_id: &str) -> Option<Arc<dyn SequenceBinding>>;

  /// Whether `meta` carries this controller's legacy ownership marker.
  fn owns_session(&self, meta: &SessionMetadata) -> bool;

  /// This controller's per-row listing fields for one batch of session ids.
  fn listing_fields(&self, session_ids: &[String], now: UtcDateTime) -> HashMap<String, Map<String, Value>>;

  /// Moves every binding of `source` onto `new`, the node that replaced it. `true` when a binding moved.
  fn move_bindings<'a>(&'a self, _source: &'a str, _new: &'a str) -> BoxFuture<'a, anyhow::Result<bool>> {
    Box::pin(async { Ok(false) })
  }
}

/// The shared adapter's settled launch/wait verdict.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SequenceLaunchSettlement {
  pub outcome: Option<String>,
  pub withheld: Option<String>,
}

/// The task execution adapter available to sequence-owned work.
pub trait SequenceExecutor: Send + Sync {
  fn launch<'a>(
    &'a self,
    session_id: &'a str,
    run_id: &'a str,
    prompt: Option<&'a str>,
  ) -> BoxFuture<'a, StoreResult<()>>;

  fn launch_and_settle<'a>(
    &'a self,
    session_id: &'a str,
    run_id: &'a str,
    prompt: Option<&'a str>,
  ) -> BoxFuture<'a, StoreResult<SequenceLaunchSettlement>>;
}

/// The dispatcher calls exposed to sequence controllers.
pub trait SequenceDispatch: Send + Sync {
  fn admit_input<'a>(
    &'a self,
    session_id: &'a str,
    event_type: &'a str,
    content: &'a str,
    actor: &'a str,
    fields: Value,
  ) -> BoxFuture<'a, StoreResult<ChatEvent>>;

  fn dispatch_pending<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, StoreResult<()>>;

  fn report_source_event(&self, child_session_id: &str, child_kind: &str) -> StoreResult<ChatEvent>;

  #[allow(clippy::too_many_arguments)]
  fn deliver_child_report_locked<'a>(
    &'a self,
    guard: &'a ControlGuard,
    child_session_id: &'a str,
    source_event: ChatEvent,
    outcome: &'a str,
    summary: &'a str,
    result_refs: Option<&'a [String]>,
    recipient: Option<&'a str>,
  ) -> BoxFuture<'a, StoreResult<(ChatEvent, bool)>>;

  fn deliver_child_report<'a>(
    &'a self,
    child_session_id: &'a str,
    source_event: ChatEvent,
    outcome: &'a str,
    summary: &'a str,
    result_refs: Option<&'a [String]>,
    recipient: Option<&'a str>,
  ) -> BoxFuture<'a, StoreResult<(ChatEvent, bool)>>;

  fn wake_parent<'a>(&'a self, parent_id: &'a str, report: ChatEvent) -> BoxFuture<'a, StoreResult<bool>>;
}

/// A restore's result (`task_completion.py::restore_chain_locked`): the restored ids, topmost first, and one
/// `(node, event, epoch)` announcement per `task_reopened` fact the call wrote. The caller sends each announcement
/// with its epoch after it releases the control lock.
pub type RestoreOutcome = (Vec<String>, Vec<(String, ChatEvent, u64)>);

/// Completion calls exposed to sequence controllers.
pub trait SequenceCompletion: Send + Sync {
  fn evaluate_automatic_completion<'a>(
    &'a self,
    session_id: &'a str,
    run_id: &'a str,
    summary: Option<&'a str>,
    result_refs: Option<&'a [String]>,
    request_id: Option<&'a str>,
  ) -> BoxFuture<'a, StoreResult<(usize, Value)>>;

  fn close_sequence_child<'a>(
    &'a self,
    session_id: &'a str,
    request_id: &'a str,
    outcome: &'a str,
    evidence: &'a Value,
  ) -> BoxFuture<'a, StoreResult<(usize, Value)>>;

  fn restore_chain<'a>(
    &'a self,
    session_id: &'a str,
    request_id: &'a str,
    reason: &'a str,
  ) -> BoxFuture<'a, StoreResult<Vec<String>>>;

  /// The restore's core for a caller that holds the control lock; returns Python's `(restored, announcements)` pair.
  fn restore_chain_locked<'a>(
    &'a self,
    guard: &'a ControlGuard,
    session_id: &'a str,
    request_id: &'a str,
    reason: &'a str,
  ) -> BoxFuture<'a, StoreResult<RestoreOutcome>>;

  /// Archive a failed worker whose report has landed, when the completion owner finds no remaining work.
  fn archive_unsuccessful_worker<'a>(
    &'a self,
    session_id: &'a str,
    run_id: &'a str,
    outcome: &'a str,
  ) -> BoxFuture<'a, StoreResult<bool>>;
}

/// Session-event aggregation calls exposed to sequence controllers.
pub trait SequenceSessionEvents: Send + Sync {
  fn prime_aggregator<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, StoreResult<u64>>;

  fn announce_appended_event<'a>(
    &'a self,
    session_id: &'a str,
    event: &'a ChatEvent,
    epoch: u64,
  ) -> BoxFuture<'a, StoreResult<()>>;
}

/// Runtime-owned collaborators used by `SequenceTree`; all are supplied by the application composition root.
pub struct SequenceTreeRuntime {
  pub dispatch: Arc<dyn SequenceDispatch>,
  pub completion: Arc<dyn SequenceCompletion>,
  pub session_events: Arc<dyn SequenceSessionEvents>,
  pub executor: Option<Arc<dyn SequenceExecutor>>,
  pub startup_stop: StartupStop,
}

/// Store-owned task-tree and run operations available to a sequence controller.
pub trait SequenceTree: Send + Sync {
  fn startup_stop(&self) -> StartupStop;

  fn load_meta<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, StoreResult<Option<SessionMetadata>>>;

  fn task_state(&self, session_id: &str) -> StoreResult<String>;

  fn control_lock<'a>(&'a self) -> BoxFuture<'a, ControlGuard>;

  fn config(&self) -> &CharlieBotConfig;

  fn run_dir(&self, session_id: &str, run_id: &str) -> std::path::PathBuf;

  fn read_run_sync(&self, session_id: &str, run_id: &str) -> StoreResult<Option<RunRecord>>;

  fn get_run<'a>(&'a self, session_id: &'a str, run_id: &'a str) -> BoxFuture<'a, StoreResult<Option<RunRecord>>>;

  fn list_run_records_sync(&self, session_id: &str) -> StoreResult<Vec<RunRecord>>;

  fn last_ended_run_id(&self, session_id: &str) -> StoreResult<Option<String>>;

  fn load_events_sync(&self, session_id: &str) -> StoreResult<Arc<Vec<SharedChatEvent>>>;

  fn terminal_outcome(&self, events: &[SharedChatEvent], run_id: &str) -> Option<String>;

  fn terminal_outcome_of<'a>(
    &'a self,
    session_id: &'a str,
    run_id: &'a str,
  ) -> BoxFuture<'a, StoreResult<Option<String>>>;

  fn run_has_terminal_fact(&self, run: &RunRecord, events: &[SharedChatEvent]) -> bool;

  fn register_run_locked<'a>(
    &'a self,
    guard: &'a ControlGuard,
    session_id: &'a str,
    run: RunRecord,
    task_spec: Option<&'a str>,
  ) -> BoxFuture<'a, StoreResult<RunRecord>>;

  fn request_stop<'a>(
    &'a self,
    session_id: &'a str,
    run_id: &'a str,
    reason: &'a str,
  ) -> BoxFuture<'a, StoreResult<StopRequest>>;

  fn deliver_to_successor<'a>(
    &'a self,
    session_id: &'a str,
    event: Value,
  ) -> BoxFuture<'a, StoreResult<Option<String>>>;

  fn dispatch(&self) -> &dyn SequenceDispatch;

  fn completion(&self) -> &dyn SequenceCompletion;

  fn session_events(&self) -> &dyn SequenceSessionEvents;

  fn executor(&self) -> Option<Arc<dyn SequenceExecutor>>;
}

/// The task-tree handle backed by one store instance.
pub struct StoreSequenceTree {
  store: Arc<Store>,
  runtime: SequenceTreeRuntime,
}

impl StoreSequenceTree {
  /// Bind a sequence tree to the server's store.
  pub fn new(store: Arc<Store>, runtime: SequenceTreeRuntime) -> Self {
    Self { store, runtime }
  }
}

impl SequenceTree for StoreSequenceTree {
  fn startup_stop(&self) -> StartupStop {
    self.runtime.startup_stop.clone()
  }

  fn load_meta<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, StoreResult<Option<SessionMetadata>>> {
    Box::pin(async move { self.store.load_meta(session_id).await })
  }

  fn task_state(&self, session_id: &str) -> StoreResult<String> {
    self.store.task_state(session_id)
  }

  fn config(&self) -> &CharlieBotConfig {
    self.store.config()
  }

  fn control_lock<'a>(&'a self) -> BoxFuture<'a, ControlGuard> {
    Box::pin(async move { self.store.control_lock().await })
  }

  fn run_dir(&self, session_id: &str, run_id: &str) -> std::path::PathBuf {
    self.store.run_dir(session_id, run_id)
  }

  fn read_run_sync(&self, session_id: &str, run_id: &str) -> StoreResult<Option<RunRecord>> {
    self.store.read_run_sync(session_id, run_id)
  }

  fn get_run<'a>(&'a self, session_id: &'a str, run_id: &'a str) -> BoxFuture<'a, StoreResult<Option<RunRecord>>> {
    Box::pin(async move { self.store.read_run_sync(session_id, run_id) })
  }

  fn list_run_records_sync(&self, session_id: &str) -> StoreResult<Vec<RunRecord>> {
    self.store.list_run_records_sync(session_id)
  }

  fn last_ended_run_id(&self, session_id: &str) -> StoreResult<Option<String>> {
    self.store.last_ended_run_id(session_id)
  }

  fn load_events_sync(&self, session_id: &str) -> StoreResult<Arc<Vec<SharedChatEvent>>> {
    // Python hands readers its cached live-facts list by reference (`src/runtime/task_sessions.py:650-662`).
    self.store.load_all_facts_shared_sync(session_id)
  }

  fn terminal_outcome(&self, events: &[SharedChatEvent], run_id: &str) -> Option<String> {
    events
      .iter()
      .rfind(|event| {
        event.get("type").and_then(CompactValue::as_str) == Some(event_types::RUN_FINISHED)
          && event.get("run_id").and_then(CompactValue::as_str) == Some(run_id)
      })
      .and_then(|event| event.get("outcome").and_then(CompactValue::as_str))
      .map(str::to_string)
  }

  fn terminal_outcome_of<'a>(
    &'a self,
    session_id: &'a str,
    run_id: &'a str,
  ) -> BoxFuture<'a, StoreResult<Option<String>>> {
    Box::pin(async move {
      let events = self.store.load_all_facts_shared_sync(session_id)?;
      Ok(self.terminal_outcome(&events, run_id))
    })
  }

  fn run_has_terminal_fact(&self, run: &RunRecord, events: &[SharedChatEvent]) -> bool {
    self.terminal_outcome(events, &run.id).is_some()
  }

  fn register_run_locked<'a>(
    &'a self,
    guard: &'a ControlGuard,
    session_id: &'a str,
    run: RunRecord,
    task_spec: Option<&'a str>,
  ) -> BoxFuture<'a, StoreResult<RunRecord>> {
    Box::pin(async move { self.store.register_run_locked(guard, session_id, run, task_spec).await })
  }

  fn request_stop<'a>(
    &'a self,
    session_id: &'a str,
    run_id: &'a str,
    reason: &'a str,
  ) -> BoxFuture<'a, StoreResult<StopRequest>> {
    Box::pin(async move { self.store.request_stop(session_id, run_id, reason).await })
  }

  fn deliver_to_successor<'a>(
    &'a self,
    session_id: &'a str,
    event: Value,
  ) -> BoxFuture<'a, StoreResult<Option<String>>> {
    Box::pin(async move { self.store.deliver_to_successor(session_id, event).await })
  }

  fn dispatch(&self) -> &dyn SequenceDispatch {
    self.runtime.dispatch.as_ref()
  }

  fn completion(&self) -> &dyn SequenceCompletion {
    self.runtime.completion.as_ref()
  }

  fn session_events(&self) -> &dyn SequenceSessionEvents {
    self.runtime.session_events.as_ref()
  }

  fn executor(&self) -> Option<Arc<dyn SequenceExecutor>> {
    self.runtime.executor.as_ref().map(Arc::clone)
  }
}

#[cfg(test)]
mod tests {
  use std::collections::HashMap;
  use std::sync::Mutex;
  use std::sync::OnceLock;

  use charliebot_store::metadata::{SessionMetadata, TaskProfile};
  use charliebot_store::{
    AccountLabelKeeper, AnchorFieldReconciler, CreateTask, RunKind, RunRecord, SequenceArchiveHook,
    SequenceBindingMover, SequenceListingFields, StoreError, StoreHookFuture, StoreHooks, TaskCaller,
  };
  use serde_json::{Map, Value};

  use super::*;

  struct TestAccountLabels;

  impl AccountLabelKeeper for TestAccountLabels {
    fn record_account_label(&self, _meta: &mut SessionMetadata, _label: Option<&str>) -> Result<bool, StoreError> {
      Ok(false)
    }

    fn account_label(&self, _meta: &SessionMetadata) -> Option<String> {
      None
    }
  }

  struct TestAnchorReconciler;

  impl AnchorFieldReconciler for TestAnchorReconciler {
    fn reconcile(&self, _disk: &SessionMetadata, _attempted: &mut SessionMetadata) -> Result<(), StoreError> {
      Ok(())
    }
  }

  struct TestSequenceHooks;

  impl SequenceListingFields for TestSequenceHooks {
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

    fn sequence_subtree_roots(&self, _metas: &[Arc<SessionMetadata>]) -> StoreResult<HashMap<String, String>> {
      Ok(HashMap::new())
    }

    fn view_subtree_roots(
      &self,
      _metas: &[Arc<SessionMetadata>],
    ) -> StoreResult<HashMap<String, HashMap<String, String>>> {
      Ok(HashMap::new())
    }
  }

  impl SequenceArchiveHook for TestSequenceHooks {
    fn on_archive<'a>(&'a self, _session_id: &'a str) -> StoreHookFuture<'a, ()> {
      Box::pin(async { Ok(()) })
    }
  }

  impl SequenceBindingMover for TestSequenceHooks {
    fn move_bindings<'a>(&'a self, _source: &'a str, _new: &'a str) -> StoreHookFuture<'a, bool> {
      Box::pin(async { Ok(false) })
    }
  }

  struct TestSequenceRuntime;

  impl SequenceDispatch for TestSequenceRuntime {
    fn admit_input<'a>(
      &'a self,
      _session_id: &'a str,
      _event_type: &'a str,
      _content: &'a str,
      _actor: &'a str,
      _fields: Value,
    ) -> BoxFuture<'a, StoreResult<ChatEvent>> {
      Box::pin(async { Err(StoreError::new("dispatch test seam was called")) })
    }

    fn dispatch_pending<'a>(&'a self, _session_id: &'a str) -> BoxFuture<'a, StoreResult<()>> {
      Box::pin(async { Err(StoreError::new("dispatch test seam was called")) })
    }

    fn report_source_event(&self, _child_session_id: &str, _child_kind: &str) -> StoreResult<ChatEvent> {
      Err(StoreError::new("dispatch test seam was called"))
    }

    fn deliver_child_report_locked<'a>(
      &'a self,
      _guard: &'a ControlGuard,
      _child_session_id: &'a str,
      _source_event: ChatEvent,
      _outcome: &'a str,
      _summary: &'a str,
      _result_refs: Option<&'a [String]>,
      _recipient: Option<&'a str>,
    ) -> BoxFuture<'a, StoreResult<(ChatEvent, bool)>> {
      Box::pin(async { Err(StoreError::new("dispatch test seam was called")) })
    }

    fn deliver_child_report<'a>(
      &'a self,
      _child_session_id: &'a str,
      _source_event: ChatEvent,
      _outcome: &'a str,
      _summary: &'a str,
      _result_refs: Option<&'a [String]>,
      _recipient: Option<&'a str>,
    ) -> BoxFuture<'a, StoreResult<(ChatEvent, bool)>> {
      Box::pin(async { Err(StoreError::new("dispatch test seam was called")) })
    }

    fn wake_parent<'a>(&'a self, _parent_id: &'a str, _report: ChatEvent) -> BoxFuture<'a, StoreResult<bool>> {
      Box::pin(async { Err(StoreError::new("dispatch test seam was called")) })
    }
  }

  impl SequenceCompletion for TestSequenceRuntime {
    fn evaluate_automatic_completion<'a>(
      &'a self,
      _session_id: &'a str,
      _run_id: &'a str,
      _summary: Option<&'a str>,
      _result_refs: Option<&'a [String]>,
      _request_id: Option<&'a str>,
    ) -> BoxFuture<'a, StoreResult<(usize, Value)>> {
      Box::pin(async { Err(StoreError::new("completion test seam was called")) })
    }

    fn close_sequence_child<'a>(
      &'a self,
      _session_id: &'a str,
      _request_id: &'a str,
      _outcome: &'a str,
      _evidence: &'a Value,
    ) -> BoxFuture<'a, StoreResult<(usize, Value)>> {
      Box::pin(async { Err(StoreError::new("completion test seam was called")) })
    }

    fn restore_chain<'a>(
      &'a self,
      _session_id: &'a str,
      _request_id: &'a str,
      _reason: &'a str,
    ) -> BoxFuture<'a, StoreResult<Vec<String>>> {
      Box::pin(async { Err(StoreError::new("completion test seam was called")) })
    }

    fn restore_chain_locked<'a>(
      &'a self,
      _guard: &'a ControlGuard,
      _session_id: &'a str,
      _request_id: &'a str,
      _reason: &'a str,
    ) -> BoxFuture<'a, StoreResult<RestoreOutcome>> {
      Box::pin(async { Err(StoreError::new("completion test seam was called")) })
    }

    fn archive_unsuccessful_worker<'a>(
      &'a self,
      _session_id: &'a str,
      _run_id: &'a str,
      _outcome: &'a str,
    ) -> BoxFuture<'a, StoreResult<bool>> {
      Box::pin(async { Err(StoreError::new("completion test seam was called")) })
    }
  }

  impl SequenceSessionEvents for TestSequenceRuntime {
    fn prime_aggregator<'a>(&'a self, _session_id: &'a str) -> BoxFuture<'a, StoreResult<u64>> {
      Box::pin(async { Err(StoreError::new("session-event test seam was called")) })
    }

    fn announce_appended_event<'a>(
      &'a self,
      _session_id: &'a str,
      _event: &'a ChatEvent,
      _epoch: u64,
    ) -> BoxFuture<'a, StoreResult<()>> {
      Box::pin(async { Err(StoreError::new("session-event test seam was called")) })
    }
  }

  struct TestController {
    session_id: String,
    calls: Arc<Mutex<Vec<&'static str>>>,
  }

  impl SequenceController for TestController {
    fn sequence_kind(&self) -> &'static str {
      "synthetic"
    }

    fn redrive<'a>(&'a self, session_id: &'a str, tree: &'a dyn SequenceTree) -> BoxFuture<'a, anyhow::Result<()>> {
      Box::pin(async move {
        assert_eq!(tree.task_state(session_id)?, "open");
        assert!(tree.load_meta(session_id).await?.is_some());
        self.calls.lock().unwrap().push("redrive");
        Ok(())
      })
    }

    fn launch_context<'a>(
      &'a self,
      meta: &'a SessionMetadata,
      run: &'a RunRecord,
      launch_text: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<Option<String>>> {
      Box::pin(async move {
        assert_eq!(meta.id, self.session_id);
        assert_eq!(run.session_id, self.session_id);
        self.calls.lock().unwrap().push("launch_context");
        Ok(Some(format!("{}:{launch_text}", run.id)))
      })
    }

    fn after_run<'a>(
      &'a self,
      session_id: &'a str,
      _run: &'a RunRecord,
      tree: &'a dyn SequenceTree,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
      Box::pin(async move {
        assert_eq!(tree.list_run_records_sync(session_id)?.len(), 1);
        self.calls.lock().unwrap().push("after_run");
        Ok(())
      })
    }

    fn recover_run<'a>(
      &'a self,
      session_id: &'a str,
      run: &'a RunRecord,
      outcome: Option<&'a str>,
      tree: &'a dyn SequenceTree,
    ) -> BoxFuture<'a, anyhow::Result<bool>> {
      Box::pin(async move {
        assert_eq!(tree.read_run_sync(session_id, &run.id)?.as_ref(), Some(run));
        self.calls.lock().unwrap().push("recover_run");
        Ok(outcome.is_some())
      })
    }

    fn reconcile_interrupted<'a>(&'a self, tree: &'a dyn SequenceTree) -> BoxFuture<'a, anyhow::Result<()>> {
      Box::pin(async move {
        let _guard = tree.control_lock().await;
        assert!(!tree.load_events_sync(&self.session_id)?.is_empty());
        self.calls.lock().unwrap().push("reconcile_interrupted");
        Ok(())
      })
    }

    fn binding(&self, _session_id: &str) -> Option<Arc<dyn SequenceBinding>> {
      None
    }

    fn owns_session(&self, _meta: &SessionMetadata) -> bool {
      false
    }

    fn listing_fields(&self, _session_ids: &[String], _now: UtcDateTime) -> HashMap<String, Map<String, Value>> {
      HashMap::new()
    }
  }

  fn controller_calls() -> Arc<Mutex<Vec<&'static str>>> {
    static CALLS: OnceLock<Arc<Mutex<Vec<&'static str>>>> = OnceLock::new();
    Arc::clone(CALLS.get_or_init(|| Arc::new(Mutex::new(Vec::new()))))
  }

  #[tokio::test]
  async fn controller_drives_every_run_method_over_store_sequence_tree() {
    controller_calls().lock().unwrap().clear();
    let controller: Arc<dyn SequenceController> =
      Arc::new(TestController { session_id: "sequence-node".to_string(), calls: controller_calls() });

    let home = tempfile::tempdir().unwrap();
    let cfg =
      charliebot_core::config::load_config(home.path(), &charliebot_core::config::ConfigSchema::default()).unwrap();
    let hooks = Arc::new(TestSequenceHooks);
    let store = Arc::new(
      Store::new(
        cfg,
        Vec::new(),
        StoreHooks {
          account_labels: Some(Arc::new(TestAccountLabels)),
          anchor_reconciler: Some(Arc::new(TestAnchorReconciler)),
          sequence_listing: Some(hooks.clone()),
          sequence_archive: Some(hooks.clone()),
          sequence_binding_mover: Some(hooks.clone()),
          runtime_events: Some(Arc::new(charliebot_store::EmptyRuntimeEventHooks)),
        },
      )
      .unwrap(),
    );
    let session_id = "sequence-node";
    let meta = store
      .create_task(
        CreateTask {
          request_id: "sequence-node-request".to_string(),
          task_parent_id: None,
          profile: TaskProfile::Manager,
          task: None,
          name: Some("Sequence node".to_string()),
          backend: Some("synthetic".to_string()),
          group: None,
          session_id: Some(session_id.to_string()),
        },
        TaskCaller::System,
        Map::new(),
      )
      .await
      .unwrap();
    let run = RunRecord {
      id: "sequence-run".to_string(),
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
    };
    let run = {
      let guard = store.control_lock().await;
      store.register_run_locked(&guard, session_id, run, Some("synthetic task")).await.unwrap()
    };
    let runtime = Arc::new(TestSequenceRuntime);
    let tree_store = Arc::clone(&store);
    let tree = StoreSequenceTree::new(
      store,
      SequenceTreeRuntime {
        dispatch: runtime.clone(),
        completion: runtime.clone(),
        session_events: runtime,
        executor: None,
        startup_stop: StartupStop::default(),
      },
    );
    tree_store.clear_chat_events_cache(session_id);
    let reads_before_facts = tree_store.history_work().file_reads;
    let first_facts = tree.load_events_sync(session_id).unwrap();
    let reads_after_first_facts = tree_store.history_work().file_reads;
    let second_facts = tree.load_events_sync(session_id).unwrap();
    assert!(Arc::ptr_eq(&first_facts, &second_facts));
    assert_eq!(reads_after_first_facts, reads_before_facts + 1);
    assert_eq!(tree_store.history_work().file_reads, reads_after_first_facts);
    controller.redrive(session_id, &tree).await.unwrap();
    assert_eq!(controller.launch_context(&meta, &run, "launch").await.unwrap().as_deref(), Some("sequence-run:launch"));
    controller.after_run(session_id, &run, &tree).await.unwrap();
    assert!(controller.recover_run(session_id, &run, Some("success"), &tree).await.unwrap());
    controller.reconcile_interrupted(&tree).await.unwrap();

    assert_eq!(
      *controller_calls().lock().unwrap(),
      ["redrive", "launch_context", "after_run", "recover_run", "reconcile_interrupted"]
    );
  }
}
