//! The pending-wake turn-end check against the cron feature's bound scheduled tasks. Each test writes task files into the
//! home's `config.d/cron.d` and the check reads them through the cron loader and the scheduler's next-fire code, as the
//! server does. The sessions are seeded as metadata files only; no schedule field is written by hand.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use charliebot_core::RepoRoot;
use charliebot_core::config::{CONFIG_FILENAME, CREDENTIALS_FILENAME};
use charliebot_core::credentials::load_credentials;
use charliebot_core::event_types::{CONFIG_RELOAD_FAILED_EVENT, CREDENTIALS_RELOAD_FAILED_EVENT};
use charliebot_core::hot_reload::HotReload;
use charliebot_registry::Registry;
use charliebot_registry::context::{FeatureContext, StartupState};
use charliebot_registry::store_hooks::store_hooks;
use charliebot_registry::turn::{CheckResult, TurnInput};
use charliebot_store::Store;
use charliebot_store::control_events::{ACTOR_USER, archived_close_event};
use charliebot_store::metadata::{SessionMetadata, TaskProfile};
use charliebot_triggers::triggers::TriggerManager;
use charliebot_web_reply::pending_wake::check_pending_wake;
use serde_json::json;
use tempfile::TempDir;

/// A synthetic home with the cron feature registered, and the startup state the cron loader and the pending-trigger
/// count read: the repo root the task files' prompts resolve under, and the trigger manager.
struct Home {
  _dir: TempDir,
  home: PathBuf,
  features: Arc<FeatureContext>,
}

impl Home {
  fn new() -> Home {
    let dir = tempfile::tempdir().expect("a temporary directory is available");
    let home = dir.path().join("home");
    let repo = dir.path().join("repo");
    fs::create_dir_all(home.join("config.d/cron.d")).expect("the cron directory");
    fs::create_dir_all(repo.join("prompts")).expect("the prompts directory");
    fs::write(repo.join("prompts/synthetic.md"), "Synthetic prompt\n").expect("the prompt file");

    let mut registry = Registry::new();
    registry.register_package("features.cron", charliebot_cron::register).expect("the cron package registers");
    let registry = Arc::new(registry);
    let cfg = registry.load_config(&home).expect("the synthetic config loads");
    let store = Store::new(cfg, registry.metadata_slots(), store_hooks(Arc::clone(&registry)))
      .expect("the store opens over the temporary home");
    let config_registry = Arc::clone(&registry);
    let config_home = home.clone();
    let credentials_home = home.clone();
    let features = Arc::new(FeatureContext {
      config: Arc::new(HotReload::new(
        home.join(CONFIG_FILENAME),
        CONFIG_RELOAD_FAILED_EVENT,
        Box::new(move || config_registry.load_config(&config_home)),
      )),
      credentials: Arc::new(HotReload::new(
        home.join(CREDENTIALS_FILENAME),
        CREDENTIALS_RELOAD_FAILED_EVENT,
        Box::new(move || load_credentials(&credentials_home)),
      )),
      store: Arc::new(store),
      registry,
      startup: Arc::new(StartupState::default()),
    });
    features
      .startup
      .insert(RepoRoot::new(Some(repo.into_os_string())).expect("the repo root is valid"))
      .expect("the repo root inserts");
    features.startup.insert(Arc::new(TriggerManager::new(Arc::clone(&features)))).expect("the trigger manager inserts");
    Home { _dir: dir, home, features }
  }

  /// Seeds one open manager session `id` under `parent`, with the metadata file the store reads.
  fn seed_manager(&self, id: &str, parent: Option<&str>) {
    let mut metadata = SessionMetadata::new(id, id, TaskProfile::Manager);
    metadata.backend = "synthetic-web".to_string();
    metadata.task_parent_id = parent.map(str::to_string);
    let path = self.features.store.metadata_path(id);
    fs::create_dir_all(path.parent().expect("a session directory")).expect("the session directory");
    fs::write(path, metadata.to_json_string(&self.features.registry.metadata_slots(), true)).expect("the metadata");
  }

  /// Writes the task file `name` in the home's cron directory: a master task on a synthetic prompt, bound to
  /// `session_id`, enabled or not.
  fn write_task(&self, name: &str, session_id: &str, enabled: bool) {
    let text = format!(
      "cron: \"0 9 * * *\"\ntimezone: UTC\nmode: master\nprompt_file: prompts/synthetic.md\nenabled: {enabled}\n\
       session_id: {session_id}\n"
    );
    self.write_raw_task(name, &text);
  }

