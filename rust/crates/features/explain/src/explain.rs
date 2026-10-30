//! The explain generation, its results file and the stale-pending reap (`src/features/explain/explain.py`).
//!
//! The results file `sessions/<sid>/data/explain_results.json` is the persisted truth: one entry per divider, keyed
//! by the divider's event index. The live generations are memory-only, so a server restart leaves pending entries
//! with no generation behind them; the read that meets one older than the one-shot budget reaps it.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::{Context, bail};
use charliebot_agents::agent_environment::AgentBaseEnvironment;
use charliebot_agents::{agent_one_shot_text, build_backend};
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_core::{fsutil, tempdir, timefmt};
use charliebot_registry::FeatureContext;
use charliebot_registry::backend::LaunchArgs;
use charliebot_store::{Store, session_channel};
use serde_json::{Map, Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::Mutex;
use tokio::task::{JoinHandle, spawn_blocking};

use crate::prompt;

/// The one-shot budget of an explain run, and the age after which a pending entry is stale.
pub const ONESHOT_TIMEOUT: Duration = Duration::from_secs(600);

const RESULTS_FILE: &str = "explain_results.json";
const NO_ROUND_TEXT_ERROR: &str = "This round has no explainable answer text.";
const REAPED_ERROR: &str = "interrupted: server restarted";
const EMPTY_ANSWER_ERROR: &str = "backend returned an empty explanation";
/// `str(TimeoutError())` is empty, so the class name is the error text.
const TIMEOUT_ERROR: &str = "TimeoutError";

/// The check-and-register critical section of `request_explain`: two racing requests on one divider register one
/// generation.
static REGISTER_LOCK: Mutex<()> = Mutex::const_new(());

/// The live generation tasks by session and divider. An entry leaves when its task ends.
static TASKS: StdMutex<BTreeMap<(String, i64), JoinHandle<()>>> = StdMutex::new(BTreeMap::new());

/// The results file's document: each divider's entry under its event index.
type Results = Map<String, Value>;

/// The session's explain results file, beside `chat_events.jsonl`.
pub fn results_path(store: &Store, session_id: &str) -> PathBuf {
  store.chat_events_path(session_id).with_file_name(RESULTS_FILE)
}

/// Runs blocking file work on the blocking pool, where Python runs its `asyncio.to_thread` calls.
async fn blocking<T: Send + 'static>(work: impl FnOnce() -> anyhow::Result<T> + Send + 'static) -> anyhow::Result<T> {
  spawn_blocking(work).await?
}

/// `json_utils.load_json_dict`: the document at `path`, or an empty one when the file does not exist yet.
fn load_results(path: &Path) -> anyhow::Result<Results> {
  if !path.exists() {
    return Ok(Results::new());
  }
  match serde_json::from_str(&fs::read_to_string(path)?)? {
    Value::Object(results) => Ok(results),
    _ => bail!("{} is not a JSON object", path.display()),
  }
}

/// `_write_entry`: replace the divider's entry and swap the whole document into place, indented as Python writes it.
fn write_entry(path: &Path, upto: i64, entry: Map<String, Value>) -> anyhow::Result<()> {
  let mut results = load_results(path)?;
  results.insert(upto.to_string(), Value::Object(entry));
  fsutil::atomic_write_text(path, &serde_json::to_string_pretty(&results)?)?;
  Ok(())
}

fn as_entry(value: &Value) -> anyhow::Result<&Map<String, Value>> {
  value.as_object().context("an explain entry is not a JSON object")
}

fn field<'a>(entry: &'a Map<String, Value>, name: &str) -> anyhow::Result<&'a Value> {
  entry.get(name).with_context(|| format!("an explain entry has no {name} field"))
}

fn is_pending(entry: &Map<String, Value>) -> bool {
  entry.get("state").and_then(Value::as_str) == Some("pending")
}

/// `_is_stale`: a pending entry whose own request has outlived the one-shot budget.
fn is_stale(entry: &Map<String, Value>) -> anyhow::Result<bool> {
  let requested_at = field(entry, "requested_at")?.as_str().context("an explain entry's requested_at is not text")?;
  let requested = OffsetDateTime::parse(requested_at, &Rfc3339)?;
  Ok(OffsetDateTime::now_utc() - requested > time::Duration::try_from(ONESHOT_TIMEOUT)?)
}

