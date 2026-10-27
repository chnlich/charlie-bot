//! The task-tree prompt contract over a real store: one assembler at every depth, inherited subtree rules without
//! ancestor node-rule or sibling leakage, the overlay states of a launch, and the historical run context.
//!
//! Each test builds a task tree in its own temporary home and reads it through `charliebot_store`. The prompt
//! references are written the way the store's prompt-change path writes them. No agent CLI and no server starts.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use charliebot_core::RepoRoot;
use charliebot_core::config::{
  BackendOption, BackendsConfig, CharlieBotConfig, PathsConfig, Sections, ServerConfig, TelegramConfig, UiConfig,
};
use charliebot_registry::Registry;
use charliebot_store::metadata::{SessionMetadata, TaskProfile, TaskSpec, TaskType, UtcDateTime};
use charliebot_store::{
  AccountLabelKeeper, AnchorFieldReconciler, CreateTask, EmptyRuntimeEventHooks, RunRecord, SequenceArchiveHook,
  SequenceBindingMover, SequenceListingFields, Store, StoreHookFuture, StoreHooks, StoreResult, TaskCaller,
};
use charliebot_tasks::prompts::{
  CoherentSnapshot, PromptEnv, PromptSnapshot, RuleInputs, SNAPSHOT_FILENAME, assemble_coherent_snapshot,
  assemble_snapshot, build_segments, capture_prompt_chain, prompt_task_type, read_run_context, resolve_launch_overlay,
};
use serde_json::{Map, Value, json};
use tempfile::TempDir;

const BACKEND_ID: &str = "golden";

struct NoHooks;

impl AccountLabelKeeper for NoHooks {
  fn record_account_label(&self, _meta: &mut SessionMetadata, _label: Option<&str>) -> StoreResult<bool> {
    Ok(false)
  }

  fn account_label(&self, _meta: &SessionMetadata) -> Option<String> {
    None
  }
}

impl AnchorFieldReconciler for NoHooks {
  fn reconcile(&self, disk: &SessionMetadata, attempted: &mut SessionMetadata) -> StoreResult<()> {
    attempted.cc_session_id = disk.cc_session_id.clone();
    attempted.cc_session_started_at = disk.cc_session_started_at;
    attempted.native_backend = disk.native_backend.clone();
    Ok(())
  }
}

impl SequenceArchiveHook for NoHooks {
  fn on_archive<'a>(&'a self, _session_id: &'a str) -> StoreHookFuture<'a, ()> {
    Box::pin(async { Ok(()) })
  }
}

impl SequenceBindingMover for NoHooks {
  fn move_bindings<'a>(&'a self, _source_session_id: &'a str, _new_session_id: &'a str) -> StoreHookFuture<'a, bool> {
    Box::pin(async { Ok(false) })
  }
}

impl SequenceListingFields for NoHooks {
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
}

fn config(home: &Path) -> CharlieBotConfig {
  CharlieBotConfig {
    charliebot_home: home.to_path_buf(),
    headless_chrome_bin: "/bin/false".to_string(),
    server: ServerConfig {
      host: "127.0.0.1".to_string(),
      port: 18651,
      subprocess_buffer_limit_mb: 1,
      session_memory_max_mb: 0,
      session_swap_max_mb: 0,
      session_tasks_max: 0,
      min_free_disk_gib: 0,
    },
    paths: PathsConfig {
      workspace_dirs: Vec::new(),
      worktree_dir: "/synthetic/worktrees".to_string(),
      scratch_dir: "/synthetic/scripts".to_string(),
    },
    backends: BackendsConfig { pool: vec![BACKEND_ID.to_string()], routing: None, options: vec![backend_option(None)] },
    ui: UiConfig { home_services: Vec::new() },
    telegram: TelegramConfig { chat_id: None },
    sections: Sections::default(),
  }
}

fn backend_option(prompt_overlay: Option<&str>) -> BackendOption {
  BackendOption {
    id: BACKEND_ID.to_string(),
    label: "Golden".to_string(),
    model: "golden-model".to_string(),
    prompt_overlay: prompt_overlay.map(String::from),
    backend_type: "codex".to_string(),
    type_fields: serde_yaml::Mapping::new(),
  }
}

/// A store over a temporary home, with the repository checkout the prompt templates come from.
struct Harness {
  _home: TempDir,
  cfg: CharlieBotConfig,
  registry: Registry,
  store: Store,
  checkout: PathBuf,
  repo_root: RepoRoot,
}

/// The ids of the three-level tree every tree test builds.
struct Tree {
  root: String,
  mid: String,
  low: String,
  worker1: String,
}

