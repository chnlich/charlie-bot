//! The reply-contract cases and the port of `tests/core/test_thread_entry.py`. The cases run under a synthetic second
//! platform, "fakechat", with snowflake-ordered ids: the real store, trigger manager and dispatcher run over a temporary
//! home, and a recording executor stands where the agent launch would go. No agent starts.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::StatusCode;
use charliebot_core::RepoRoot;
use charliebot_core::config::BackendOption;
use charliebot_core::credentials::load_credentials;
use charliebot_core::event_types;
use charliebot_core::hot_reload::HotReload;
use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot, SlotField};
use charliebot_registry::store_hooks::store_hooks;
use charliebot_registry::{FeatureContext, Registry, RegistryError, RunningTurnInputs, StartupState};
use charliebot_sched::triggers::{PendingTrigger, TriggerManager};
use charliebot_store::metadata::{SessionMetadata, SessionStatus, TaskProfile};
use charliebot_store::{CreateTask, SIDEBAR_CHANNEL, Store, StoreResult, Subscription, TaskCaller};
use charliebot_tasks::completion::TaskCompletion;
use charliebot_tasks::dispatch::{DispatchExecutor, Dispatcher};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use tempfile::TempDir;

use super::*;

const SCOPE: &str = "fakechat_reply_scope.md";

/// The synthetic platform the cases run under. Its ids sort as integers, so "100" sorts above "99".
const FAKECHAT: ThreadPlatform = ThreadPlatform {
  name: "fakechat",
  display_name: "Fakechat",
  reply_event_type: "fakechat_reply",
  reply_command: "charliebot fakechat reply",
  max_post_chars: 2000,
  scope_doc: SCOPE,
  follow_trigger_prefix: "fakechat-thread-follow",
  id_key: IdOrdering::Snowflake,
  origin_field: "fakechat_origin",
  watermark_field: "fakechat_watermark_id",
  id_label: "id",
  mention_key: "mention_id",
  block_keys: &["channel_id", "thread_ts", "mention_id"],
  summon_line: "fakechat summon",
};

/// The summon block every summon-side case sends.
fn summon_block() -> Value {
  json!({"channel_id": "c1", "thread_ts": "t1", "mention_id": "m1"})
}

/// A fakechat summon injection as the log holds it.
fn summon_event(event_id: &str) -> Value {
  json!({"id": event_id, "type": event_types::AGENT_MESSAGE, "content": "summon", "fakechat": {"thread": "t1"}})
}

/// The arguments of one fakechat summon into session `s1`, borrowed from the caller's owned origin and block.
fn summon<'a>(origin: &'a Value, block: &'a Value) -> Summon<'a> {
  Summon { session_id: "s1", label: "Fakechat #c1", origin, block, content: "fakechat summon", user: Some("u1") }
}

/// The origin of the synthetic thread the cases bind a session to.
fn origin() -> Value {
  json!({"channel_id": "c1", "thread_ts": "t1"})
}

#[tokio::test]
async fn thread_event_reads_share_the_store_snapshot() {
  let home = Home::new();
  home.thread_session("shared-thread-events", origin()).await;
  home.append("shared-thread-events", json!({"type": "user", "content": "question"})).await;

  let first = load_chat_events(&home.features, "shared-thread-events").await.expect("the events read");
  let second = load_chat_events(&home.features, "shared-thread-events").await.expect("the events read again");

  assert!(Arc::ptr_eq(&first, &second), "both calls hold the same event allocation");
}

#[tokio::test]
async fn concurrent_thread_reply_reuses_one_keyed_delivery_and_receipt() {
  let home = Home::new();
  home.thread_session("retry-thread-reply", origin()).await;
  let adapter = FakeAdapter::with_thread(&[]);
  let dyn_adapter: Arc<dyn ThreadAdapter> = adapter.clone();
  let (first, second) = tokio::join!(
    post_reply_with_request_id(&dyn_adapter, &home.features, "retry-thread-reply", "the answer", Some("reply-run")),
    post_reply_with_request_id(&dyn_adapter, &home.features, "retry-thread-reply", "the answer", Some("reply-run")),
  );

  assert_eq!(first, second);
  assert_eq!(adapter.posts.lock().unwrap().len(), 1);
  let replies: Vec<_> = home
    .events("retry-thread-reply")
    .into_iter()
    .filter(|event| event.get("type").and_then(Value::as_str) == Some(FAKECHAT.reply_event_type))
    .collect();
  assert_eq!(replies.len(), 1);
  assert_eq!(replies[0]["request_id"], "reply-run");
}

/// The chat-event type of a fakechat reply event, as the platform writes it.
fn reply_type() -> &'static str {
  FAKECHAT.reply_event_type
}

fn register_fakechat(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.metadata_slot(MetadataSlot {
    owner: "fakechat",
    on: MetadataFile::Session,
    after: None,
    fields: vec![
      SlotField { name: "fakechat_origin", default: Value::Null },
      SlotField { name: "fakechat_watermark_id", default: Value::Null },
    ],
  })
}

/// Records each launch the dispatcher asks for, and launches nothing.
#[derive(Default)]
struct RecordingExecutor {
  launches: Mutex<Vec<(String, Vec<Value>)>>,
}

impl DispatchExecutor for RecordingExecutor {
  fn launch<'a>(
    &'a self,
    session_id: &'a str,
    pending_inputs: &'a [Value],
    _launch_run_id: Option<&'a str>,
  ) -> BoxFuture<'a, StoreResult<Option<String>>> {
    Box::pin(async move {
      self
        .launches
        .lock()
        .expect("the launch log is never poisoned")
        .push((session_id.to_string(), pending_inputs.to_vec()));
      Ok(None)
    })
  }
}