fn new_entry(
  state: &str,
  backend: &str,
  answer: &str,
  error: Option<&str>,
  requested_at: &str,
  generated_at: Option<&str>,
) -> Map<String, Value> {
  let mut entry = Map::new();
  entry.insert("state".into(), state.into());
  entry.insert("backend".into(), backend.into());
  entry.insert("answer".into(), answer.into());
  entry.insert("error".into(), error.into());
  entry.insert("requested_at".into(), requested_at.into());
  entry.insert("generated_at".into(), generated_at.into());
  entry
}

fn pending_entry(backend: &str, requested_at: &str) -> Map<String, Value> {
  new_entry("pending", backend, "", None, requested_at, None)
}

fn terminal_entry(
  backend: &str,
  requested_at: &str,
  state: &str,
  answer: &str,
  error: Option<&str>,
) -> Map<String, Value> {
  new_entry(state, backend, answer, error, requested_at, Some(&timefmt::utc_now_iso()))
}

/// `_reap_stale_pending`: land a stale pending entry as an error, persist it, and log the reap.
async fn reap(
  features: &FeatureContext,
  session_id: &str,
  upto: i64,
  entry: Map<String, Value>,
) -> anyhow::Result<Map<String, Value>> {
  let mut reaped = entry;
  reaped.insert("state".into(), "error".into());
  reaped.insert("error".into(), REAPED_ERROR.into());
  reaped.insert("generated_at".into(), timefmt::utc_now_iso().into());
  let path = results_path(&features.store, session_id);
  let write = reaped.clone();
  blocking(move || write_entry(&path, upto, write)).await?;
  tracing::warn!(session_id, upto, "explain_pending_reaped");
  Ok(reaped)
}

/// `extract_round_text`: the last assistant text in the events `[0, upto + 1)`, or `None` when there is none.
fn extract_round_text(store: &Store, session_id: &str, upto: i64) -> anyhow::Result<Option<String>> {
  // A divider below zero leaves an empty range, as Python's `range(0, upto + 1)` does.
  let end = usize::try_from(upto.saturating_add(1)).unwrap_or(0);
  let range = store.load_chat_events_shared_range_sync(session_id, 0, end)?;
  let events = &range.events[range.range];
  for message in store.events_to_messages_shared(events)?.iter().rev() {
    if message.get("role").and_then(Value::as_str) != Some("assistant") {
      continue;
    }
    match message.get("content") {
      None | Some(Value::Null) => {}
      Some(Value::String(text)) if !text.trim().is_empty() => return Ok(Some(text.clone())),
      Some(Value::String(_)) => {}
      Some(other) => bail!("an assistant message's content is not text: {other}"),
    }
  }
  Ok(None)
}

/// The read-only copy of `chat_events.jsonl` that a run reads. The copy lives in its own temporary directory, and
/// dropping the copy removes that directory.
struct HistoryCopy {
  _dir: tempdir::TempDir,
  path: PathBuf,
}

impl HistoryCopy {
  /// `_make_ro_copy`: copy the session's events into a fresh temporary directory, then chmod the copy 0444.
  fn make(store: &Store, session_id: &str) -> anyhow::Result<Self> {
    let source = store.chat_events_path(session_id);
    let dir = tempdir::mkdtemp("charliebot-explain-")?;
    let path = dir.path().join(source.file_name().context("the chat events path names no file")?);
    fs::copy(&source, &path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o444))?;
    Ok(Self { _dir: dir, path })
  }
}

/// One explain generation: the divider it explains, the backend it runs, and the request time its entry carries.
struct Generation {
  features: Arc<FeatureContext>,
  session_id: String,
  upto: i64,
  option: BackendOption,
  cfg: Arc<CharlieBotConfig>,
  requested_at: String,
}

/// How a generation ends.
enum Outcome {
  Ready(String),
  Error(String),
}