impl Harness {
  fn new() -> Self {
    let home = tempfile::tempdir().expect("temp home");
    let cfg = config(home.path());
    let registry = Registry::new();
    let hooks = StoreHooks {
      account_labels: Some(Arc::new(NoHooks)),
      anchor_reconciler: Some(Arc::new(NoHooks)),
      sequence_listing: Some(Arc::new(NoHooks)),
      sequence_archive: Some(Arc::new(NoHooks)),
      sequence_binding_mover: Some(Arc::new(NoHooks)),
      runtime_events: Some(Arc::new(EmptyRuntimeEventHooks)),
    };
    let store = Store::new(cfg.clone(), registry.metadata_slots(), hooks).expect("store opens over the temp home");
    let checkout = PathBuf::from(env!("CARGO_MANIFEST_DIR")).ancestors().nth(4).expect("repo root").to_path_buf();
    let repo_root = RepoRoot::new(Some(checkout.as_os_str().to_os_string())).expect("repo root is valid");
    Self { _home: home, cfg, registry, store, checkout, repo_root }
  }

  /// Opens a store over the same home. Its metadata cache starts empty, so it reads the files this harness wrote.
  fn reopen_store(&mut self) {
    let hooks = StoreHooks {
      account_labels: Some(Arc::new(NoHooks)),
      anchor_reconciler: Some(Arc::new(NoHooks)),
      sequence_listing: Some(Arc::new(NoHooks)),
      sequence_archive: Some(Arc::new(NoHooks)),
      sequence_binding_mover: Some(Arc::new(NoHooks)),
      runtime_events: Some(Arc::new(EmptyRuntimeEventHooks)),
    };
    self.store = Store::new(self.cfg.clone(), self.registry.metadata_slots(), hooks).expect("store reopens");
  }

  fn env(&self) -> PromptEnv<'_> {
    PromptEnv { cfg: &self.cfg, registry: &self.registry, checkout: &self.repo_root }
  }

  async fn node(
    &self,
    request_id: &str,
    parent: Option<&str>,
    profile: TaskProfile,
    task: Option<TaskSpec>,
    name: &str,
  ) -> SessionMetadata {
    let request = CreateTask {
      request_id: request_id.to_string(),
      task_parent_id: parent.map(String::from),
      profile,
      task,
      name: Some(name.to_string()),
      backend: Some(BACKEND_ID.to_string()),
      group: None,
      session_id: None,
    };
    self.store.create_task(request, TaskCaller::Operator, Map::new()).await.expect("task node is created")
  }

  async fn tree(&self) -> Tree {
    let root = self.node("root", None, TaskProfile::Manager, None, "Root").await;
    let mid = self.node("mid", Some(&root.id), TaskProfile::Manager, None, "Mid").await;
    let low = self.node("low", Some(&mid.id), TaskProfile::Manager, None, "Low").await;
    let worker1 = self
      .node("w1", Some(&low.id), TaskProfile::Worker, Some(worker_task("worker goal", Some("/tmp/repo-w"))), "W1")
      .await;
    // The sibling's goal must never reach the worker's instructions.
    self.node("w2", Some(&low.id), TaskProfile::Worker, Some(worker_task("sibling goal", None)), "W2").await;
    Tree { root: root.id, mid: mid.id, low: low.id, worker1: worker1.id }
  }

  /// Stores the bodies, then writes the node's refs into its metadata file. The refs have no store writer yet, so the
  /// file is written directly and the store is reopened, which drops the metadata cache that held the old copy.
  async fn set_refs(&mut self, session_id: &str, subtree: Option<&str>, node: Option<&str>) {
    let mut meta = self.store.load_meta(session_id).await.expect("metadata reads").expect("the node exists");
    meta.subtree_prompt_ref = subtree.map(|body| self.store.store_prompt_body(body).expect("body stores"));
    meta.node_prompt_ref = node.map(|body| self.store.store_prompt_body(body).expect("body stores"));
    fs::write(self.store.metadata_path(session_id), meta.to_json_string(&self.registry.metadata_slots(), true))
      .expect("metadata writes");
    self.reopen_store();
  }

  /// The snapshot a launch of `kind` for `session_id` would assemble, with the chain captured under the lock.
  async fn snapshot_for(&self, session_id: &str, kind: &str) -> PromptSnapshot {
    let guard = self.store.control_lock().await;
    let meta = self.store.load_meta(session_id).await.expect("metadata reads").expect("the node exists");
    let (chain, node_ref) = capture_prompt_chain(&self.store, &guard, &meta).await.expect("chain captures");
    drop(guard);
    let inputs = RuleInputs { overlay: None, chain: &chain, node_ref: node_ref.as_deref() };
    let (segments, _overlay_error) = build_segments(&self.env(), &meta, kind, &inputs).expect("segments build");
    assemble_snapshot(&segments)
  }
}