  fn write_raw_task(&self, name: &str, text: &str) {
    fs::write(self.home.join("config.d/cron.d").join(format!("{name}.yaml")), text).expect("the task file");
  }

  /// Archives the task of `session_id` with its lifecycle fact, as a user's archive does. The scheduler reads that task
  /// state to skip each fire of a bound session, so the bound task holds no fire after this.
  async fn archive_task(&self, session_id: &str) {
    let guard = self.features.store.control_lock().await;
    let event = archived_close_event(
      session_id,
      "synthetic-archive",
      "archived by the user",
      None,
      None,
      None,
      ACTOR_USER,
      &[],
      &[],
    );
    self.features.store.append_control_fact(&guard, session_id, event).await.expect("the archive fact appends");
    drop(guard);
    self.features.store.invalidate_tree_index();
  }

  /// The check's answer for one web turn of `session_id`, the turn that ends with no Waiting on you item.
  async fn answer(&self, session_id: &str) -> CheckResult {
    check_pending_wake(&self.features, &web_turn(session_id)).await.expect("the check answers")
  }
}

/// A web turn with no Waiting on you item, whose only event is the user message it answers.
fn web_turn(session_id: &str) -> TurnInput {
  TurnInput {
    session_id: session_id.to_string(),
    colleague_read_only: false,
    parent_approved: false,
    exit_code: 0,
    continuation_failed: false,
    status: json!({"goal": "a goal", "waiting": []}),
    last_input_provenance: None,
    turn: vec![json!({"type": "user", "id": "u1", "content": "go on", "received_at": "2026-11-19T10:00:00Z"})],
  }
}

fn is_pass(result: &CheckResult) -> bool {
  matches!(result, CheckResult::Pass)
}

fn is_block(result: &CheckResult) -> bool {
  matches!(result, CheckResult::Block(_))
}

#[tokio::test]
async fn an_enabled_task_bound_to_the_session_passes() {
  let home = Home::new();
  home.seed_manager("root", None);
  home.write_task("bound-task", "root", true);
  let result = home.answer("root").await;
  assert!(is_pass(&result), "{result:?}");
}

#[tokio::test]
async fn a_disabled_task_bound_to_the_session_blocks() {
  let home = Home::new();
  home.seed_manager("root", None);
  home.write_task("bound-task", "root", false);
  let result = home.answer("root").await;
  assert!(is_block(&result), "{result:?}");
}

#[tokio::test]
async fn an_enabled_task_bound_to_an_open_child_passes_the_parent_turn() {
  let home = Home::new();
  home.seed_manager("root", None);
  home.seed_manager("child", Some("root"));
  home.write_task("child-task", "child", true);
  let result = home.answer("root").await;
  assert!(is_pass(&result), "{result:?}");
}

#[tokio::test]
async fn a_task_bound_to_an_archived_session_blocks() {
  let home = Home::new();
  home.seed_manager("root", None);
  home.write_task("bound-task", "root", true);
  home.archive_task("root").await;
  let result = home.answer("root").await;
  assert!(is_block(&result), "{result:?}");
}

#[tokio::test]
async fn a_malformed_task_file_holds_no_wake_and_blocks() {
  let home = Home::new();
  home.seed_manager("root", None);
  home.write_raw_task("broken-task", "cron: [unclosed\nsession_id: root\n");
  let result = home.answer("root").await;
  assert!(is_block(&result), "{result:?}");
}

/// The check's time for a turn that leaves no wake under 45 open children: the whole subtree is read, and the entry must
/// answer within 2 s. A task bound to a session outside the subtree keeps the cron read live without a wake. Run with
/// `--ignored --nocapture` to see the timings.
#[tokio::test]
#[ignore = "integration"]
async fn a_turn_with_45_open_children_answers_within_two_seconds() {
  let home = Home::new();
  home.seed_manager("root", None);
  for index in 0..45 {
    home.seed_manager(&format!("child-{index:02}"), Some("root"));
  }
  home.seed_manager("elsewhere", None);
  home.write_task("bound-elsewhere", "elsewhere", true);
  let mut timings = Vec::new();
  for _ in 0..5 {
    let started = Instant::now();
    let result = home.answer("root").await;
    timings.push(started.elapsed());
    assert!(is_block(&result), "{result:?}");
  }
  let longest = timings.iter().max().copied().unwrap_or_default();
  eprintln!("pending-wake entry, 45 open children: {timings:?}");
  assert!(longest < Duration::from_secs(2), "the entry took {longest:?}");
}