/// A store, trigger manager and dispatcher over a temporary home, with the fakechat slot registered.
struct Home {
  _dir: TempDir,
  features: Arc<FeatureContext>,
  executor: Arc<RecordingExecutor>,
}

impl Home {
  fn new() -> Self {
    Self::build(None)
  }

  fn with_router(command: Vec<String>) -> Self {
    Self::build(Some(command))
  }

  fn build(command: Option<Vec<String>>) -> Self {
    let dir = tempfile::tempdir().expect("temporary directory exists");
    let mut registry = Registry::new();
    registry.register_package("tests.fakechat", register_fakechat).expect("the fakechat slot registers");
    registry
      .register_package("src.runtime.backend_pool", backend_pool::register)
      .expect("the backend pool metadata slot registers");
    let registry = Arc::new(registry);
    let mut loaded = registry.load_config(dir.path()).expect("an empty home loads with the defaults");
    // Task creation takes the first configured backend when a create names none.
    loaded.backends.options.push(BackendOption {
      id: "synthetic-fakechat".to_string(),
      label: "Synthetic fakechat".to_string(),
      model: "synthetic-model".to_string(),
      prompt_overlay: None,
      backend_type: "synthetic".to_string(),
      type_fields: Default::default(),
    });
    loaded.backends.pool = vec!["synthetic-fakechat".to_string()];
    loaded.backends.options.push(BackendOption {
      id: "synthetic-strong".to_string(),
      label: "Synthetic strong".to_string(),
      model: "synthetic-model".to_string(),
      prompt_overlay: None,
      backend_type: "synthetic".to_string(),
      type_fields: Default::default(),
    });
    loaded.backends.routing = command.map(|command| charliebot_core::config::RoutingConfig { command });
    let reload_config = loaded.clone();
    let config = Arc::new(HotReload::new(
      loaded.config_file(),
      "chat_threads_test_config_reload_failed",
      Box::new(move || Ok(reload_config.clone())),
    ));
    let store = Arc::new(
      Store::new(loaded, registry.metadata_slots(), store_hooks(Arc::clone(&registry))).expect("the store opens"),
    );
    let credential_home = dir.path().to_path_buf();
    let credentials = Arc::new(HotReload::new(
      dir.path().join("credentials.yaml"),
      "chat_threads_test_credentials_reload_failed",
      Box::new(move || load_credentials(&credential_home)),
    ));
    let startup = StartupState::default();
    startup.insert(RunningTurnInputs::new(|_| Vec::new())).expect("the stub inserts once");
    startup.insert(backend_pool::BackendPool::default()).expect("the backend pool inserts once");
    let features = Arc::new(FeatureContext { config, credentials, store, registry, startup: Arc::new(startup) });

    features.startup.insert(TaskCompletion::new(Arc::clone(&features))).expect("the completion inserts once");
    let completion = features.startup.require::<TaskCompletion>().expect("the completion is in startup");
    let sequence: Arc<dyn charliebot_registry::sequence::SequenceCompletion> = completion.clone();
    let run: Arc<dyn charliebot_tasks::dispatch::RunFinishedCompletion> = completion;
    let executor = Arc::new(RecordingExecutor::default());
    let dispatcher = Dispatcher::new(Arc::clone(&features.store), Arc::clone(&features), sequence, run);
    let dyn_executor: Arc<dyn DispatchExecutor> = executor.clone();
    dispatcher.set_executor(Some(dyn_executor));
    features.startup.insert(dispatcher).expect("the dispatcher inserts once");
    features
      .startup
      .insert(Arc::new(TriggerManager::new(Arc::clone(&features))))
      .expect("the trigger manager inserts once");
    Self { _dir: dir, features, executor }
  }

  /// A manager session `session_id`, created as the summon path creates one, with `origin` in its slot.
  async fn thread_session(&self, session_id: &str, origin: Value) {
    let request = CreateTask {
      request_id: format!("root-{session_id}"),
      task_parent_id: None,
      profile: TaskProfile::Manager,
      task: None,
      name: Some(format!("session {session_id}")),
      backend: None,
      group: None,
      session_id: Some(session_id.to_string()),
    };
    let slots = Map::from_iter([("fakechat_origin".to_string(), origin)]);
    self.features.store.create_task(request, TaskCaller::System, slots).await.expect("the session is created");
  }

  async fn meta(&self, session_id: &str) -> SessionMetadata {
    self.features.store.get_session(session_id).await.expect("the metadata reads").expect("the session exists")
  }

  fn events(&self, session_id: &str) -> Vec<Value> {
    self.features.store.load_chat_events_sync(session_id).expect("the chat log reads")
  }

  async fn append(&self, session_id: &str, event: Value) -> Value {
    self.features.store.append_chat_event(session_id, event).await.expect("the chat event appends")
  }

  async fn triggers(&self, session_id: &str) -> Vec<PendingTrigger> {
    let manager = self.features.startup.require::<Arc<TriggerManager>>().expect("the manager is in startup");
    manager.list_triggers(session_id).await.expect("the trigger records read")
  }

  fn launches(&self) -> Vec<(String, Vec<Value>)> {
    self.executor.launches.lock().expect("the launch log is never poisoned").clone()
  }

  /// Every sidebar frame that arrives within a short quiet window.
  async fn sidebar_frames(subscription: &mut Subscription) -> Vec<Value> {
    let mut frames = Vec::new();
    while let Ok(Some(frame)) = tokio::time::timeout(Duration::from_millis(100), subscription.recv()).await {
      frames.push(serde_json::from_str(&frame).expect("a sidebar frame is JSON"));
    }
    frames
  }
}

fn router_command(directory: &TempDir, body: &str, arguments: &[&str]) -> Vec<String> {
  let path = directory.path().join("fake-router.sh");
  fs::write(&path, format!("set -eu\n{body}\n")).unwrap();
  let mut command = vec!["/bin/sh".to_string(), path.to_string_lossy().into_owned()];
  command.extend(arguments.iter().map(|argument| (*argument).to_string()));
  command
}