impl Generation {
  /// `_generate`: run the explain and land its entry. A failure lands as the entry's error text and is logged.
  async fn generate(&self, timeout: Duration) {
    if let Err(error) = self.run(timeout).await {
      let text = exception_text(&error);
      tracing::warn!(session_id = %self.session_id, upto = self.upto, error = %text, "explain_failed");
      if let Err(finish_error) = self.finish(Outcome::Error(text)).await {
        tracing::error!(session_id = %self.session_id, upto = self.upto, error = %finish_error, "explain_finish_failed");
      }
    }
  }

  async fn run(&self, timeout: Duration) -> anyhow::Result<()> {
    let store = Arc::clone(&self.features.store);
    let session_id = self.session_id.clone();
    let upto = self.upto;
    let Some(round_text) = blocking(move || extract_round_text(&store, &session_id, upto)).await? else {
      return self.finish(Outcome::Error(NO_ROUND_TEXT_ERROR.to_string())).await;
    };
    let answer = self.ask(&round_text, timeout).await?;
    if answer.is_empty() {
      return self.finish(Outcome::Error(EMPTY_ANSWER_ERROR.to_string())).await;
    }
    self.finish(Outcome::Ready(answer)).await
  }

  /// One base one-shot over the read-only history copy. The copy's directory is removed when this returns.
  async fn ask(&self, round_text: &str, timeout: Duration) -> anyhow::Result<String> {
    let store = Arc::clone(&self.features.store);
    let session_id = self.session_id.clone();
    let copy = blocking(move || HistoryCopy::make(&store, &session_id)).await?;
    let prompt = prompt::user(self.upto, round_text, &copy.path);
    let launch = LaunchArgs { cgroup_session_id: Some(self.session_id.clone()), ..LaunchArgs::default() };
    let built = build_backend(Arc::clone(&self.features.registry), &self.option, &self.cfg, launch)?;
    let base_env = self.features.startup.require::<AgentBaseEnvironment>()?;
    agent_one_shot_text(built, &base_env.values, &prompt, prompt::SYSTEM, Some(timeout)).await
  }

  /// `_finish`: land the terminal entry, then broadcast the state the file now holds.
  async fn finish(&self, outcome: Outcome) -> anyhow::Result<()> {
    let (state, answer, error) = match outcome {
      Outcome::Ready(answer) => ("ready", answer, None),
      Outcome::Error(error) => ("error", String::new(), Some(error)),
    };
    let entry = terminal_entry(&self.option.id, &self.requested_at, state, &answer, error.as_deref());
    let store = Arc::clone(&self.features.store);
    let session_id = self.session_id.clone();
    let upto = self.upto;
    blocking(move || write_entry(&results_path(&store, &session_id), upto, entry)).await?;
    let frame = json!({"type": "explain_status", "upto": upto, "state": state, "backend": self.option.id});
    self.features.store.broadcast(&session_channel(&self.session_id), frame).await?;
    tracing::info!(session_id = %self.session_id, upto = self.upto, state, "explain_finished");
    Ok(())
  }
}

/// The error text an entry carries: the innermost message, the timeout's class name, or the error's own name when
/// the message is empty.
fn exception_text(error: &anyhow::Error) -> String {
  if error.chain().any(|cause| cause.is::<tokio::time::error::Elapsed>()) {
    return TIMEOUT_ERROR.to_string();
  }
  let root = error.root_cause();
  let text = root.to_string();
  if text.is_empty() { format!("{root:?}") } else { text }
}

/// Starts the generation as a detached task, registered under its divider until the task ends.
fn spawn_generation(generation: Generation) {
  let key = (generation.session_id.clone(), generation.upto);
  let finished = key.clone();
  let mut tasks = TASKS.lock().expect("the explain task registry is never poisoned");
  let handle = tokio::spawn(async move {
    generation.generate(ONESHOT_TIMEOUT).await;
    TASKS.lock().expect("the explain task registry is never poisoned").remove(&finished);
  });
  tasks.insert(key, handle);
}