fn worker_task(goal: &str, repo_path: Option<&str>) -> TaskSpec {
  TaskSpec { goal: goal.to_string(), repo_path: repo_path.map(String::from), ..TaskSpec::default() }
}

fn sources_of(snapshot: &PromptSnapshot) -> Vec<(String, String, Option<String>)> {
  snapshot
    .blocks
    .iter()
    .flat_map(|block| block.sources.iter())
    .map(|source| (source.scope.clone(), source.source_ref.clone(), source.source_session_id.clone()))
    .collect()
}

fn refs_of(snapshot: &PromptSnapshot) -> Vec<String> {
  sources_of(snapshot).into_iter().map(|(_, source_ref, _)| source_ref).collect()
}

fn has_ref(snapshot: &PromptSnapshot, reference: &str) -> bool {
  refs_of(snapshot).iter().any(|source_ref| source_ref == reference)
}

fn position(snapshot: &PromptSnapshot, reference: &str) -> usize {
  refs_of(snapshot).iter().position(|source_ref| source_ref == reference).expect("the reference is in the snapshot")
}

#[tokio::test]
async fn manager_template_is_identical_at_every_depth_with_no_pm_load() {
  let harness = Harness::new();
  let tree = harness.tree().await;
  let mut built = Vec::new();
  for id in [&tree.root, &tree.mid, &tree.low] {
    built.push(harness.snapshot_for(id, "manager_turn").await);
  }
  for snapshot in &built {
    assert!(has_ref(snapshot, "prompts/task_manager.md"));
    assert!(has_ref(snapshot, "prompts/task_base.md"));
    assert!(!snapshot.instructions_text().contains("PM identity"));
  }
  assert_eq!(built[1].blocks, built[0].blocks);
  assert_eq!(built[2].blocks, built[0].blocks);
}

#[tokio::test]
async fn manager_prompt_carries_the_shared_master_rules_in_order() {
  let harness = Harness::new();
  let tree = harness.tree().await;
  let snapshot = harness.snapshot_for(&tree.root, "manager_turn").await;
  assert!(position(&snapshot, "prompts/task_base.md") < position(&snapshot, "prompts/master.md"));
  assert!(position(&snapshot, "prompts/master.md") < position(&snapshot, "prompts/task_manager.md"));
  assert_eq!(position(&snapshot, "prompts/manager_workflows.md"), position(&snapshot, "prompts/master.md") + 1);
  let joined = snapshot.instructions_text();
  assert!(joined.contains("Direct work and delegation divide by where the change lands."));
  assert!(joined.contains("Repository implementation stays with worker leaves at every manager depth."));
  for filename in ["master.md", "manager_workflows.md"] {
    let text = fs::read_to_string(harness.checkout.join("prompts").join(filename)).expect("template reads");
    for line in text.lines().filter(|line| line.starts_with("## ")) {
      assert!(joined.contains(line), "{line}");
    }
  }
}

#[tokio::test]
async fn default_empty_local_rules_launch_cleanly() {
  let harness = Harness::new();
  let tree = harness.tree().await;
  for id in [&tree.root, &tree.worker1] {
    let meta = harness.store.load_meta(id).await.expect("metadata reads").expect("the node exists");
    assert!(meta.subtree_prompt_ref.is_none() && meta.node_prompt_ref.is_none());
    let snapshot = harness.snapshot_for(id, "manager_turn").await;
    assert!(!snapshot.blocks.is_empty());
    assert!(sources_of(&snapshot).iter().all(|(scope, _, _)| scope != "subtree" && scope != "node"));
  }
}

#[tokio::test]
async fn worker_kinds_get_their_applicable_contracts() {
  let harness = Harness::new();
  let tree = harness.tree().await;
  for kind in ["work", "review", "iteration", "scheduled_step"] {
    let snapshot = harness.snapshot_for(&tree.worker1, kind).await;
    assert!(has_ref(&snapshot, "prompts/task_base.md"), "{kind}");
    if kind == "review" {
      assert!(has_ref(&snapshot, "src/runtime/review.py:review_rules_text"), "{kind}");
    } else {
      assert!(has_ref(&snapshot, "prompts/worker.md"), "{kind}");
    }
    assert!(!has_ref(&snapshot, "prompts/verify.md"), "{kind}");
  }
}

