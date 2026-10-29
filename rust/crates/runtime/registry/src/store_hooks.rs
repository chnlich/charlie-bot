//! The registry's answers to the hook traits the store calls (`charliebot_store::StoreHooks`). The store names no
//! package: it asks these adapters, which read the registered backend lifecycles and sequence controllers. The server
//! injects them at startup, when it constructs the store.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use charliebot_core::config::CharlieBotConfig;
use charliebot_store::hooks::{AfterTurnWork, EventRenderer, RuntimeEventHooks};
use charliebot_store::metadata::{SessionMetadata, UtcDateTime};
use charliebot_store::{
  AccountLabelKeeper, AnchorFieldReconciler, SequenceArchiveHook, SequenceBindingMover, SequenceListingFields,
  StoreError, StoreHookFuture, StoreHooks, StoreResult,
};
use serde_json::{Map, Value};

use crate::registry::Registry;

/// The store's hooks, answered by the registered implementations.
pub fn store_hooks(registry: Arc<Registry>) -> StoreHooks {
  let account_labels: Arc<dyn AccountLabelKeeper> =
    Arc::new(LifecycleAccountLabels { registry: Arc::clone(&registry) });
  let anchor_reconciler: Arc<dyn AnchorFieldReconciler> =
    Arc::new(LifecycleAnchors { registry: Arc::clone(&registry) });
  let sequence_listing: Arc<dyn SequenceListingFields> =
    Arc::new(RegisteredSequences { registry: Arc::clone(&registry) });
  let sequence_archive: Arc<dyn SequenceArchiveHook> =
    Arc::new(RegisteredSequenceArchive { registry: Arc::clone(&registry) });
  let sequence_binding_mover: Arc<dyn SequenceBindingMover> =
    Arc::new(RegisteredSequenceBindingMover { registry: Arc::clone(&registry) });
  let runtime_events: Arc<dyn RuntimeEventHooks> = Arc::new(RegistryRuntimeEvents { registry });
  StoreHooks {
    account_labels: Some(account_labels),
    anchor_reconciler: Some(anchor_reconciler),
    sequence_listing: Some(sequence_listing),
    sequence_archive: Some(sequence_archive),
    sequence_binding_mover: Some(sequence_binding_mover),
    runtime_events: Some(runtime_events),
  }
}

/// The account label is a fact of the session: every registered lifecycle that keeps one reads and writes it.
struct LifecycleAccountLabels {
  registry: Arc<Registry>,
}

impl AccountLabelKeeper for LifecycleAccountLabels {
  fn record_account_label(&self, meta: &mut SessionMetadata, label: Option<&str>) -> StoreResult<bool> {
    let mut changed = false;
    for lifecycle in self.registry.lifecycles() {
      changed |= lifecycle.record_account_label(meta, label);
    }
    Ok(changed)
  }

  fn account_label(&self, meta: &SessionMetadata) -> Option<String> {
    self.registry.lifecycles().iter().find_map(|lifecycle| lifecycle.account_label(meta))
  }
}

/// Restores the resume anchors and each lifecycle's account label from disk before an ordinary save.
struct LifecycleAnchors {
  registry: Arc<Registry>,
}

impl AnchorFieldReconciler for LifecycleAnchors {
  fn reconcile(&self, disk: &SessionMetadata, attempted: &mut SessionMetadata) -> StoreResult<()> {
    if attempted.cc_session_id != disk.cc_session_id {
      tracing::warn!(
        session_id = %attempted.id,
        field = "cc_session_id",
        on_disk = ?disk.cc_session_id,
        attempted = ?attempted.cc_session_id,
        "session_anchor_write_corrected"
      );
      attempted.cc_session_id.clone_from(&disk.cc_session_id);
    }
    if attempted.native_backend != disk.native_backend {
      tracing::warn!(
        session_id = %attempted.id,
        field = "native_backend",
        on_disk = ?disk.native_backend,
        attempted = ?attempted.native_backend,
        "session_anchor_write_corrected"
      );
      attempted.native_backend.clone_from(&disk.native_backend);
    }
    for lifecycle in self.registry.lifecycles() {
      let label_on_disk = lifecycle.account_label(disk);
      if lifecycle.account_label(attempted) != label_on_disk {
        tracing::warn!(
          session_id = %attempted.id,
          field = lifecycle.account_source(),
          on_disk = ?label_on_disk,
          attempted = ?lifecycle.account_label(attempted),
          "session_anchor_write_corrected"
        );
        lifecycle.record_account_label(attempted, label_on_disk.as_deref());
      }
    }
    Ok(())
  }
}