/// The fakechat adapter over an in-memory thread: posts and acks record, and `read_eligible` returns `thread`.
#[derive(Default)]
struct FakeAdapter {
  posts: Mutex<Vec<(Value, String, String)>>,
  post_keys: Mutex<HashSet<String>>,
  post_delay: Duration,
  acks: Mutex<Vec<Value>>,
  removed: Mutex<Vec<Value>>,
  thread: Mutex<Vec<ThreadMessage>>,
}

impl FakeAdapter {
  fn with_thread(messages: &[(&str, &str)]) -> Arc<Self> {
    Self::with_thread_and_delay(messages, Duration::ZERO)
  }

  fn with_thread_and_delay(messages: &[(&str, &str)], post_delay: Duration) -> Arc<Self> {
    let adapter = Self { post_delay, ..Self::default() };
    *adapter.thread.lock().expect("the thread lock is never poisoned") = messages
      .iter()
      .map(|(id, text)| ThreadMessage {
        id: id.to_string(),
        user: Some("u".to_string()),
        text: text.to_string(),
        created_at: None,
        metadata: None,
        bot_id: None,
        is_bot: false,
        eligible: true,
      })
      .collect();
    Arc::new(adapter)
  }
}

impl ThreadAdapter for FakeAdapter {
  fn platform(&self) -> &ThreadPlatform {
    &FAKECHAT
  }

  fn post<'a>(&'a self, address: &'a Value, text: &'a str, post_key: &'a str) -> BoxFuture<'a, anyhow::Result<()>> {
    let post_delay = self.post_delay;
    Box::pin(async move {
      tokio::time::sleep(post_delay).await;
      self.posts.lock().expect("the post log is never poisoned").push((
        address.clone(),
        text.to_string(),
        post_key.to_string(),
      ));
      self.post_keys.lock().expect("the post-key log is never poisoned").insert(post_key.to_string());
      Ok(())
    })
  }

  fn find_own_post_by_key<'a>(
    &'a self,
    _address: &'a Value,
    post_key: &'a str,
    _text: &'a str,
    _created_after: &'a str,
    _messages: Option<&'a [ThreadMessage]>,
  ) -> BoxFuture<'a, anyhow::Result<bool>> {
    Box::pin(async move { Ok(self.post_keys.lock().expect("the post-key log is never poisoned").contains(post_key)) })
  }

  fn add_ack<'a>(&'a self, block: &'a Value) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move {
      self.acks.lock().expect("the ack log is never poisoned").push(block.clone());
      Ok(())
    })
  }

  fn remove_ack<'a>(&'a self, block: &'a Value) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move {
      self.removed.lock().expect("the removal log is never poisoned").push(block.clone());
      Ok(())
    })
  }

  fn read_thread<'a>(
    &'a self,
    _origin: &'a Value,
    _config: &'a CharlieBotConfig,
  ) -> BoxFuture<'a, anyhow::Result<Vec<ThreadMessage>>> {
    Box::pin(async move { Ok(self.thread.lock().expect("the thread lock is never poisoned").clone()) })
  }

  fn address_of(&self, origin: &Value) -> Value {
    origin.clone()
  }

  fn log_fields(&self, address: &Value) -> Value {
    json!({"channel": address["channel_id"]})
  }
}

/// Wait until the background work has run: the predicate holds, or the bound runs out.
async fn settle(done: impl Fn() -> bool) {
  for _ in 0..400 {
    if done() {
      return;
    }
    tokio::time::sleep(Duration::from_millis(5)).await;
  }
  panic!("the background work did not finish");
}

/// The reply-contract fixture: a synthetic checkout whose `prompts/` holds the given docs.
fn checkout(docs: &[(&str, &str)]) -> (TempDir, RepoRoot) {
  let dir = tempfile::tempdir().unwrap();
  fs::create_dir(dir.path().join("prompts")).unwrap();
  for (name, text) in docs {
    fs::write(dir.path().join("prompts").join(name), text).unwrap();
  }
  let repo = RepoRoot::new(Some(OsString::from(dir.path()))).unwrap();
  (dir, repo)
}

fn all_docs(scope_text: &str) -> Vec<(&str, &str)> {
  vec![(SCOPE, scope_text), ("thread_reply_redline.md", "red line\n"), ("thread_reply_format.md", "\treply format\r\n")]
}

#[test]
fn a_doc_is_read_fresh_stripped_and_with_universal_newlines() {
  let (dir, repo) = checkout(&[(SCOPE, "\u{1c}first\r\nsecond\rthird\u{3000}\n")]);
  assert_eq!(load_prompt_doc(&repo, SCOPE, "cause").unwrap(), "first\nsecond\nthird");

  fs::write(dir.path().join("prompts").join(SCOPE), "edited\n").unwrap();
  assert_eq!(load_prompt_doc(&repo, SCOPE, "cause").unwrap(), "edited");
}

#[test]
fn a_missing_doc_names_its_path_and_the_likely_cause() {
  let (dir, repo) = checkout(&[]);
  let error = load_prompt_doc(&repo, SCOPE, "the cause").unwrap_err();
  assert_eq!(
    error.to_string(),
    format!("{SCOPE} prompt not found at {}/prompts/{SCOPE} — the cause", dir.path().display())
  );
}

#[test]
fn the_contract_joins_the_platform_part_and_the_three_docs() {
  let (_dir, repo) = checkout(&all_docs("scope text"));
  let text = thread_reply_contract(&repo, &FAKECHAT, "platform part").unwrap();
  assert_eq!(text, "platform part\n\nscope text\nred line\nreply format");
}