/// `request_explain`: register the divider's generation, or return the live pending entry. Returns the entry and
/// whether this call registered a new generation.
pub async fn request_explain(
  features: &Arc<FeatureContext>,
  session_id: &str,
  upto: i64,
  option: BackendOption,
  cfg: Arc<CharlieBotConfig>,
) -> anyhow::Result<(Map<String, Value>, bool)> {
  let _register = REGISTER_LOCK.lock().await;
  let path = results_path(&features.store, session_id);
  let load_path = path.clone();
  let results = blocking(move || load_results(&load_path)).await?;
  if let Some(value) = results.get(&upto.to_string()) {
    let entry = as_entry(value)?;
    if is_pending(entry) {
      if !is_stale(entry)? {
        return Ok((entry.clone(), false));
      }
      reap(features, session_id, upto, entry.clone()).await?;
    }
  }
  let requested_at = timefmt::utc_now_iso();
  let fresh = pending_entry(&option.id, &requested_at);
  let write = fresh.clone();
  blocking(move || write_entry(&path, upto, write)).await?;
  spawn_generation(Generation {
    features: Arc::clone(features),
    session_id: session_id.to_string(),
    upto,
    option,
    cfg,
    requested_at,
  });
  Ok((fresh, true))
}

/// `get_explain_entry`: the divider's entry, reaped first when it is a stale pending one. `None` when there is no
/// entry.
pub async fn get_explain_entry(
  features: &FeatureContext,
  session_id: &str,
  upto: i64,
) -> anyhow::Result<Option<Map<String, Value>>> {
  let path = results_path(&features.store, session_id);
  let results = blocking(move || load_results(&path)).await?;
  let Some(value) = results.get(&upto.to_string()) else {
    return Ok(None);
  };
  let entry = as_entry(value)?;
  if is_pending(entry) && is_stale(entry)? {
    return reap(features, session_id, upto, entry.clone()).await.map(Some);
  }
  Ok(Some(entry.clone()))
}

/// `explain_status`: every entry's `{state, backend, generated_at}` in file order, bodies excluded. A stale pending
/// entry is reaped on the way.
pub async fn explain_status(features: &FeatureContext, session_id: &str) -> anyhow::Result<Map<String, Value>> {
  let path = results_path(&features.store, session_id);
  let results = blocking(move || load_results(&path)).await?;
  let mut summary = Map::new();
  for (key, value) in results {
    let mut entry = as_entry(&value)?.clone();
    if is_pending(&entry) && is_stale(&entry)? {
      entry = reap(features, session_id, key.parse::<i64>()?, entry).await?;
    }
    let generated_at = entry.get("generated_at").cloned().unwrap_or(Value::Null);
    summary.insert(
      key,
      json!({"state": field(&entry, "state")?, "backend": field(&entry, "backend")?, "generated_at": generated_at}),
    );
  }
  Ok(summary)
}

#[cfg(test)]
mod tests {
  use std::os::unix::fs::PermissionsExt;
  use std::time::SystemTime;

  use super::*;
  use crate::test_support::{
    Home, OPTION_ID, Start, answer_script, delayed_answer_script, empty_script, failing_script, hanging_script,
    subscribe,
  };

  const ANSWER: &str = "The round set up the plan.";

  fn option_of(home: &Home) -> BackendOption {
    home.features.config.get().expect("the config loads").get_backend_option(OPTION_ID).expect("the option").clone()
  }

  fn cfg_of(home: &Home) -> Arc<CharlieBotConfig> {
    home.features.config.get().expect("the config loads")
  }

  /// A generation of `upto` in `session_id`, with the request time the registration would have stamped.
  fn generation(home: &Home, session_id: &str, upto: i64) -> Generation {
    Generation {
      features: Arc::clone(&home.features),
      session_id: session_id.to_string(),
      upto,
      option: option_of(home),
      cfg: cfg_of(home),
      requested_at: timefmt::utc_now_iso(),
    }
  }

  async fn entry_of(home: &Home, session_id: &str, upto: i64) -> Map<String, Value> {
    get_explain_entry(&home.features, session_id, upto).await.expect("the read").expect("an entry")
  }

  fn stale_time() -> String {
    timefmt::iso_from_system_time(SystemTime::now() - ONESHOT_TIMEOUT - Duration::from_secs(1))
  }

  /// Writes one entry through the same writer the server uses.
  fn write_raw_entry(home: &Home, session_id: &str, upto: i64, entry: Map<String, Value>) {
    write_entry(&results_path(&home.features.store, session_id), upto, entry).expect("the entry writes");
  }