#[tokio::test]
async fn verify_task_gets_the_verify_contract() {
  let harness = Harness::new();
  let tree = harness.tree().await;
  let verify = harness
    .node(
      "verify-leaf",
      Some(&tree.low),
      TaskProfile::Worker,
      Some(TaskSpec { goal: "verify goal".to_string(), task_type: Some(TaskType::Verify), ..TaskSpec::default() }),
      "V",
    )
    .await;
  let snapshot = harness.snapshot_for(&verify.id, "work").await;
  assert!(has_ref(&snapshot, "prompts/verify.md"));
  assert!(!has_ref(&snapshot, "prompts/worker.md"));
}

#[test]
fn type_less_task_renders_the_implement_contract() {
  assert_eq!(prompt_task_type(None), TaskType::Implement);
  assert_eq!(prompt_task_type(Some(&worker_task("sweep", None))), TaskType::Implement);
  let quick = TaskSpec { goal: "bump".to_string(), task_type: Some(TaskType::QuickEdit), ..TaskSpec::default() };
  assert_eq!(prompt_task_type(Some(&quick)), TaskType::QuickEdit);
}

#[tokio::test]
async fn three_levels_with_both_scopes_prove_inheritance_and_ordering() {
  let mut harness = Harness::new();
  let tree = harness.tree().await;
  harness.set_refs(&tree.root, Some("root subtree rule"), Some("root node rule")).await;
  harness.set_refs(&tree.mid, Some("mid subtree rule"), Some("mid node rule")).await;
  harness.set_refs(&tree.low, None, Some("low node rule")).await;

  let worker = harness.snapshot_for(&tree.worker1, "work").await;
  let scoped: Vec<(String, Option<String>)> = sources_of(&worker)
    .into_iter()
    .filter(|(scope, _, _)| scope == "subtree" || scope == "node")
    .map(|(scope, _, owner)| (scope, owner))
    .collect();
  assert_eq!(
    scoped,
    [("subtree".to_string(), Some(tree.root.clone())), ("subtree".to_string(), Some(tree.mid.clone()))]
  );
  let subtree_texts: Vec<&str> = worker
    .blocks
    .iter()
    .filter(|block| block.sources.iter().any(|source| source.scope == "subtree"))
    .map(|block| block.text.as_str())
    .collect();
  assert_eq!(subtree_texts, ["root subtree rule", "mid subtree rule"]);
  let joined = worker.instructions_text();
  assert!(!joined.contains("root node rule"));
  assert!(!joined.contains("mid node rule"));
  assert!(!joined.contains("sibling goal"));

  let mid = harness.snapshot_for(&tree.mid, "manager_turn").await;
  let mid_scoped: Vec<(String, Option<String>)> = sources_of(&mid)
    .into_iter()
    .filter(|(scope, _, _)| scope == "subtree" || scope == "node")
    .map(|(scope, _, owner)| (scope, owner))
    .collect();
  assert_eq!(
    mid_scoped,
    [
      ("subtree".to_string(), Some(tree.root.clone())),
      ("subtree".to_string(), Some(tree.mid.clone())),
      ("node".to_string(), Some(tree.mid.clone())),
    ]
  );
}

#[tokio::test]
async fn manager_prompt_fills_the_agent_message_limit() {
  let harness = Harness::new();
  let root = harness.node("root", None, TaskProfile::Manager, None, "Root").await;
  let snapshot = harness.snapshot_for(&root.id, "manager_turn").await;
  let joined = snapshot.instructions_text();
  assert!(joined.contains("`charliebot session send` refuses a message over 2000 weighted characters."));
  assert!(!joined.contains("{{agent_message_limit}}"));
}

#[tokio::test]
async fn overlay_states_of_a_launch_are_judged_and_reported() {
  let harness = Harness::new();
  let tree = harness.tree().await;
  let meta = harness.store.load_meta(&tree.root).await.expect("metadata reads").expect("the node exists");

  let undeclared = assemble_overlay(&harness, &meta, None).await;
  assert!(!undeclared.declared);
  assert!(undeclared.overlay_error.is_none());

  let none = assemble_overlay(&harness, &meta, Some("none")).await;
  assert!(none.declared);
  assert!(none.overlay_error.is_none());

  let present = assemble_overlay(&harness, &meta, Some("Kimi-K3")).await;
  assert!(present.declared);
  assert!(present.overlay_error.is_none());
  assert!(has_ref(&present.snapshot, "prompts/model_overlays/Kimi-K3.md"));

  let missing = assemble_overlay(&harness, &meta, Some("no-such-overlay")).await;
  assert!(missing.declared);
  assert_eq!(missing.overlay_error.as_ref().map(|error| error.python_class_name()), Some("FileNotFoundError"));
  assert!(!has_ref(&missing.snapshot, "prompts/model_overlays/no-such-overlay.md"));
  assert_eq!(resolve_launch_overlay(&backend_option(Some("none"))), (None, true));
}