#[test]
fn a_missing_contract_doc_fails_with_its_own_likely_cause() {
  let (_dir, repo) = checkout(&[(SCOPE, "scope"), ("thread_reply_redline.md", "red")]);
  let error = thread_reply_contract(&repo, &FAKECHAT, "part").unwrap_err().to_string();
  assert!(error.starts_with("thread_reply_format.md prompt not found at "), "{error}");
  assert!(error.ends_with("— the repo checkout most likely predates the thread-reply-format rename commit"), "{error}");
}

#[test]
fn the_segment_names_the_three_docs_as_base_full_sources() {
  let segment = thread_reply_segment(&FAKECHAT, "text".to_string());
  assert_eq!(segment.text, "text");
  assert_eq!(segment.delivery, "full");
  let refs: Vec<&str> = segment.sources.iter().map(|source| source.source_ref.as_str()).collect();
  assert_eq!(
    refs,
    ["prompts/fakechat_reply_scope.md", "prompts/thread_reply_redline.md", "prompts/thread_reply_format.md"]
  );
  assert!(segment.sources.iter().all(|source| source.scope == "base" && source.source_session_id.is_none()));
}

#[test]
fn fakechat_marker_keys_derive_from_the_name() {
  assert_eq!(FAKECHAT.notice_key(), "fakechat_notice");
  assert_eq!(FAKECHAT.backfill_key(), "fakechat_backfill");
  assert_eq!(FAKECHAT.ack_event_type(), "fakechat_ack");
}

#[test]
fn newest_thread_input_picks_the_newest_fakechat_block() {
  // b1 carries no summon block; s3 carries another platform's block.
  let events = vec![
    json!({"id": "b1", "type": event_types::AGENT_MESSAGE}),
    json!({"id": "s1", "type": event_types::AGENT_MESSAGE, "fakechat": {"thread": "t1"}}),
    json!({"id": "s2", "type": event_types::AGENT_MESSAGE, "fakechat": {"thread": "t1", "nudge_of": "s1"}}),
    json!({"id": "s3", "type": event_types::AGENT_MESSAGE, "slack": {"thread": "t1"}}),
  ];
  let ids = |list: &[&str]| list.iter().map(|id| id.to_string()).collect::<Vec<String>>();
  let bound = newest_thread_input(&FAKECHAT, &events, &ids(&["b1", "s1", "s2", "s3"]));
  assert_eq!(bound, Some(("s2".to_string(), &json!({"thread": "t1", "nudge_of": "s1"}))));
  assert_eq!(newest_thread_input(&FAKECHAT, &events, &ids(&["b1"])), None);
  assert_eq!(newest_thread_input(&FAKECHAT, &events, &ids(&["s3"])), None);
}

#[test]
fn replied_nudged_noticed_read_the_fakechat_shapes() {
  let events = vec![
    json!({"id": "r1", "type": reply_type(), "fakechat_reply": {"answers": "s1"}}),
    json!({"id": "r2", "type": reply_type(), "fakechat_reply": {"answers": "s2"}}),
    json!({"id": "n1", "type": event_types::AGENT_MESSAGE, "fakechat": {"nudge_of": "s1"}}),
    json!({"id": "x1", "type": event_types::ASSISTANT_ERROR, "fakechat_notice": {event_types::INPUT_EVENT_ID: "s1"}}),
  ];
  assert!(replied(&FAKECHAT, &events, "s1"));
  assert!(!replied(&FAKECHAT, &events, "s9"));
  assert!(nudged(&FAKECHAT, &events, "s1"));
  assert!(!nudged(&FAKECHAT, &events, "s2"));
  assert!(noticed(&FAKECHAT, &events, "s1"));
  assert!(!noticed(&FAKECHAT, &events, "s2"));
}

#[test]
fn lost_summons_reports_only_the_unanswered_unowned_unmarked() {
  let mut events = vec![
    summon_event("s1"),
    summon_event("s2"),
    summon_event("s3"),
    summon_event("s4"),
    json!({"id": "d1", "type": event_types::MASTER_DONE, event_types::INPUT_EVENT_IDS: ["s1"]}),
    json!({"id": "m2", "type": event_types::ASSISTANT_ERROR, "fakechat_backfill": {event_types::INPUT_EVENT_ID: "s2"}}),
  ];
  let ids = |list: &[&str]| list.iter().map(|id| id.to_string()).collect::<HashSet<String>>();
  let ids_of = |lost: Vec<Value>| lost.iter().map(|ev| ev["id"].as_str().unwrap().to_string()).collect::<Vec<_>>();
  assert_eq!(ids_of(lost_summons(&FAKECHAT, &events, &ids(&["s3"]), &HashSet::new())), ["s4"]);
  // The running set keeps its summon off the report the same way.
  assert!(lost_summons(&FAKECHAT, &events, &ids(&["s3"]), &ids(&["s4"])).is_empty());
  // A marker under another platform's backfill key marks nothing here.
  events.push(
    json!({"id": "m3", "type": event_types::ASSISTANT_ERROR, "slack_backfill": {event_types::INPUT_EVENT_ID: "s4"}}),
  );
  assert_eq!(ids_of(lost_summons(&FAKECHAT, &events, &ids(&["s3"]), &HashSet::new())), ["s4"]);
}

#[test]
fn unread_after_orders_ids_by_the_platform_key() {
  struct Message {
    id: &'static str,
    human: bool,
  }
  let messages = [
    Message { id: "98", human: true },
    Message { id: "99", human: true },
    Message { id: "100", human: true },
    Message { id: "101", human: false },
  ];
  let ids = |watermark: Option<&str>| {
    unread_after(&messages, |m| m.human, |m| m.id, watermark, IdOrdering::Snowflake)
      .into_iter()
      .map(|m| m.id)
      .collect::<Vec<_>>()
  };
  // Integer order: "100" sorts above the "99" watermark (the string sort would not).
  assert_eq!(ids(Some("99")), ["100"]);
  // A None watermark passes every eligible message; the ineligible one still drops.
  assert_eq!(ids(None), ["98", "99", "100"]);
  assert!(ids(Some("100")).is_empty());
}