  #[test]
  fn a_pending_entry_renders_its_fields_in_order() {
    let entry = pending_entry("claude-sonnet", "2026-10-18T07:15:40.685829+00:00");

    assert_eq!(
      serde_json::to_string(&entry).unwrap(),
      r#"{"state":"pending","backend":"claude-sonnet","answer":"","error":null,"requested_at":"2026-10-18T07:15:40.685829+00:00","generated_at":null}"#
    );
  }

  #[test]
  fn the_results_file_is_indented_by_two_and_keeps_each_divider_in_insertion_order() {
    let home = Home::new(vec![]);
    let path = home.dir.path().join("explain_results.json");
    let first = new_entry("ready", "b", "A", None, "t0", Some("t1"));
    let second = new_entry("error", "b", "", Some("boom"), "t2", Some("t3"));

    write_entry(&path, 12, first).unwrap();
    write_entry(&path, 3, second).unwrap();
    write_entry(&path, 12, new_entry("ready", "b", "B", None, "t4", Some("t5"))).unwrap();

    assert_eq!(
      fs::read_to_string(&path).unwrap(),
      concat!(
        "{\n",
        "  \"12\": {\n",
        "    \"state\": \"ready\",\n",
        "    \"backend\": \"b\",\n",
        "    \"answer\": \"B\",\n",
        "    \"error\": null,\n",
        "    \"requested_at\": \"t4\",\n",
        "    \"generated_at\": \"t5\"\n",
        "  },\n",
        "  \"3\": {\n",
        "    \"state\": \"error\",\n",
        "    \"backend\": \"b\",\n",
        "    \"answer\": \"\",\n",
        "    \"error\": \"boom\",\n",
        "    \"requested_at\": \"t2\",\n",
        "    \"generated_at\": \"t3\"\n",
        "  }\n",
        "}",
      )
    );
  }

  #[test]
  fn a_results_file_that_is_not_json_fails_the_read() {
    let home = Home::new(vec![]);
    let path = home.dir.path().join("explain_results.json");
    fs::write(&path, "{not json").unwrap();

    let error = load_results(&path).unwrap_err();

    assert!(error.downcast_ref::<serde_json::Error>().is_some(), "{error}");
  }

  #[tokio::test]
  async fn a_ready_generation_lands_the_answer_and_its_generation_time() {
    let home = Home::new(vec![Start::Script(answer_script(ANSWER))]);
    home.create_session("s1");
    home.append_round("s1", Some("The answer.")).await;

    generation(&home, "s1", 2).generate(ONESHOT_TIMEOUT).await;

    let entry = entry_of(&home, "s1", 2).await;
    assert_eq!(entry["state"], "ready");
    assert_eq!(entry["backend"], OPTION_ID);
    assert_eq!(entry["answer"], ANSWER);
    assert_eq!(entry["error"], Value::Null);
    assert!(entry["generated_at"].is_string());
    assert_eq!(home.factory.builds(), 1);
  }

  #[tokio::test]
  async fn a_round_without_assistant_text_ends_in_the_no_text_error_before_any_agent_starts() {
    let home = Home::new(vec![]);
    home.create_session("s1");
    home.append_round("s1", None).await;

    generation(&home, "s1", 1).generate(ONESHOT_TIMEOUT).await;

    let entry = entry_of(&home, "s1", 1).await;
    assert_eq!(entry["state"], "error");
    assert_eq!(entry["answer"], "");
    assert_eq!(entry["error"], NO_ROUND_TEXT_ERROR);
    assert_eq!(home.factory.builds(), 0);
  }

  #[tokio::test]
  async fn an_empty_answer_ends_in_the_empty_error() {
    let home = Home::new(vec![Start::Script(empty_script())]);
    home.create_session("s1");
    home.append_round("s1", Some("The answer.")).await;

    generation(&home, "s1", 2).generate(ONESHOT_TIMEOUT).await;

    let entry = entry_of(&home, "s1", 2).await;
    assert_eq!(entry["state"], "error");
    assert_eq!(entry["error"], EMPTY_ANSWER_ERROR);
  }