/// The sequence rows of the session listings, from the registered controllers.
struct RegisteredSequences {
  registry: Arc<Registry>,
}

impl SequenceListingFields for RegisteredSequences {
  fn owns_session(&self, meta: &SessionMetadata) -> bool {
    self.registry.sequence_owns_session(meta)
  }

  fn listing_fields(
    &self,
    session_ids: &[String],
    now: UtcDateTime,
  ) -> StoreResult<HashMap<String, Map<String, Value>>> {
    Ok(self.registry.sequence_listing_fields(session_ids, now))
  }

  fn sequence_subtree_roots(&self, metas: &[Arc<SessionMetadata>]) -> StoreResult<HashMap<String, String>> {
    Ok(subtree_roots(metas, |meta| self.registry.sequence_owns_session(meta), false))
  }

  fn view_subtree_roots(
    &self,
    metas: &[Arc<SessionMetadata>],
  ) -> StoreResult<HashMap<String, HashMap<String, String>>> {
    let mut view_roots: HashMap<&'static str, HashSet<String>> = HashMap::new();
    for meta in metas {
      for contribution in self.registry.sidebar_contributions() {
        if let Some(view) = contribution.view_member(meta) {
          view_roots.entry(view).or_default().insert(meta.id.clone());
        }
      }
    }
    Ok(
      view_roots
        .into_iter()
        .map(|(view, roots)| {
          let members = subtree_roots(metas, |meta| roots.contains(&meta.id), true);
          (view.to_string(), members)
        })
        .collect(),
    )
  }
}

fn subtree_roots(
  metas: &[Arc<SessionMetadata>],
  is_root: impl Fn(&SessionMetadata) -> bool,
  include_root: bool,
) -> HashMap<String, String> {
  let by_id: HashMap<&str, &SessionMetadata> = metas.iter().map(|meta| (meta.id.as_str(), meta.as_ref())).collect();
  let mut roots: HashMap<String, String> = HashMap::new();
  for start in metas {
    let mut chain = Vec::new();
    let mut seen = HashSet::from([start.id.as_str()]);
    let mut root = if include_root && is_root(start) { Some(start.id.as_str()) } else { None };
    let mut parent_id = start.task_parent_id.as_deref();
    while root.is_none() {
      let Some(parent) = parent_id else { break };
      if let Some(root_id) = roots.get(parent) {
        root = Some(root_id.as_str());
        break;
      }
      let Some(parent_meta) = by_id.get(parent).copied() else { break };
      if !seen.insert(parent_meta.id.as_str()) {
        break;
      }
      if is_root(parent_meta) {
        root = Some(parent_meta.id.as_str());
        break;
      }
      chain.push(parent_meta.id.as_str());
      parent_id = parent_meta.task_parent_id.as_deref();
    }
    if let Some(root_id) = root {
      let root_owned = root_id.to_string();
      for member in chain.into_iter().chain(std::iter::once(start.id.as_str())) {
        roots.insert(member.to_string(), root_owned.clone());
      }
      if include_root {
        roots.insert(root_owned.clone(), root_owned);
      }
    }
  }
  roots
}

/// The sequence archive duty of a node's registered binding.
struct RegisteredSequenceArchive {
  registry: Arc<Registry>,
}

impl SequenceArchiveHook for RegisteredSequenceArchive {
  fn on_archive<'a>(&'a self, session_id: &'a str) -> StoreHookFuture<'a, ()> {
    Box::pin(async move {
      let binding = self.registry.binding_for(session_id).map_err(|error| StoreError::new(error.to_string()))?;
      if let Some(binding) = binding {
        binding.on_archive().await.map_err(|error| StoreError::new(error.to_string()))?;
      }
      Ok(())
    })
  }
}

/// The bindings each registered sequence controller moves when an elone replaces a node.
struct RegisteredSequenceBindingMover {
  registry: Arc<Registry>,
}