#[test]
fn follow_label_is_one_line_and_follow_floor_parses_it_back() {
  let label = follow_wake_label(&FAKECHAT, "1700000000.000100");
  assert_eq!(label, "fakechat-thread-follow floor=1700000000.000100");
  assert_eq!(follow_floor(&label).as_deref(), Some("1700000000.000100"));
  assert_eq!(follow_floor("no floor on this label"), None);
  assert_eq!(follow_floor("floor=not-numeric"), None);
}

#[test]
fn chunk_text_respects_the_limit_and_keeps_the_content() {
  let text = "first paragraph\n\nsecond paragraph\n\nthird";
  let chunks = chunk_text(text, 20);
  assert!(chunks.iter().all(|chunk| chunk.chars().count() <= 20));
  assert_eq!(chunks.concat(), text);
  assert_eq!(chunk_text("short", 20), ["short"]);
  assert_eq!(chunk_text(&"x".repeat(25), 10), ["x".repeat(10), "x".repeat(10), "x".repeat(5)]);
}

#[test]
fn no_file_server_links_refuses_the_first_link_and_names_the_publish_command() {
  let link = "http://localhost:18498/absolute_filepath/home/u/caf%C3%A9/page.html";
  let later = "https://charliebot.example/absolute_filepath/home/u/other.html";

  let error = assert_no_file_server_links(&format!("see {link} or {later}"), None).unwrap_err();

  assert_eq!(error.status, StatusCode::UNPROCESSABLE_ENTITY);
  // The first link is named with its query and fragment as written, and the command carries the path the link names,
  // percent-decoded.
  let detail = error.to_string();
  assert!(detail.contains(link), "{detail}");
  assert!(detail.contains("charliebot publish /home/u/café/page.html"), "{detail}");
}

#[test]
fn no_file_server_links_refuses_a_portless_link_the_same_way() {
  let link = "https://charliebot.example/absolute_filepath/home/u/page.html";

  let error = assert_no_file_server_links(&format!("see {link}"), None).unwrap_err();

  assert_eq!(error.status, StatusCode::UNPROCESSABLE_ENTITY);
  let detail = error.to_string();
  assert!(detail.contains(link), "{detail}");
  assert!(detail.contains("charliebot publish /home/u/page.html"), "{detail}");
}

#[test]
fn no_file_server_links_refuses_retired_and_local_paths_with_publish_instruction() {
  let root = tempfile::tempdir().unwrap();
  let context = file_urls::ScanContext {
    server_port: 18498,
    charliebot_home: root.path().join("charliebot-home"),
    session_dir: root.path().join("sessions").join("synthetic"),
    user_home: root.path().join("user-home"),
  };
  let retired = "https://charliebot.example:18498/files/sessions/synthetic/page.html";
  for (text, named) in [(retired, retired), ("[plan](artifacts/plan.html)", "artifacts/plan.html")] {
    let error = assert_no_file_server_links(text, Some(&context)).expect_err("a dead file link is refused");

    assert_eq!(error.status, StatusCode::UNPROCESSABLE_ENTITY, "{text}");
    let detail = error.to_string();
    assert!(detail.contains(named), "{detail}");
    assert!(detail.contains("Publish the page with `charliebot publish "), "{detail}");
    assert!(detail.contains("write the URL the command prints into the reply"), "{detail}");
  }
}

#[test]
fn no_file_server_links_passes_a_published_url_and_names_route_links() {
  let port = 8080;
  let published = "https://publish.example/Ab3dEf6hIj8kLm1nOp2q/page.html";
  let route_url = format!("http://127.0.0.1:{port}/diff");
  let text = format!("open {published} and {route_url}");

  assert!(assert_no_file_server_links(&text, None).is_ok());

  let routes = application_route_links(&text, port);
  assert_eq!(routes, std::slice::from_ref(&route_url));
  let note = operator_only_note(&routes).expect("a route link names the operator line");
  assert!(note.contains(&route_url), "{note}");
  assert_eq!(operator_only_note(&[]), None);
}

#[tokio::test]
async fn ack_messages_orders_ids_by_the_platform_id_key() {
  let home = Home::new();
  home.thread_session("s1", origin()).await;
  // "100" sorts above "99" only as an integer; the string sort would pick "99".
  let adapter: Arc<dyn ThreadAdapter> = FakeAdapter::with_thread(&[("99", "a"), ("100", "b")]);
  let ids = vec!["100".to_string(), "99".to_string()];

  let readback = ack_messages(&adapter, &home.features, "s1", &ids).await.unwrap();

  assert_eq!(readback, json!({"acked": 2, "watermark_id": "100"}));
  assert_eq!(home.meta("s1").await.extra("fakechat_watermark_id"), Some(&json!("100")));
  let ack_event = home.events("s1").into_iter().find(|ev| ev["type"] == "fakechat_ack").expect("the ack is logged");
  assert_eq!(ack_event["content"], "Fakechat thread ack: 2 message(s) read through 100");
  assert_eq!(ack_event["fakechat_ack"], json!({"message_ids": ["99", "100"], "watermark_id": "100"}));
}

#[tokio::test]
async fn audit_nudges_a_summon_of_the_current_contract_for_the_one_line_input() {
  audit_nudges_a_summon_of_the_current_contract(FAKECHAT.summon_line).await;
}