  #[tokio::test]
  async fn an_agent_that_exits_with_an_error_and_says_nothing_ends_in_the_empty_error() {
    let home = Home::new(vec![Start::Script(failing_script())]);
    home.create_session("s1");
    home.append_round("s1", Some("The answer.")).await;

    generation(&home, "s1", 2).generate(ONESHOT_TIMEOUT).await;

    assert_eq!(entry_of(&home, "s1", 2).await["error"], EMPTY_ANSWER_ERROR);
  }

  #[tokio::test]
  async fn an_exception_with_empty_text_is_named_by_its_class() {
    let home = Home::new(vec![Start::EmptyFailure]);
    home.create_session("s1");
    home.append_round("s1", Some("The answer.")).await;

    generation(&home, "s1", 2).generate(ONESHOT_TIMEOUT).await;

    let entry = entry_of(&home, "s1", 2).await;
    assert_eq!(entry["state"], "error");
    assert_eq!(entry["error"], "EmptyFailure");
  }

  #[tokio::test]
  async fn an_exception_with_text_lands_its_message() {
    let home = Home::new(vec![]);
    home.create_session("s1");
    home.append_round("s1", Some("The answer.")).await;

    generation(&home, "s1", 2).generate(ONESHOT_TIMEOUT).await;

    assert_eq!(entry_of(&home, "s1", 2).await["error"], "the scripted backend has no start left");
  }

  #[tokio::test]
  async fn a_one_shot_past_its_budget_ends_in_the_timeout_error() {
    let home = Home::new(vec![Start::Script(hanging_script())]);
    home.create_session("s1");
    home.append_round("s1", Some("The answer.")).await;

    generation(&home, "s1", 2).generate(Duration::from_millis(200)).await;

    let entry = entry_of(&home, "s1", 2).await;
    assert_eq!(entry["state"], "error");
    assert_eq!(entry["error"], TIMEOUT_ERROR);
  }

  #[tokio::test]
  async fn the_terminal_state_is_broadcast_as_a_frame_without_the_answer() {
    let home = Home::new(vec![Start::Script(answer_script(ANSWER))]);
    home.create_session("s1");
    home.append_round("s1", Some("The answer.")).await;
    let mut frames = subscribe(&home.features.store, "s1");

    generation(&home, "s1", 2).generate(ONESHOT_TIMEOUT).await;

    let frame: Value = serde_json::from_str(&frames.recv().await.expect("a frame")).unwrap();
    assert_eq!(frame, json!({"type": "explain_status", "upto": 2, "state": "ready", "backend": OPTION_ID}));
  }