impl SequenceBindingMover for RegisteredSequenceBindingMover {
  fn move_bindings<'a>(&'a self, source_session_id: &'a str, new_session_id: &'a str) -> StoreHookFuture<'a, bool> {
    Box::pin(async move {
      let mut moved = false;
      for controller in self.registry.sequence_controllers() {
        moved |= controller
          .move_bindings(source_session_id, new_session_id)
          .await
          .map_err(|error| StoreError::new(error.to_string()))?;
      }
      Ok(moved)
    })
  }
}

/// The failure of an after-turn work when no feature context is bound: the server binds one at startup, and a bound one
/// that was dropped counts as unbound.
const NO_FEATURE_CONTEXT: &str = "no FeatureContext is bound to the registry (unset, or the bound one was dropped)";

/// Bridges feature turn contributions and event renderers into the store's runtime event hooks.
struct RegistryRuntimeEvents {
  registry: Arc<Registry>,
}

impl RuntimeEventHooks for RegistryRuntimeEvents {
  fn event_renderers(&self) -> StoreResult<Vec<(String, Arc<dyn EventRenderer>)>> {
    let mut renderers = Vec::new();
    for (_, contribution) in self.registry.turn_contributions.pairs() {
      for (event_type, renderer_fn) in contribution.event_renderers() {
        let boxed: Arc<dyn EventRenderer> = Arc::new(move |event: &Value| {
          let res = renderer_fn(event);
          Ok(if res.is_null() { None } else { Some(res) })
        });
        renderers.push((event_type.to_string(), boxed));
      }
    }
    Ok(renderers)
  }

  /// One work per turn contribution, in registration order. The feature context is read as the turn ends, so a missing
  /// one fails every work of that turn by name.
  fn after_turn(&self, meta: SessionMetadata, event: Value, cfg: CharlieBotConfig) -> Vec<AfterTurnWork> {
    let features = self.registry.feature_context();
    self
      .registry
      .turn_contributions
      .values()
      .map(|contribution| {
        let contribution = Arc::clone(contribution);
        let features = features.clone();
        let meta = meta.clone();
        let event = event.clone();
        let cfg = cfg.clone();
        AfterTurnWork {
          name: contribution.class_name().to_string(),
          future: Box::pin(async move {
            let features = features.ok_or_else(|| StoreError::new(NO_FEATURE_CONTEXT))?;
            contribution
              .after_turn(&meta, &event, &cfg, &features)
              .await
              .map_err(|error| StoreError::new(format!("{error:#}")))
          }),
        }
      })
      .collect()
  }

  fn mark_sidebar_dirty(&self, session_id: &str) {
    charliebot_store::sidebar_state::mark_sidebar_dirty(session_id);
  }

  fn drop_session_runtime_state(&self, _session_id: &str) {}