#[tokio::test]
async fn audit_nudges_a_summon_of_the_current_contract_for_the_full_prompt() {
  audit_nudges_a_summon_of_the_current_contract(
    "Fakechat summon; post the reply with `charliebot fakechat reply --file <path>`.",
  )
  .await;
}

/// Both summon formats of the current contract get the nudge: the one-line input, and the older full prompt that named
/// the reply command.
async fn audit_nudges_a_summon_of_the_current_contract(content: &str) {
  let home = Home::new();
  home.thread_session("s1", origin()).await;
  let adapter: Arc<dyn ThreadAdapter> = FakeAdapter::with_thread(&[]);
  home
    .append(
      "s1",
      json!({"id": "s1", "type": event_types::AGENT_MESSAGE, "content": content,
             "fakechat": {"channel_id": "c1", "thread_ts": "t1", "mention_id": "m1"}}),
    )
    .await;
  let done = json!({"type": event_types::MASTER_DONE, "input_event_id": "s1", "exit_code": 0, "still_thinking": false});

  let acted = deliver_done(&adapter, &home.features, "s1", &done).await.unwrap();

  assert!(acted);
  settle(|| home.launches().len() == 1).await;
  let nudge =
    home.events("s1").into_iter().find(|ev| ev["fakechat"]["nudge_of"] == "s1").expect("the nudge is persisted");
  assert_eq!(nudge["type"], event_types::AGENT_MESSAGE);
  assert_eq!(nudge["from_session_name"], "Fakechat");
  // The nudge carries the decision only: no link, no reply-command usage.
  let text = nudge["content"].as_str().unwrap();
  assert!(
    text.starts_with("Fakechat thread: the round answering this mention ended without posting a reply"),
    "{text}"
  );
  assert!(!text.contains("http"));
  assert!(!text.contains("fakechat reply"));
  assert_eq!(nudge["fakechat"], json!({"channel_id": "c1", "thread_ts": "t1", "mention_id": "m1", "nudge_of": "s1"}));
}

#[tokio::test]
async fn audit_leaves_a_marker_contract_summon_alone() {
  let home = Home::new();
  home.thread_session("s1", origin()).await;
  let adapter: Arc<dyn ThreadAdapter> = FakeAdapter::with_thread(&[]);
  // A summon carrying neither the one-line input nor the reply command predates the contract.
  home
    .append(
      "s1",
      json!({"id": "s1", "type": event_types::AGENT_MESSAGE, "content": "fakechat marker summon",
             "fakechat": {"channel_id": "c1", "thread_ts": "t1", "mention_id": "m1"}}),
    )
    .await;
  let before = home.events("s1").len();
  let done = json!({"type": event_types::MASTER_DONE, "input_event_id": "s1", "exit_code": 0, "still_thinking": false});

  let acted = deliver_done(&adapter, &home.features, "s1", &done).await.unwrap();

  assert!(!acted);
  assert_eq!(home.events("s1").len(), before);
}

#[tokio::test]
async fn round_completion_and_boot_audit_emit_one_nudge_then_notice() {
  let home = Home::new();
  home.thread_session("audit-retry", origin()).await;
  let backend_pool =
    home.features.startup.require::<backend_pool::BackendPool>().expect("the backend pool is in startup");
  home
    .features
    .startup
    .insert(charliebot_tasks::master_queue::MasterQueue::new(backend_pool))
    .expect("the master queue inserts once");
  let adapter = FakeAdapter::with_thread_and_delay(&[], Duration::from_millis(50));
  let dyn_adapter: Arc<dyn ThreadAdapter> = adapter.clone();
  let summon = home
    .append(
      "audit-retry",
      json!({
        "type": event_types::AGENT_MESSAGE,
        "content": FAKECHAT.summon_line,
        "fakechat": {"channel_id": "c1", "thread_ts": "t1", "mention_id": "m1"},
      }),
    )
    .await;
  let summon_done = home
    .append("audit-retry", json!({"type": event_types::MASTER_DONE, "input_event_id": summon["id"], "exit_code": 0}))
    .await;

  assert!(deliver_done(&dyn_adapter, &home.features, "audit-retry", &summon_done).await.unwrap());
  settle(|| home.launches().len() == 1).await;
  let nudge = home
    .events("audit-retry")
    .into_iter()
    .find(|event| event.get("fakechat").and_then(|block| block.get("nudge_of")).is_some())
    .expect("the silent summon creates a nudge");
  let nudge_done = home
    .append("audit-retry", json!({"type": event_types::MASTER_DONE, "input_event_id": nudge["id"], "exit_code": 0}))
    .await;

  let (delivered, backfilled) = tokio::join!(
    deliver_done(&dyn_adapter, &home.features, "audit-retry", &nudge_done),
    backfill_lost_summons(&dyn_adapter, &home.features),
  );
  delivered.unwrap();
  backfilled.unwrap();

  let events = home.events("audit-retry");
  let nudges: Vec<_> =
    events.iter().filter(|event| event.get("fakechat").and_then(|block| block.get("nudge_of")).is_some()).collect();
  let notices: Vec<_> = events
    .iter()
    .filter(|event| event.get("fakechat_notice").and_then(|notice| notice.get(event_types::INPUT_EVENT_ID)).is_some())
    .collect();
  assert_eq!(nudges.len(), 1);
  assert_eq!(notices.len(), 1);
  assert_eq!(adapter.posts.lock().unwrap().len(), 1);
  assert_eq!(nudges[0]["request_id"], format!("fakechat-nudge:{}", summon["id"].as_str().unwrap()));
  assert_eq!(notices[0]["request_id"], format!("fakechat-notice:{}", summon["id"].as_str().unwrap()));
}