  #[tokio::test]
  async fn the_history_copy_is_read_only_and_its_directory_goes_with_it() {
    let home = Home::new(vec![]);
    home.create_session("s1");
    home.append_round("s1", Some("The answer.")).await;

    let copy = HistoryCopy::make(&home.features.store, "s1").unwrap();
    let dir = copy.path.parent().unwrap().to_path_buf();

    let dir_name = dir.file_name().unwrap().to_string_lossy();
    let suffix = dir_name.strip_prefix("charliebot-explain-").unwrap();
    assert_eq!(suffix.len(), 8);
    assert!(suffix.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'));
    assert_eq!(fs::metadata(&copy.path).unwrap().permissions().mode() & 0o777, 0o444);
    assert_eq!(fs::read(&copy.path).unwrap(), fs::read(home.features.store.chat_events_path("s1")).unwrap());
    drop(copy);
    assert!(!dir.exists());
  }

  #[tokio::test]
  async fn two_concurrent_requests_on_one_divider_start_one_generation() {
    let home = Home::new(vec![Start::Script(delayed_answer_script(1, ANSWER))]);
    home.create_session("s1");
    home.append_round("s1", Some("The answer.")).await;
    let mut frames = subscribe(&home.features.store, "s1");

    let (first, second) = tokio::join!(
      request_explain(&home.features, "s1", 2, option_of(&home), cfg_of(&home)),
      request_explain(&home.features, "s1", 2, option_of(&home), cfg_of(&home)),
    );

    let created = [first.unwrap().1, second.unwrap().1];
    assert_eq!(created.iter().filter(|created| **created).count(), 1);
    let frame: Value = serde_json::from_str(&frames.recv().await.expect("a frame")).unwrap();
    assert_eq!(frame["state"], "ready");
    assert_eq!(home.factory.builds(), 1);
  }

  #[tokio::test]
  async fn a_live_pending_entry_is_returned_without_a_new_generation() {
    let home = Home::new(vec![]);
    home.create_session("s1");
    let pending = pending_entry(OPTION_ID, &timefmt::utc_now_iso());
    write_raw_entry(&home, "s1", 2, pending.clone());

    let (entry, created) = request_explain(&home.features, "s1", 2, option_of(&home), cfg_of(&home)).await.unwrap();

    assert!(!created);
    assert_eq!(entry, pending);
    assert_eq!(home.factory.builds(), 0);
  }

  #[tokio::test]
  async fn a_stale_pending_entry_is_reaped_by_the_register_request_and_re_registered() {
    let home = Home::new(vec![]);
    home.create_session("s1");
    write_raw_entry(&home, "s1", 3, pending_entry(OPTION_ID, &stale_time()));
    let mut frames = subscribe(&home.features.store, "s1");

    let (entry, created) = request_explain(&home.features, "s1", 3, option_of(&home), cfg_of(&home)).await.unwrap();

    assert!(created);
    assert_eq!(entry["state"], "pending");
    frames.recv().await.expect("the new generation finishes");
    assert_eq!(entry_of(&home, "s1", 3).await["error"], NO_ROUND_TEXT_ERROR);
  }

  #[tokio::test]
  async fn a_stale_pending_entry_is_reaped_by_the_entry_read() {
    let home = Home::new(vec![]);
    home.create_session("s1");
    write_raw_entry(&home, "s1", 2, pending_entry(OPTION_ID, &stale_time()));

    let entry = get_explain_entry(&home.features, "s1", 2).await.unwrap().unwrap();

    assert_eq!(entry["state"], "error");
    assert_eq!(entry["backend"], OPTION_ID);
    assert_eq!(entry["answer"], "");
    assert_eq!(entry["error"], REAPED_ERROR);
    assert!(entry["generated_at"].is_string());
    assert_eq!(entry_of(&home, "s1", 2).await, entry, "the reaped entry is what the file now holds");
  }

  #[tokio::test]
  async fn a_stale_pending_entry_is_reaped_by_the_status_read_in_file_order() {
    let home = Home::new(vec![]);
    home.create_session("s1");
    write_raw_entry(&home, "s1", 5, pending_entry(OPTION_ID, &stale_time()));
    write_raw_entry(&home, "s1", 2, new_entry("ready", OPTION_ID, "A", None, "t0", Some("t1")));

    let summary = explain_status(&home.features, "s1").await.unwrap();

    assert_eq!(
      serde_json::to_value(&summary).unwrap(),
      json!({
        "5": {"state": "error", "backend": OPTION_ID, "generated_at": summary["5"]["generated_at"].clone()},
        "2": {"state": "ready", "backend": OPTION_ID, "generated_at": "t1"},
      })
    );
    assert_eq!(entry_of(&home, "s1", 5).await["error"], REAPED_ERROR);
  }

  #[tokio::test]
  async fn a_missing_file_has_no_entry_and_an_empty_status() {
    let home = Home::new(vec![]);
    home.create_session("s1");

    assert!(get_explain_entry(&home.features, "s1", 2).await.unwrap().is_none());
    assert!(explain_status(&home.features, "s1").await.unwrap().is_empty());
  }

  #[tokio::test]
  async fn a_timeout_is_named_by_its_class_and_a_deeper_message_by_its_root() {
    let elapsed = tokio::time::timeout(Duration::from_millis(1), std::future::pending::<()>()).await.unwrap_err();
    let timeout = anyhow::Error::new(elapsed).context("running the one-shot");
    assert_eq!(exception_text(&timeout), TIMEOUT_ERROR);

    let wrapped = anyhow::anyhow!("inner failure").context("running the one-shot");
    assert_eq!(exception_text(&wrapped), "inner failure");
  }
}