  fn copy_on_fork(&self, parent_dir: &Path, child_dir: &Path) -> StoreResult<()> {
    for contribution in self.registry.sidebar_contributions() {
      contribution.copy_on_fork(parent_dir, child_dir).map_err(|error| StoreError::new(error.to_string()))?;
    }
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use std::fmt::Debug;
  use std::sync::Mutex;
  use std::time::Duration;

  use charliebot_core::test_support::EventSink;
  use charliebot_store::Store;
  use charliebot_store::metadata::TaskProfile;
  use futures_util::future::BoxFuture;
  use serde_json::json;
  use tracing::field::{Field, Visit};
  use tracing::{Event, Level};

  use super::*;
  use charliebot_core::config::BackendOption;
  use crate::context::{FeatureContext, features_over};
  use crate::registry::{
    AgentBackend, BackendFactory, BackendLifecycle, BackendTraits, BackendTypeRegistration, LaunchArgs,
    RegistryError, ResumeStyle,
  };
  use crate::turn::TurnContribution;

  /// The session ids the recording contribution saw, held in the test context's startup state.
  #[derive(Default)]
  struct CallLog(Mutex<Vec<String>>);

  /// Fails every after-turn call.
  struct FailingContribution;

  impl TurnContribution for FailingContribution {
    fn class_name(&self) -> &'static str {
      "FailingContribution"
    }

    fn after_turn<'a>(
      &'a self,
      _meta: &'a SessionMetadata,
      _done_event: &'a Value,
      _cfg: &'a CharlieBotConfig,
      _features: &'a Arc<FeatureContext>,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
      Box::pin(async { Err(anyhow::anyhow!("synthetic after-turn failure")) })
    }
  }

  /// Records the session of each after-turn call.
  struct RecordingContribution;

  impl TurnContribution for RecordingContribution {
    fn class_name(&self) -> &'static str {
      "RecordingContribution"
    }

    fn after_turn<'a>(
      &'a self,
      meta: &'a SessionMetadata,
      _done_event: &'a Value,
      _cfg: &'a CharlieBotConfig,
      features: &'a Arc<FeatureContext>,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
      Box::pin(async move {
        let calls = features.startup.require::<CallLog>()?;
        calls.0.lock().unwrap().push(meta.id.clone());
        Ok(())
      })
    }
  }

  fn register_pair(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.turn_contribution("failing", Arc::new(FailingContribution))?;
    registry.turn_contribution("recording", Arc::new(RecordingContribution))
  }

  fn pair_registry() -> Arc<Registry> {
    let mut registry = Registry::new();
    registry.register_package("src.features.synthetic", register_pair).unwrap();
    Arc::new(registry)
  }

  /// One error event as the tracing layer saw it: its message and the text of each field.
  #[derive(Debug, Default)]
  struct ErrorEvent {
    message: String,
    fields: Vec<(&'static str, String)>,
  }

  impl ErrorEvent {
    fn field(&self, name: &str) -> &str {
      let (_, value) = self.fields.iter().find(|(key, _)| *key == name).expect("the event carries the field");
      value
    }
  }

  impl Visit for ErrorEvent {
    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
      let text = format!("{value:?}");
      if field.name() == "message" {
        self.message = text;
      } else {
        self.fields.push((field.name(), text));
      }
    }
  }

  /// Keeps each error event that the store's after-turn tasks log.
  struct ErrorCapture(Arc<Mutex<Vec<ErrorEvent>>>);

  impl EventSink for ErrorCapture {
    fn event(&self, event: &Event<'_>) {
      if *event.metadata().level() == Level::ERROR {
        let mut captured = ErrorEvent::default();
        event.record(&mut captured);
        self.0.lock().unwrap().push(captured);
      }
    }
  }

  /// Captures the events of one level logged on this test's thread through the shared test capture. The returned
  /// guard keeps the capture installed until it drops.
  fn capture_events_at(level: Level) -> (Arc<Mutex<Vec<ErrorEvent>>>, charliebot_core::test_support::CaptureGuard) {
    struct AtLevel(Arc<Mutex<Vec<ErrorEvent>>>, Level);

    impl EventSink for AtLevel {
      fn event(&self, event: &Event<'_>) {
        if *event.metadata().level() == self.1 {
          let mut captured = ErrorEvent::default();
          event.record(&mut captured);
          self.0.lock().unwrap().push(captured);
        }
      }
    }

    let events = Arc::new(Mutex::new(Vec::new()));
    let guard = charliebot_core::test_support::install_events(Arc::new(AtLevel(Arc::clone(&events), level)));
    (events, guard)
  }

  /// Captures the error events logged on this test's thread through the shared test capture. The returned guard keeps
  /// the capture installed.
  fn capture_errors() -> (Arc<Mutex<Vec<ErrorEvent>>>, charliebot_core::test_support::CaptureGuard) {
    let errors = Arc::new(Mutex::new(Vec::new()));
    let guard = charliebot_core::test_support::install_events(Arc::new(ErrorCapture(Arc::clone(&errors))));
    (errors, guard)
  }

  /// Writes the session's metadata file. The synthetic registry registers no metadata slots, so none is written.
  fn write_session(store: &Store, session_id: &str) {
    let meta = SessionMetadata::new(session_id, "Synthetic", TaskProfile::Manager);
    let path = store.metadata_path(session_id);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, meta.to_json_string(&[], true)).unwrap();
  }

  /// Ends one turn of the synthetic session: its metadata exists, then its master_done is persisted.
  async fn end_turn(store: &Store) {
    write_session(store, "synthetic");
    store.persist_and_broadcast("synthetic", json!({"type": "master_done"})).await.unwrap();
  }

  /// One synthetic lifecycle that keeps a `claude_account` label in the metadata extras.
  struct SyntheticAccountLifecycle;

  impl BackendLifecycle for SyntheticAccountLifecycle {
    fn account_source(&self) -> &'static str {
      "claude_account"
    }

    fn account_label(&self, meta: &SessionMetadata) -> Option<String> {
      meta.extra("claude_account").and_then(Value::as_str).map(str::to_string)
    }

    fn record_account_label(&self, meta: &mut SessionMetadata, label: Option<&str>) -> bool {
      let value = label.map_or(Value::Null, |label| Value::String(label.to_string()));
      if meta.extra("claude_account") == Some(&value) {
        return false;
      }
      meta.set_extra("claude_account", value);
      true
    }
  }

  /// Accepts every option mapping; the anchor-guard test builds no backend.
  fn accept_every_option(_: &serde_yaml::Mapping) -> Result<(), String> {
    Ok(())
  }

  struct NeverBuilt;

  impl BackendFactory for NeverBuilt {
    fn build(
      &self,
      _option: Option<&BackendOption>,
      _cfg: &CharlieBotConfig,
      _launch: LaunchArgs,
    ) -> anyhow::Result<Box<dyn AgentBackend>> {
      anyhow::bail!("the anchor-guard test builds no backend")
    }
  }

  /// A registry whose one backend type carries the synthetic account lifecycle.
  /// The session the guard reads from disk: one live anchor per protected field.
  fn anchored_session() -> SessionMetadata {
    let mut meta = SessionMetadata::new("synthetic", "Synthetic", TaskProfile::Manager);
    meta.cc_session_id = Some("cc-live".to_string());
    meta.set_extra("claude_account", json!("pool-b"));
    meta
  }

  fn anchor_registry() -> Arc<Registry> {
    let mut registry = Registry::new();
    registry
      .register_package("src.features.synthetic", |registry| {
        registry.backend_type(BackendTypeRegistration {
          type_name: "synthetic-lifecycle",
          traits: BackendTraits {
            resume: ResumeStyle::NativeId,
            restart_reattach: true,
            preassigned_session_id: false,
            family_prefix: None,
            manual_compact: false,
            compact_hint: None,
          },
          option_model: accept_every_option,
          factory: Arc::new(NeverBuilt),
          lifecycle: Some(Arc::new(SyntheticAccountLifecycle)),
          translate_fallback: false,
        })
      })
      .unwrap();
    Arc::new(registry)
  }

  #[tokio::test]
  async fn the_anchor_guard_corrects_an_unauthorized_save_back_to_disk() {
    // Python: tests/test_session_anchor_guard.py::test_whole_object_save_with_a_stale_label_is_corrected_back_to_disk
    let (warns, _guard) = capture_events_at(Level::WARN);
    let anchors = LifecycleAnchors { registry: anchor_registry() };
    let disk = anchored_session();
    let mut attempted = disk.clone();
    attempted.cc_session_id = Some("cc-stale".to_string());
    attempted.native_backend = Some("synthetic-stale".to_string());
    attempted.set_extra("claude_account", json!("pool-a"));

    anchors.reconcile(&disk, &mut attempted).unwrap();

    assert_eq!(attempted.cc_session_id.as_deref(), Some("cc-live"), "the stale save may not roll the anchor back");
    assert_eq!(attempted.native_backend, None);
    assert_eq!(attempted.extra("claude_account"), Some(&json!("pool-b")));
    let warns = warns.lock().unwrap();
    let corrections: Vec<&ErrorEvent> =
      warns.iter().filter(|event| event.message == "session_anchor_write_corrected").collect();
    assert_eq!(corrections.len(), 3, "{warns:?}");
    // The visitor Debug-formats each field, so `&'static str` field names keep their quotes.
    assert_eq!(corrections[0].field("field"), r#""cc_session_id""#);
    assert_eq!(corrections[0].field("on_disk"), r#"Some("cc-live")"#);
    assert_eq!(corrections[0].field("attempted"), r#"Some("cc-stale")"#);
    assert_eq!(corrections[1].field("field"), r#""native_backend""#);
    assert_eq!(corrections[2].field("field"), r#""claude_account""#);
    assert_eq!(corrections[2].field("on_disk"), r#"Some("pool-b")"#);
    assert_eq!(corrections[2].field("attempted"), r#"Some("pool-a")"#);
  }

  #[tokio::test]
  async fn the_authorized_channels_still_change_the_anchors() {
    // Python: tests/test_session_anchor_guard.py::test_authorized_channels_still_change_the_anchors
    let home = tempfile::tempdir().unwrap();
    let features = features_over(home.path(), anchor_registry());
    let store = &features.store;
    write_session(store, "synthetic");

    let read_back = store.persist_cc_session_id("synthetic", "cc-2", None).await.unwrap();
    assert_eq!(read_back.as_deref(), Some("cc-2"));
    let disk = store.read_metadata_fresh("synthetic").unwrap().unwrap();
    assert_eq!(disk.cc_session_id.as_deref(), Some("cc-2"));
    assert!(disk.cc_session_started_at.is_some());

    // The switch endpoint's backfill funnel records the producing backend.
    let read_back = store.persist_native_backend("synthetic", "codex-o3").await.unwrap();
    assert_eq!(read_back.as_deref(), Some("codex-o3"));
    let disk = store.read_metadata_fresh("synthetic").unwrap().unwrap();
    assert_eq!(disk.native_backend.as_deref(), Some("codex-o3"));
    assert_eq!(disk.cc_session_id.as_deref(), Some("cc-2"));
  }

  /// Yields until `done` holds. The store's after-turn tasks run on this test's current-thread runtime meanwhile.
  async fn settle(done: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
      while !done() {
        tokio::task::yield_now().await;
      }
    })
    .await
    .expect("the after-turn tasks finish within five seconds");
  }

  #[tokio::test]
  async fn master_done_runs_every_contribution_and_logs_the_failed_task_by_name() {
    let (errors, _guard) = capture_errors();
    let registry = pair_registry();
    let home = tempfile::tempdir().unwrap();
    let features = features_over(home.path(), Arc::clone(&registry));
    registry.bind_feature_context(&features).unwrap();
    features.startup.insert(CallLog::default()).unwrap();
    let calls = features.startup.require::<CallLog>().unwrap();

    end_turn(&features.store).await;
    settle(|| errors.lock().unwrap().len() == 1 && calls.0.lock().unwrap().len() == 1).await;

    let errors = errors.lock().unwrap();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].message, "background_task_failed");
    assert_eq!(errors[0].field("task_name"), "after-turn-FailingContribution-synthetic");
    assert!(errors[0].field("error").contains("synthetic after-turn failure"));
    assert_eq!(*calls.0.lock().unwrap(), ["synthetic"]);
  }

  #[tokio::test]
  async fn master_done_without_a_bound_context_fails_each_work_by_name() {
    let (errors, _guard) = capture_errors();
    let home = tempfile::tempdir().unwrap();
    let features = features_over(home.path(), pair_registry());

    end_turn(&features.store).await;
    settle(|| errors.lock().unwrap().len() == 2).await;

    assert_missing_context(&errors.lock().unwrap());
  }

  #[tokio::test]
  async fn master_done_after_the_bound_context_drops_fails_each_work_by_name() {
    let (errors, _guard) = capture_errors();
    let registry = pair_registry();
    let home = tempfile::tempdir().unwrap();
    let features = features_over(home.path(), Arc::clone(&registry));
    registry.bind_feature_context(&features).unwrap();
    // The store outlives the context only if the store holds no strong link to it, which is what this checks.
    let store = Arc::clone(&features.store);
    drop(features);

    end_turn(&store).await;
    settle(|| errors.lock().unwrap().len() == 2).await;

    assert_missing_context(&errors.lock().unwrap());
  }

  /// Each of the two works of a turn with no live context failed with the named error.
  fn assert_missing_context(errors: &[ErrorEvent]) {
    let mut names: Vec<&str> = errors.iter().map(|error| error.field("task_name")).collect();
    names.sort_unstable();
    assert_eq!(names, ["after-turn-FailingContribution-synthetic", "after-turn-RecordingContribution-synthetic"]);
    for error in errors {
      assert_eq!(error.message, "background_task_failed");
      assert!(error.field("error").contains("no FeatureContext is bound"));
    }
  }
}