#[tokio::test]
async fn consume_mention_advances_the_watermark_by_the_platform_id_key() {
  let home = Home::new();
  home.thread_session("s1", origin()).await;
  home
    .features
    .store
    .update_slot_fields("s1", "fakechat", Map::from_iter([("fakechat_watermark_id".to_string(), json!("99"))]))
    .await
    .unwrap();

  // Integer order: "100" sorts above the "99" watermark only as an integer.
  consume_mention(&FAKECHAT, &home.features, "s1", "100").await.unwrap();

  assert_eq!(home.meta("s1").await.extra("fakechat_watermark_id"), Some(&json!("100")));
  assert!(home.triggers("s1").await.is_empty());
}

#[tokio::test]
async fn follow_message_drops_below_the_watermark_and_arms_above_it() {
  let home = Home::new();
  home.thread_session("s1", origin()).await;
  home
    .features
    .store
    .update_slot_fields("s1", "fakechat", Map::from_iter([("fakechat_watermark_id".to_string(), json!("100"))]))
    .await
    .unwrap();
  let adapter: Arc<dyn ThreadAdapter> = FakeAdapter::with_thread(&[]);
  let expected = origin();
  let origin_matches = |candidate: &Value| *candidate == expected;

  // Integer order: "99" sits below the "100" watermark even though the string sorts above.
  let dropped = follow_message(&adapter, &home.features, "s1", "99", origin_matches).await.unwrap();
  assert_eq!(dropped, None);
  assert!(home.triggers("s1").await.is_empty());

  let armed = follow_message(&adapter, &home.features, "s1", "101", origin_matches).await.unwrap();

  assert_eq!(armed.as_deref(), Some("s1"));
  let labels: Vec<String> = home.triggers("s1").await.into_iter().map(|record| record.message).collect();
  assert_eq!(labels, ["fakechat-thread-follow floor=101"]);
}

#[tokio::test]
async fn follow_message_revives_an_archived_session_and_arms_after_the_unarchive() {
  let home = Home::new();
  home.thread_session("s1", origin()).await;
  home.features.store.archive_session("s1").await.unwrap();
  let mut sidebar = home.features.store.subscribe(SIDEBAR_CHANNEL);
  let adapter: Arc<dyn ThreadAdapter> = FakeAdapter::with_thread(&[]);
  let expected = origin();

  let armed = follow_message(&adapter, &home.features, "s1", "105", |candidate| *candidate == expected).await.unwrap();

  assert_eq!(armed.as_deref(), Some("s1"));
  assert_eq!(home.meta("s1").await.status, SessionStatus::Active);
  let frames = Home::sidebar_frames(&mut sidebar).await;
  assert!(frames.iter().any(is_unarchive_frame), "{frames:?}");
  // The unarchive lands before the arm: create_trigger rejects an archived session, so the arm's record proves the order.
  assert_eq!(home.triggers("s1").await.len(), 1);
}

/// Whether a sidebar frame is the task-tree change that announces an unarchive.
fn is_unarchive_frame(frame: &Value) -> bool {
  frame.get("type").and_then(Value::as_str) == Some("task_tree_changed")
    && frame.to_string().contains("session_unarchived")
}

#[tokio::test]
async fn accept_summon_unarchive_broadcasts_the_task_tree_change() {
  let home = Home::new();
  home.thread_session("s1", origin()).await;
  home.features.store.archive_session("s1").await.unwrap();
  let mut sidebar = home.features.store.subscribe(SIDEBAR_CHANNEL);
  let adapter = FakeAdapter::with_thread(&[]);
  let dyn_adapter: Arc<dyn ThreadAdapter> = adapter.clone();

  let (origin_value, block) = (origin(), summon_block());
  let sid = accept_summon(&dyn_adapter, &home.features, summon(&origin_value, &block)).await.unwrap();

  assert_eq!(sid, "s1");
  assert_eq!(home.meta("s1").await.status, SessionStatus::Active);
  // The mention path's unarchive notifies the sidebar the same way the follow path's does.
  let frames = Home::sidebar_frames(&mut sidebar).await;
  assert!(frames.iter().any(is_unarchive_frame), "{frames:?}");
  settle(|| home.launches().len() == 1 && adapter.acks.lock().unwrap().len() == 1).await;
}

#[tokio::test]
async fn backfill_revives_an_archived_session_with_unread_messages() {
  let home = Home::new();
  home.thread_session("s1", origin()).await;
  home.features.store.archive_session("s1").await.unwrap();
  let mut sidebar = home.features.store.subscribe(SIDEBAR_CHANNEL);
  let adapter = FakeAdapter::with_thread(&[("105", "posted while the socket was down")]);
  let dyn_adapter: Arc<dyn ThreadAdapter> = adapter.clone();

  let armed_count = backfill_followed_threads(&dyn_adapter, &home.features).await.unwrap();

  assert_eq!(armed_count, 1);
  assert_eq!(home.meta("s1").await.status, SessionStatus::Active);
  let frames = Home::sidebar_frames(&mut sidebar).await;
  assert!(frames.iter().any(is_unarchive_frame), "{frames:?}");
  let labels: Vec<String> = home.triggers("s1").await.into_iter().map(|record| record.message).collect();
  assert_eq!(labels, ["fakechat-thread-follow floor=105"]);
}

#[tokio::test]
async fn backfill_arms_an_active_session_without_a_revival() {
  let home = Home::new();
  home.thread_session("s1", origin()).await;
  let mut sidebar = home.features.store.subscribe(SIDEBAR_CHANNEL);
  let adapter: Arc<dyn ThreadAdapter> = FakeAdapter::with_thread(&[("105", "hello")]);

  let armed_count = backfill_followed_threads(&adapter, &home.features).await.unwrap();

  assert_eq!(armed_count, 1);
  assert!(Home::sidebar_frames(&mut sidebar).await.iter().all(|frame| !is_unarchive_frame(frame)));
  assert_eq!(home.triggers("s1").await.len(), 1);
}