/// The coherent snapshot of a manager launch under `overlay`, as the launch path assembles it.
async fn assemble_overlay(harness: &Harness, meta: &SessionMetadata, overlay: Option<&str>) -> CoherentSnapshot {
  let option = backend_option(overlay);
  assemble_coherent_snapshot(&harness.store, &harness.env(), meta, "manager_turn", &option)
    .await
    .expect("the snapshot assembles")
}

/// Registers a run pinned to `task_spec` under the control lock, the way the launch path does.
async fn register_run(harness: &Harness, session_id: &str, run_id: &str, task_spec: Option<&str>) {
  let run: RunRecord =
    serde_json::from_value(json!({"id": run_id, "session_id": session_id, "kind": "work"})).expect("a run record");
  let guard = harness.store.control_lock().await;
  harness.store.register_run_locked(&guard, session_id, run, task_spec).await.expect("the run registers");
}

async fn record_snapshot_ref(harness: &Harness, session_id: &str, run_id: &str, path: &Path) {
  let fields: Map<String, Value> =
    json!({"prompt_snapshot_ref": path.display().to_string()}).as_object().cloned().expect("an object");
  harness.store.record_observation(session_id, run_id, fields).await.expect("the snapshot ref records");
}

#[tokio::test]
async fn run_context_returns_the_stored_snapshot() {
  let mut harness = Harness::new();
  let tree = harness.tree().await;
  harness.set_refs(&tree.root, Some("the root subtree rule"), None).await;
  register_run(&harness, &tree.worker1, "ctx-run", Some("pinned spec")).await;
  let snapshot = harness.snapshot_for(&tree.worker1, "work").await;
  let path = harness.store.run_dir(&tree.worker1, "ctx-run").join(SNAPSHOT_FILENAME);
  fs::create_dir_all(path.parent().expect("run directory")).expect("run directory");
  fs::write(&path, serde_json::to_string_pretty(&snapshot.to_json_dict()).expect("json")).expect("snapshot writes");
  record_snapshot_ref(&harness, &tree.worker1, "ctx-run", &path).await;

  let run = harness.store.read_run_sync(&tree.worker1, "ctx-run").expect("run reads").expect("the run exists");
  let payload = read_run_context(&harness.store, &tree.worker1, "ctx-run").expect("the context reads");
  assert_eq!(payload["mode"], "historical");
  assert_eq!(payload["snapshot"]["prompt_hash"], json!(snapshot.prompt_hash()));
  assert_eq!(payload["snapshot"]["char_count"], json!(snapshot.char_count()));
  assert!(payload.get("legacy_prompt").is_none());
  assert_eq!(payload["task_spec"], json!({"ref": run.task_spec_ref, "sha256": run.task_spec_hash}));
  let logs: Vec<&str> = payload["logs"].as_object().expect("logs").keys().map(String::as_str).collect();
  assert_eq!(logs, ["raw_log_ref", "events_ref", "result_ref"]);
  assert_eq!(read_run_context(&harness.store, &tree.worker1, "ctx-run").expect("reads again"), payload);
}

#[tokio::test]
async fn corrupt_new_snapshot_is_an_error() {
  let harness = Harness::new();
  let tree = harness.tree().await;
  register_run(&harness, &tree.worker1, "corrupt-snapshot", None).await;
  let path = harness.store.run_dir(&tree.worker1, "corrupt-snapshot").join(SNAPSHOT_FILENAME);
  fs::create_dir_all(path.parent().expect("run directory")).expect("run directory");
  fs::write(&path, "not json at all").expect("snapshot writes");
  record_snapshot_ref(&harness, &tree.worker1, "corrupt-snapshot", &path).await;

  let error =
    read_run_context(&harness.store, &tree.worker1, "corrupt-snapshot").expect_err("a corrupt snapshot fails");
  assert_eq!(error.status, 500);
  assert_eq!(
    error.detail,
    format!("stored prompt snapshot unreadable at {}: Expecting value: line 1 column 1 (char 0)", path.display())
  );
}

#[tokio::test]
async fn unknown_run_is_a_404_with_the_task_text() {
  let harness = Harness::new();
  let tree = harness.tree().await;
  let error = read_run_context(&harness.store, &tree.worker1, "no-such-run").expect_err("the run is missing");
  assert_eq!(error.status, 404);
  assert_eq!(error.detail, format!("run no-such-run not found in task {}", tree.worker1));
}