#[tokio::test]
async fn backfill_leaves_an_archived_session_without_unread_archived() {
  let home = Home::new();
  home.thread_session("s1", origin()).await;
  home.features.store.archive_session("s1").await.unwrap();
  let adapter: Arc<dyn ThreadAdapter> = FakeAdapter::with_thread(&[]);

  let armed_count = backfill_followed_threads(&adapter, &home.features).await.unwrap();

  assert_eq!(armed_count, 0);
  assert_eq!(home.meta("s1").await.status, SessionStatus::Archived);
  assert!(home.triggers("s1").await.is_empty());
}

#[tokio::test]
async fn accept_summon_creates_the_session_and_spawns_the_round_and_ack() {
  let home = Home::new();
  let adapter = FakeAdapter::with_thread(&[]);
  let dyn_adapter: Arc<dyn ThreadAdapter> = adapter.clone();

  let (origin_value, block) = (origin(), summon_block());
  let sid = accept_summon(&dyn_adapter, &home.features, summon(&origin_value, &block)).await.unwrap();

  assert_eq!(sid, "s1");
  let meta = home.meta("s1").await;
  assert!(meta.name.starts_with("Fakechat #c1 "), "{}", meta.name);
  assert_eq!(meta.profile, TaskProfile::Manager);
  assert_eq!(meta.extra("fakechat_origin"), Some(&origin()));
  assert_eq!(meta.group.as_deref(), Some("Fakechat #c1"));
  let summon_event = home
    .events("s1")
    .into_iter()
    .find(|ev| ev["fakechat"] == summon_block())
    .expect("the summon is persisted under the platform's key");
  assert_eq!(summon_event["type"], event_types::AGENT_MESSAGE);
  assert_eq!(summon_event["from_session_name"], "Fakechat");
  settle(|| home.launches().len() == 1 && adapter.acks.lock().unwrap().len() == 1).await;
  let (launched_session, pending) = home.launches().remove(0);
  assert_eq!(launched_session, "s1");
  // The round wakes on the summon's own id: the admitted input takes it as its event id.
  assert_eq!(pending[0]["id"], summon_event["id"]);
  assert_eq!(*adapter.acks.lock().unwrap(), [summon_block()]);
}

#[tokio::test]
async fn accept_summon_routes_the_full_thread_before_creating_the_session() {
  let router_dir = tempfile::tempdir().unwrap();
  let capture = router_dir.path().join("router-input.json");
  let capture_text = capture.to_string_lossy().into_owned();
  let command = router_command(
    &router_dir,
    "cat > \"$1\"\nprintf '%s' '{\"backends\":[\"synthetic-strong\"],\"reason\":\"thread route\"}'",
    &[&capture_text],
  );
  let home = Home::with_router(command);
  let adapter = FakeAdapter::with_thread(&[("100", " first "), ("101", "second\nline")]);
  let dyn_adapter: Arc<dyn ThreadAdapter> = adapter.clone();
  let (origin_value, block) = (origin(), summon_block());

  let sid = accept_summon(&dyn_adapter, &home.features, summon(&origin_value, &block)).await.unwrap();

  let meta = home.meta(&sid).await;
  assert_eq!(meta.backend, "synthetic-strong");
  assert_eq!(meta.extra("route"), Some(&json!({"backends": ["synthetic-strong"], "reason": "thread route"})));
  let input: Value = serde_json::from_slice(&fs::read(capture).unwrap()).unwrap();
  assert_eq!(input["role"], "thread");
  assert_eq!(input["text"], "u:  first \nu: second\nline");
  assert_eq!(input["candidates"].as_array().unwrap().len(), 2);
  settle(|| home.launches().len() == 1 && adapter.acks.lock().unwrap().len() == 1).await;
}

#[tokio::test]
async fn accept_summon_posts_router_failure_without_creating_a_session() {
  let router_dir = tempfile::tempdir().unwrap();
  let command = router_command(&router_dir, "cat >/dev/null\nprintf '%s' 'router boom' >&2\nexit 3", &[]);
  let home = Home::with_router(command);
  let adapter = FakeAdapter::with_thread(&[]);
  let dyn_adapter: Arc<dyn ThreadAdapter> = adapter.clone();
  let (origin_value, block) = (origin(), summon_block());

  let error = accept_summon(&dyn_adapter, &home.features, summon(&origin_value, &block)).await.unwrap_err();

  assert_eq!(error.status, StatusCode::INTERNAL_SERVER_ERROR);
  assert!(home.features.store.get_session("s1").await.unwrap().is_none());
  assert_eq!(adapter.posts.lock().unwrap().len(), 1);
  assert!(adapter.posts.lock().unwrap()[0].1.contains("router boom"));
  assert!(home.launches().is_empty());
}

#[tokio::test]
async fn accept_summon_uses_the_pool_for_an_empty_router_order() {
  let router_dir = tempfile::tempdir().unwrap();
  let command =
    router_command(&router_dir, "cat >/dev/null\nprintf '%s' '{\"backends\":[],\"reason\":\"no preference\"}'", &[]);
  let home = Home::with_router(command);
  let adapter = FakeAdapter::with_thread(&[]);
  let dyn_adapter: Arc<dyn ThreadAdapter> = adapter.clone();
  let (origin_value, block) = (origin(), summon_block());

  let sid = accept_summon(&dyn_adapter, &home.features, summon(&origin_value, &block)).await.unwrap();

  let meta = home.meta(&sid).await;
  assert_eq!(meta.backend, "synthetic-fakechat");
  assert_eq!(meta.extra("route"), Some(&json!({"backends": [], "reason": "no preference"})));
  settle(|| home.launches().len() == 1 && adapter.acks.lock().unwrap().len() == 1).await;
}
