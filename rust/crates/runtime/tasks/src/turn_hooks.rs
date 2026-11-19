//! The turn-end hook list: the mechanical checks a turn passes before its `master_done` is written.
//!
//! The list is `hooks/turn_end.json` in the served repo: an array of `{name, command}`. Each command runs under `sh -c`
//! with one shared JSON object on stdin (`TurnInput`). Its result follows Claude Code's Stop hooks: exit 0 passes, a
//! stdout JSON object with `decision: "block"` and a `reason` blocks, and a `systemMessage` is the entry's notice;
//! exit 2 blocks with stderr as the reason. The session's turn contribution attaches a notice to a message of the turn
//! when the turn has one; otherwise the runner posts it as an agent message from the entry's name. Any other exit, a
//! timeout, or a stdout that is not a JSON object is an `error`: it is shown, and the turn ends without continuing.
//!
//! A checkpoint runs every entry in list order and writes one `turn_hooks` event with all the results. Each block's
//! reason is shown in the session, and the same conversation continues with the reasons, at most `MAX_CONTINUATIONS`
//! times. Every step is derived from the session's event log, so a restart resumes the step a crash interrupted.

use std::path::Path;
use std::process::{Output, Stdio};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use charliebot_agents::transport::kill::kill_process_group;
use charliebot_core::RepoRoot;
use charliebot_core::event_types::{
  AGENT_MESSAGE, FROM_SESSION_NAME, MASTER_DONE, TURN_HOOKS, TURN_HOOKS_ID, WEB_STATUS,
};
use charliebot_registry::FeatureContext;
use charliebot_registry::turn::TurnContribution;
use charliebot_store::MessageProvenance;
use charliebot_store::metadata::{SessionMetadata, UtcDateTime};
use charliebot_store::reply_order::{self, Receipt};
use nix::sys::signal::Signal;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// The file that holds the turn-end hook list, relative to the served repo.
pub const HOOKS_FILE: &str = "hooks/turn_end.json";
/// The blocks that continue one turn. The block after them ends the turn with its reason shown.
pub const MAX_CONTINUATIONS: usize = 8;
/// The time limit of one entry's command.
pub const ENTRY_TIMEOUT: Duration = Duration::from_secs(30);

/// One check of the list.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
  pub name: String,
  pub command: String,
}

/// The shared JSON object every command reads on stdin.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TurnInput {
  pub session_id: String,
  /// `web`, a thread platform's name (`slack`, `discord`, `feishu`), or null for a plain manager session.
  pub kind: Option<String>,
  /// The turn's exit code; a zero-output turn carries 1.
  pub exit_code: i64,
  /// Whether the turn is the continuation of a block and exited with an error. Such a turn misses the entries that
  /// blocked it again, so each of those entries answers its second miss rather than passing the turn.
  pub continuation_failed: bool,
  /// The session's stored status payload (goal, now, updated_at, waiting), or null when it stores none.
  pub status: Value,
  /// The events since the previous `master_done`, in order, up to the call. An input the running run received carries
  /// `received_at`, the moment of its receipt.
  pub turn: Vec<Value>,
}

impl TurnInput {
  /// Whether a check judges this turn's exit: a turn that exited cleanly, or a failed continuation, which a check
  /// judges as a miss. Any other error exit is not judged, so the check passes it.
  pub fn exit_is_judged(&self) -> bool {
    self.exit_code == 0 || self.continuation_failed
  }
}

/// A check command's answer, as the command prints it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckResult {
  Pass,
  /// The check passes and its notice reaches the user.
  Notice(String),
  /// The check misses; the reason continues the turn.
  Block(String),
}

impl CheckResult {
  /// The command's stdout for this answer. A pass prints nothing.
  pub fn stdout(&self) -> Option<String> {
    match self {
      Self::Pass => None,
      Self::Notice(message) => Some(json!({"systemMessage": message}).to_string()),
      Self::Block(reason) => Some(json!({"decision": "block", "reason": reason}).to_string()),
    }
  }
}

/// One entry's outcome after its command ran.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
  Pass { notice: Option<String> },
  Block { reason: String },
  Error { message: String },
}

/// One entry's name and outcome.
#[derive(Clone, Debug)]
pub struct Ran {
  pub name: String,
  pub outcome: Outcome,
}

/// The outcome of one command from its exit code and output, under Claude Code's Stop-hook rules.
pub fn outcome_of(exit: Option<i32>, stdout: &str, stderr: &str) -> Outcome {
  let stderr = stderr.trim();
  match exit {
    Some(0) => {
      let stdout = stdout.trim();
      if stdout.is_empty() {
        return Outcome::Pass { notice: None };
      }
      let object = match serde_json::from_str::<Value>(stdout) {
        Ok(Value::Object(object)) => object,
        _ => return Outcome::Error { message: "stdout is not a JSON object".to_string() },
      };
      if object.get("decision").and_then(Value::as_str) == Some("block") {
        return match object.get("reason").and_then(Value::as_str).filter(|reason| !reason.is_empty()) {
          Some(reason) => Outcome::Block { reason: reason.to_string() },
          None => Outcome::Error { message: "a block names no reason".to_string() },
        };
      }
      Outcome::Pass { notice: object.get("systemMessage").and_then(Value::as_str).map(str::to_string) }
    }
    Some(2) => {
      let json_reason = serde_json::from_str::<Value>(stdout.trim()).ok().and_then(|value| {
        let object = value.as_object()?;
        (object.get("decision").and_then(Value::as_str) == Some("block"))
          .then(|| object.get("reason").and_then(Value::as_str).filter(|reason| !reason.is_empty()))
          .flatten()
          .map(str::to_string)
      });
      if let Some(reason) = json_reason {
        Outcome::Block { reason }
      } else if !stderr.is_empty() {
        Outcome::Block { reason: stderr.to_string() }
      } else {
        Outcome::Error { message: "exit 2 with no reason on stderr or JSON".to_string() }
      }
    }
    Some(code) if stderr.is_empty() => Outcome::Error { message: format!("exit {code}") },
    Some(code) => Outcome::Error { message: format!("exit {code}: {stderr}") },
    None => Outcome::Error { message: "the command was killed by a signal".to_string() },
  }
}

/// Runs every entry in list order with `input` on stdin and `cwd` as its directory. An entry that exceeds `timeout` is
/// killed, and its outcome is an error.
pub async fn run_entries(
  entries: &[Entry],
  input: &TurnInput,
  cwd: &Path,
  timeout: Duration,
) -> anyhow::Result<Vec<Ran>> {
  let payload = serde_json::to_vec(input).context("encode the turn-end check input")?;
  let mut ran = Vec::with_capacity(entries.len());
  for entry in entries {
    let outcome = run_entry(entry, &payload, cwd, timeout).await?;
    ran.push(Ran { name: entry.name.clone(), outcome });
  }
  Ok(ran)
}

async fn run_entry(entry: &Entry, payload: &[u8], cwd: &Path, timeout: Duration) -> anyhow::Result<Outcome> {
  // The check leads its own process group, so a timeout kills the commands it started, not only the shell.
  let child = Command::new("sh")
    .arg("-c")
    .arg(&entry.command)
    .current_dir(cwd)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .process_group(0)
    .kill_on_drop(true)
    .spawn()
    .with_context(|| format!("start turn-end check {}", entry.name))?;
  let pid = child.id().with_context(|| format!("turn-end check {} has no process id", entry.name))?;
  let run = async move {
    let mut child = child;
    let mut stdin = child.stdin.take().context("the check's stdin is piped")?;
    // A command that exits without reading its input closes the pipe. Its exit decides the outcome.
    match stdin.write_all(payload).await {
      Ok(()) => {}
      Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {}
      Err(error) => return Err(error).context("write the turn-end check input"),
    }
    drop(stdin);
    child.wait_with_output().await.context("wait for the turn-end check")
  };
  match tokio::time::timeout(timeout, run).await {
    Ok(output) => {
      let Output { status, stdout, stderr } = output.with_context(|| format!("run turn-end check {}", entry.name))?;
      Ok(outcome_of(status.code(), &String::from_utf8_lossy(&stdout), &String::from_utf8_lossy(&stderr)))
    }
    Err(_) => {
      kill_process_group(pid, Signal::SIGKILL);
      Ok(Outcome::Error { message: format!("timed out after {} s", timeout.as_secs()) })
    }
  }
}

/// The turn-end hook list of the served repo. A missing or malformed list fails the checkpoint.
pub fn load_entries(repo: &RepoRoot) -> anyhow::Result<Vec<Entry>> {
  let path = repo.path().join(HOOKS_FILE);
  let text =
    std::fs::read_to_string(&path).with_context(|| format!("read the turn-end hook list {}", path.display()))?;
  serde_json::from_str(&text).with_context(|| format!("parse the turn-end hook list {}", path.display()))
}

/// Whether a run end belongs to an existing failure or quota handoff instead of the normal turn-end checkpoint.
pub(crate) fn skips_checkpoint(finish_extras: &serde_json::Map<String, Value>) -> bool {
  finish_extras.contains_key(crate::master_run::FINISH_FAILURE_REASON)
    || finish_extras.contains_key(crate::master_run::FINISH_QUOTA_EXHAUSTED)
}

/// The checkpoint's answer: the turn ends, or it continues the same conversation with this prompt.
#[derive(Debug, PartialEq, Eq)]
pub enum Checkpoint {
  Finish,
  Continue(String),
}

/// The session's log as the checkpoint reads it.
struct SessionLog {
  /// The events since the previous `master_done`, with `received_at` on the inputs the running run received.
  turn: Vec<Value>,
  /// The payload of the session's latest stored status, or null.
  status: Value,
}

fn read_log(features: &FeatureContext, session_id: &str) -> anyhow::Result<SessionLog> {
  let loaded = features.store.load_chat_events_shared_sync(session_id)?;
  let receipts = reply_order::running_receipts(&features.store, session_id, loaded.as_slice())?;
  let mut events: Vec<Value> = loaded.iter().map(|event| event.as_ref().to_value()).collect();
  let status = events
    .iter()
    .rev()
    .find(|event| event["type"] == WEB_STATUS)
    .map_or(Value::Null, |event| event[WEB_STATUS].clone());
  let start = events.iter().rposition(|event| event["type"] == MASTER_DONE).map_or(0, |index| index + 1);
  let mut turn = events.split_off(start);
  for event in &mut turn {
    let Some(id) = event["id"].as_str() else { continue };
    if let Some(receipt) = receipts.iter().find(|receipt| receipt.input_event_id == id) {
      event["received_at"] = json!(receipt.at.to_str());
    }
  }
  Ok(SessionLog { turn, status })
}

/// The turn's received inputs, each with the moment the running run received it, from the `received_at` stamps.
pub fn received_receipts(turn: &[Value]) -> anyhow::Result<Vec<Receipt>> {
  let mut receipts = Vec::new();
  for event in turn {
    let Some(received_at) = event["received_at"].as_str() else { continue };
    let at = UtcDateTime::parse(received_at).with_context(|| format!("parse received_at {received_at}"))?;
    let input_event_id = event["id"].as_str().unwrap_or_default().to_string();
    receipts.push(Receipt { input_event_id, run_id: String::new(), at });
  }
  Ok(receipts)
}

/// Whether `event` is a `turn_hooks` record with at least one block.
fn is_block_record(event: &Value) -> bool {
  event["type"] == TURN_HOOKS
    && event["results"].as_array().is_some_and(|results| results.iter().any(|result| result["result"] == "block"))
}

/// Whether `event` shows the reason of the `turn_hooks` record `record_id`.
fn shows_reason_of(event: &Value, record_id: &str) -> bool {
  event["type"] == AGENT_MESSAGE && event[TURN_HOOKS_ID].as_str() == Some(record_id)
}

/// The turn's block that its continuation has not yet answered: the last block record with no event after it except
/// the display of its own reason. A block record with a later event, even a later `turn_hooks` record, has run its
/// continuation.
fn pending_block(turn: &[Value]) -> Option<usize> {
  let index = turn.iter().rposition(is_block_record)?;
  let record_id = turn[index]["id"].as_str()?;
  let continued = turn[index + 1..].iter().any(|event| !shows_reason_of(event, record_id));
  (!continued).then_some(index)
}

/// How many blocks the turn recorded before `turn`'s last event: the cap counts the block records.
fn blocks_before(turn: &[Value], end: usize) -> usize {
  turn[..end].iter().filter(|event| is_block_record(event)).count()
}

/// The continuation prompt for a block record: each blocked entry's reason, opened by its name.
fn continuation_prompt(record: &Value) -> String {
  record["results"]
    .as_array()
    .into_iter()
    .flatten()
    .filter(|result| result["result"] == "block")
    .map(|result| {
      format!("{}: {}", result["name"].as_str().unwrap_or_default(), result["reason"].as_str().unwrap_or_default())
    })
    .collect::<Vec<_>>()
    .join("\n\n")
}

/// Persists one agent message from `sender` into the session and returns the persisted event.
async fn show_message(
  features: &FeatureContext,
  session_id: &str,
  sender: &str,
  text: &str,
  record_id: Option<&str>,
) -> anyhow::Result<Value> {
  let provenance = MessageProvenance { from_session: session_id.to_string(), from_session_name: sender.to_string() };
  let mut event = features.store.build_agent_message_event(session_id, text, provenance);
  if let Some(record_id) = record_id {
    event[TURN_HOOKS_ID] = json!(record_id).into();
  }
  let persisted = features.store.persist_and_broadcast(session_id, event.to_value()).await?;
  Ok(persisted.to_value())
}

/// Delivers a passed check's notice. The session's turn contribution attaches it to a message of the turn when the turn
/// has one, and a session no contribution owns has none. Otherwise the runner posts it from the entry's name and marks
/// the session unread. A turn that already holds the notice gets no second one, so a replayed checkpoint adds nothing.
async fn deliver_notice(
  features: &Arc<FeatureContext>,
  session_id: &str,
  owner: Option<&Arc<dyn TurnContribution>>,
  turn: &[Value],
  name: &str,
  message: &str,
) -> anyhow::Result<()> {
  if let Some(contribution) = owner
    && contribution.show_turn_notice(features, session_id, turn, message).await?
  {
    return Ok(());
  }
  if notice_posted(turn, name, message) {
    return Ok(());
  }
  show_message(features, session_id, name, message, None).await?;
  features.store.mark_unread(session_id).await?;
  Ok(())
}

/// Whether the turn already holds the runner's notice `message` from the entry `name`. A block's reason carries its
/// record's id and the notice carries none, so a block never counts as the notice.
fn notice_posted(turn: &[Value], name: &str, message: &str) -> bool {
  turn.iter().any(|event| {
    event["type"] == AGENT_MESSAGE
      && event[FROM_SESSION_NAME] == name
      && event["content"] == message
      && event[TURN_HOOKS_ID].is_null()
  })
}

/// The turn-end checkpoint of the running turn of `session_id`. `has_conversation` says whether the turn's conversation
/// id is known: a continuation resumes it, and without one the turn ends with an error.
pub(crate) async fn checkpoint(
  features: &Arc<FeatureContext>,
  session_id: &str,
  meta: &SessionMetadata,
  exit_code: i64,
  has_conversation: bool,
) -> anyhow::Result<Checkpoint> {
  let log = read_log(features, session_id)?;
  let owner = features.registry.turn_owner(meta);
  if let Some(index) = pending_block(&log.turn) {
    return resume_block(features, session_id, &log, index, has_conversation).await;
  }

  let checkout = features.startup.require::<RepoRoot>()?;
  let entries = load_entries(&checkout)?;
  let input = TurnInput {
    session_id: session_id.to_string(),
    kind: owner.as_ref().map(|(kind, _)| (*kind).to_string()),
    exit_code,
    // Past the pending check, a block in this turn has run its continuation, and this checkpoint judges that run.
    continuation_failed: exit_code != 0 && log.turn.iter().any(is_block_record),
    status: log.status.clone(),
    turn: log.turn.clone(),
  };
  let mut ran = run_entries(&entries, &input, checkout.path(), ENTRY_TIMEOUT).await?;
  let contribution = owner.as_ref().map(|(_, contribution)| contribution);
  for run in &mut ran {
    let Outcome::Pass { notice: Some(message) } = &run.outcome else { continue };
    let delivered = deliver_notice(features, session_id, contribution, &log.turn, &run.name, message).await;
    if let Err(error) = delivered {
      run.outcome = Outcome::Error { message: format!("notice delivery failed: {error:#}") };
    }
  }
  let results: Vec<Value> = ran.iter().map(result_json).collect();
  let record =
    features.store.persist_and_broadcast(session_id, json!({"type": TURN_HOOKS, "results": results})).await?;
  record["id"].as_str().context("the persisted turn_hooks record carries its id")?;
  let record_value = record.to_value();

  if let Some(Ran { name, outcome: Outcome::Error { message } }) =
    ran.iter().find(|run| matches!(run.outcome, Outcome::Error { .. }))
  {
    let text = format!("Turn-end check {name} failed: {message}");
    show_error(features, session_id, &text).await?;
    return Ok(Checkpoint::Finish);
  }
  if !ran.iter().any(|run| matches!(run.outcome, Outcome::Block { .. })) {
    return Ok(Checkpoint::Finish);
  }
  show_reasons(features, session_id, &record_value, 0).await?;
  if blocks_before(&log.turn, log.turn.len()) >= MAX_CONTINUATIONS {
    return Ok(Checkpoint::Finish);
  }
  if !has_conversation {
    show_no_conversation(features, session_id).await?;
    return Ok(Checkpoint::Finish);
  }
  Ok(Checkpoint::Continue(continuation_prompt(&record_value)))
}

/// Resumes a block whose continuation has not run: the reasons are shown again where their display was lost, then the
/// same conversation continues. The cap and the missing conversation end the turn as they did when the block was
/// recorded.
async fn resume_block(
  features: &Arc<FeatureContext>,
  session_id: &str,
  log: &SessionLog,
  index: usize,
  has_conversation: bool,
) -> anyhow::Result<Checkpoint> {
  let record = &log.turn[index];
  let record_id = record["id"].as_str().context("a turn_hooks record carries its id")?;
  let shown = log.turn[index + 1..].iter().filter(|event| shows_reason_of(event, record_id)).count();
  show_reasons(features, session_id, record, shown).await?;
  if blocks_before(&log.turn, index) >= MAX_CONTINUATIONS {
    return Ok(Checkpoint::Finish);
  }
  if !has_conversation {
    show_no_conversation(features, session_id).await?;
    return Ok(Checkpoint::Finish);
  }
  Ok(Checkpoint::Continue(continuation_prompt(record)))
}

/// Shows the reason of each block in `record` from the `skip`-th on, one agent message per blocked entry. The message's
/// sender is the entry's name in the hook list, so each entry labels its own blocks.
async fn show_reasons(features: &FeatureContext, session_id: &str, record: &Value, skip: usize) -> anyhow::Result<()> {
  let record_id = record["id"].as_str().context("a turn_hooks record carries its id")?;
  let blocks = record["results"].as_array().into_iter().flatten().filter(|result| result["result"] == "block");
  for result in blocks.skip(skip) {
    let name = result["name"].as_str().context("a turn_hooks block carries its entry name")?;
    show_message(features, session_id, name, result["reason"].as_str().unwrap_or_default(), Some(record_id)).await?;
  }
  Ok(())
}

async fn show_no_conversation(features: &FeatureContext, session_id: &str) -> anyhow::Result<()> {
  let text = "Turn-end check continuation needs the conversation id, and this backend stored none. The turn ends.";
  show_error(features, session_id, text).await
}

/// Persists `text` as a visible error event of the session, so the checkpoint's own notice is shown without becoming a
/// pending input that would start another turn.
async fn show_error(features: &FeatureContext, session_id: &str, text: &str) -> anyhow::Result<()> {
  features.store.persist_and_broadcast(session_id, charliebot_agents::event_builders::make_error_event(text)).await?;
  features.store.mark_unread(session_id).await?;
  Ok(())
}

fn result_json(run: &Ran) -> Value {
  match &run.outcome {
    Outcome::Pass { notice } => {
      let mut result = json!({"name": run.name, "result": "pass"});
      if let Some(notice) = notice {
        result["system_message"] = json!(notice);
      }
      result
    }
    Outcome::Block { reason } => json!({"name": run.name, "result": "block", "reason": reason}),
    Outcome::Error { message } => json!({"name": run.name, "result": "error", "reason": message}),
  }
}

#[cfg(test)]
mod tests {
  use charliebot_core::event_types::{ERROR, SCHEDULED_TRIGGER, USER};
  use charliebot_registry::{Registry, RegistryError};
  use futures_util::future::BoxFuture;

  use super::*;

  fn entry(name: &str, command: &str) -> Entry {
    Entry { name: name.to_string(), command: command.to_string() }
  }

  fn input(turn: Vec<Value>) -> TurnInput {
    TurnInput {
      session_id: "synthetic-session".to_string(),
      kind: Some("web".to_string()),
      exit_code: 0,
      continuation_failed: false,
      status: Value::Null,
      turn,
    }
  }

  /// A `turn_hooks` record, with the given results, named `id`.
  fn record(id: &str, results: Value) -> Value {
    json!({"type": TURN_HOOKS, "id": id, "results": results})
  }

  fn block_result(name: &str, reason: &str) -> Value {
    json!({"name": name, "result": "block", "reason": reason})
  }

  #[test]
  fn a_stop_hook_answer_reads_by_exit_code_and_stdout() {
    assert_eq!(outcome_of(Some(0), "", ""), Outcome::Pass { notice: None });
    assert_eq!(
      outcome_of(Some(0), r#"{"decision": "block", "reason": "post the reply"}"#, ""),
      Outcome::Block { reason: "post the reply".to_string() }
    );
    assert_eq!(
      outcome_of(Some(0), r#"{"systemMessage": "the thread was told"}"#, ""),
      Outcome::Pass { notice: Some("the thread was told".to_string()) }
    );
    assert_eq!(outcome_of(Some(2), "", "missing reply\n"), Outcome::Block { reason: "missing reply".to_string() });
    assert_eq!(
      outcome_of(Some(2), r#"{"decision":"block","reason":"JSON reason"}"#, "stderr reason"),
      Outcome::Block { reason: "JSON reason".to_string() }
    );
    assert_eq!(
      outcome_of(Some(2), r#"{"decision":"block","reason":"JSON reason"}"#, ""),
      Outcome::Block { reason: "JSON reason".to_string() }
    );
  }

  #[test]
  fn a_command_error_is_an_error_and_never_a_pass_or_a_block() {
    assert!(matches!(outcome_of(Some(0), "not json", ""), Outcome::Error { .. }));
    assert!(matches!(outcome_of(Some(0), r#"["a list"]"#, ""), Outcome::Error { .. }));
    assert!(matches!(outcome_of(Some(0), r#"{"decision": "block"}"#, ""), Outcome::Error { .. }));
    assert!(matches!(outcome_of(Some(2), "", ""), Outcome::Error { .. }));
    assert!(matches!(outcome_of(Some(1), "", "boom"), Outcome::Error { message } if message == "exit 1: boom"));
    assert!(matches!(outcome_of(None, "", ""), Outcome::Error { .. }));
  }

  #[test]
  fn failure_and_quota_finish_extras_skip_the_normal_checkpoint() {
    let mut failure = serde_json::Map::new();
    failure.insert(crate::master_run::FINISH_FAILURE_REASON.to_string(), json!("quota"));
    let mut quota = serde_json::Map::new();
    quota.insert(crate::master_run::FINISH_QUOTA_EXHAUSTED.to_string(), json!(true));
    assert!(skips_checkpoint(&failure));
    assert!(skips_checkpoint(&quota));
    assert!(!skips_checkpoint(&serde_json::Map::new()));
  }

  #[tokio::test]
  async fn the_shared_input_reaches_each_command_on_stdin_and_a_failure_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let entries = [entry("echo-input", "cat > input.json; exit 0"), entry("fails", "exit 3")];
    let ran =
      run_entries(&entries, &input(vec![json!({"id": "u1"})]), dir.path(), Duration::from_secs(5)).await.unwrap();

    let received: Value =
      serde_json::from_str(&std::fs::read_to_string(dir.path().join("input.json")).unwrap()).unwrap();
    assert_eq!(received["session_id"], "synthetic-session");
    assert_eq!(received["turn"][0]["id"], "u1");
    assert_eq!(ran[0].outcome, Outcome::Pass { notice: None });
    assert!(matches!(&ran[1].outcome, Outcome::Error { message } if message == "exit 3"), "{:?}", ran[1]);
  }

  // The limit is far shorter than the real one so this case stays a fast unit test.
  #[tokio::test]
  async fn a_command_past_its_time_limit_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let entries = [entry("hangs", "sleep 30")];
    let ran = run_entries(&entries, &input(vec![]), dir.path(), Duration::from_millis(20)).await.unwrap();

    assert!(matches!(&ran[0].outcome, Outcome::Error { message } if message.starts_with("timed out")), "{:?}", ran[0]);
  }

  #[test]
  fn a_block_continues_with_each_blocked_entry_named_in_the_prompt() {
    let record = record(
      "r1",
      json!([
        {"name": "web-reply", "result": "pass"},
        block_result("web-reply", "answer the question"),
        block_result("web-status", "set the status"),
      ]),
    );
    assert_eq!(continuation_prompt(&record), "web-reply: answer the question\n\nweb-status: set the status");
  }

  #[test]
  fn the_four_crash_states_read_from_the_log() {
    // 1. No block in the turn: the checkpoint runs the list.
    assert_eq!(pending_block(&[json!({"type": "user", "id": "u1"})]), None);

    // 2. A block recorded, nothing after it: the continuation has not run, so it resumes with the stored reason.
    let blocked = record("r1", json!([block_result("web-reply", "answer")]));
    assert_eq!(pending_block(std::slice::from_ref(&blocked)), Some(0));

    // 3. A block recorded, and its reason shown (the display is not a continuation): still resumes.
    let shown = json!({"type": AGENT_MESSAGE, "id": "m1", TURN_HOOKS_ID: "r1", "content": "answer"});
    assert_eq!(pending_block(&[blocked.clone(), shown.clone()]), Some(0));

    // 4. A block recorded, then a continuation event: the continuation ran, so nothing is pending and the checkpoint
    // runs the list again.
    let reply = json!({"type": "web_reply", "id": "w1"});
    assert_eq!(pending_block(&[blocked, shown, reply]), None);
  }

  #[test]
  fn a_later_list_run_after_a_continuation_is_not_a_pending_block() {
    let blocked = record("r1", json!([block_result("web-reply", "answer")]));
    let passed = record("r2", json!([{"name": "web-reply", "result": "pass"}]));
    assert_eq!(pending_block(&[blocked, passed]), None);
  }

  #[test]
  fn the_cap_counts_the_block_records_before_the_turn_ends() {
    let block = record("r", json!([block_result("web-reply", "answer")]));
    let turn: Vec<Value> = (0..MAX_CONTINUATIONS).map(|_| block.clone()).collect();
    assert_eq!(blocks_before(&turn, turn.len()), MAX_CONTINUATIONS);
    assert_eq!(blocks_before(&turn, 0), 0);
  }

  #[test]
  fn a_received_input_carries_its_receipt_time() {
    let turn =
      [json!({"type": "user", "id": "u1", "received_at": "2026-01-01T00:00:01Z"}), json!({"type": "user", "id": "u2"})];
    let receipts = received_receipts(&turn).unwrap();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].input_event_id, "u1");
    assert_eq!(receipts[0].at, UtcDateTime::parse("2026-01-01T00:00:01Z").unwrap());
  }

  /// A turn-end list with one entry `gate` that runs `command`.
  fn gate_list(command: &str) -> String {
    json!([{"name": "gate", "command": command}]).to_string()
  }

  /// Blocks until the turn holds a continuation's event, then passes.
  const GATE_PASSES_AFTER_CONTINUATION: &str =
    "grep -q continuation_ran && exit 0; printf '{\"decision\":\"block\",\"reason\":\"needs work\"}'";
  const GATE_ALWAYS_BLOCKS: &str = "printf '{\"decision\":\"block\",\"reason\":\"needs work\"}'";
  /// Blocks, and on a failed continuation answers its second miss with a notice.
  const GATE_NOTICES_A_FAILED_CONTINUATION: &str = concat!(
    "if grep -q '\"continuation_failed\":true'; then printf '{\"systemMessage\":\"failed continuation\"}'; ",
    "else printf '{\"decision\":\"block\",\"reason\":\"needs work\"}'; fi"
  );

  /// The event that `AttachingTurns` persists for each notice it attaches.
  const ATTACHED_NOTICE: &str = "test_attached_notice";

  /// A turn contribution that owns every manager session's turn-end checks, as the web contribution does for web
  /// sessions. It attaches a notice when the turn holds a user message, and has no message to attach to otherwise.
  struct AttachingTurns;

  impl TurnContribution for AttachingTurns {
    fn turn_kind(&self, _meta: &SessionMetadata) -> Option<&'static str> {
      Some("web")
    }

    fn show_turn_notice<'a>(
      &'a self,
      features: &'a Arc<FeatureContext>,
      session_id: &'a str,
      turn: &'a [Value],
      message: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<bool>> {
      Box::pin(async move {
        if !turn.iter().any(|event| event["type"] == USER) {
          return Ok(false);
        }
        features.store.persist_and_broadcast(session_id, json!({"type": ATTACHED_NOTICE, "content": message})).await?;
        Ok(true)
      })
    }
  }

  fn register_attaching_turns(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.turn_contribution("attaching-turns", Arc::new(AttachingTurns))
  }

  /// A manager session in a fresh checkout at `home` whose turn-end list is `hooks`. Its turns are owned by
  /// `AttachingTurns`.
  async fn checkpoint_session(home: &Path, hooks: &str, request_id: &str) -> (Arc<FeatureContext>, SessionMetadata) {
    let features = crate::test_fixtures::features_with_packages(
      home,
      &crate::test_fixtures::claude_config(None),
      &[("backends.claude_code", charliebot_claude_code::register), ("test.attaching_turns", register_attaching_turns)],
    );
    crate::test_fixtures::insert_checkout(&features.startup, home);
    std::fs::write(home.join(HOOKS_FILE), hooks).unwrap();
    let meta = crate::test_fixtures::manager_session(&features, request_id, "fable", None, "cc-synthetic").await;
    (features, meta)
  }

  async fn append(features: &FeatureContext, session_id: &str, event: Value) {
    features.store.append_chat_event(session_id, event).await.unwrap();
  }

  fn turn_events(features: &FeatureContext, session_id: &str) -> Vec<Value> {
    read_log(features, session_id).unwrap().turn
  }

  fn count_of(turn: &[Value], event_type: &str) -> usize {
    turn.iter().filter(|event| event["type"] == event_type).count()
  }

  #[tokio::test]
  async fn a_block_replays_without_a_second_record_until_its_continuation_runs_then_passes() {
    let home = tempfile::tempdir().unwrap();
    let (features, meta) =
      checkpoint_session(home.path(), &gate_list(GATE_PASSES_AFTER_CONTINUATION), "turn-hooks-pass").await;
    let session_id = &meta.id;

    let first = checkpoint(&features, session_id, &meta, 0, true).await.unwrap();
    assert_eq!(first, Checkpoint::Continue("gate: needs work".to_string()));

    // A restart before the continuation ran replays the same block: no second record and no second reason.
    let replayed = checkpoint(&features, session_id, &meta, 0, true).await.unwrap();
    assert_eq!(replayed, first);
    let turn = turn_events(&features, session_id);
    assert_eq!(count_of(&turn, TURN_HOOKS), 1);
    assert_eq!(count_of(&turn, AGENT_MESSAGE), 1);

    append(&features, session_id, json!({"type": "continuation_ran", "id": "c1"})).await;
    assert_eq!(checkpoint(&features, session_id, &meta, 0, true).await.unwrap(), Checkpoint::Finish);
    let turn = turn_events(&features, session_id);
    assert_eq!(count_of(&turn, TURN_HOOKS), 2);
    assert_eq!(count_of(&turn, AGENT_MESSAGE), 1);
    let last_record = turn.iter().rev().find(|event| event["type"] == TURN_HOOKS).unwrap();
    assert_eq!(last_record["results"][0]["result"], "pass");
  }

  #[tokio::test]
  async fn a_continuation_that_errors_reaches_its_block_as_the_second_miss() {
    let home = tempfile::tempdir().unwrap();
    let (features, meta) =
      checkpoint_session(home.path(), &gate_list(GATE_NOTICES_A_FAILED_CONTINUATION), "turn-hooks-failed").await;
    let session_id = &meta.id;

    let first = checkpoint(&features, session_id, &meta, 0, true).await.unwrap();
    assert_eq!(first, Checkpoint::Continue("gate: needs work".to_string()));
    append(&features, session_id, json!({"type": "assistant_error", "id": "e1", "content": "Agent error"})).await;

    // The continuation exited with an error: its checkpoint judges that block's second miss, passing with the notice.
    assert_eq!(checkpoint(&features, session_id, &meta, 1, true).await.unwrap(), Checkpoint::Finish);
    let turn = turn_events(&features, session_id);
    let last_record = turn.iter().rev().find(|event| event["type"] == TURN_HOOKS).unwrap();
    assert_eq!(last_record["results"][0]["result"], "pass");
    assert_eq!(last_record["results"][0]["system_message"], "failed continuation");
  }

  #[tokio::test]
  async fn a_block_that_never_passes_continues_eight_times_then_the_turn_ends() {
    let home = tempfile::tempdir().unwrap();
    let (features, meta) = checkpoint_session(home.path(), &gate_list(GATE_ALWAYS_BLOCKS), "turn-hooks-cap").await;
    let session_id = &meta.id;

    for continuation in 1..=MAX_CONTINUATIONS {
      let answer = checkpoint(&features, session_id, &meta, 0, true).await.unwrap();
      assert_eq!(answer, Checkpoint::Continue("gate: needs work".to_string()), "continuation {continuation}");
      append(&features, session_id, json!({"type": "continuation_ran", "id": format!("c{continuation}")})).await;
    }
    assert_eq!(checkpoint(&features, session_id, &meta, 0, true).await.unwrap(), Checkpoint::Finish);
    let turn = turn_events(&features, session_id);
    assert_eq!(count_of(&turn, TURN_HOOKS), MAX_CONTINUATIONS + 1);
    assert_eq!(count_of(&turn, AGENT_MESSAGE), MAX_CONTINUATIONS + 1);
  }

  #[tokio::test]
  async fn a_failing_check_is_shown_and_ends_the_turn_without_continuing() {
    let home = tempfile::tempdir().unwrap();
    let (features, meta) = checkpoint_session(home.path(), &gate_list("exit 3"), "turn-hooks-error").await;
    let session_id = &meta.id;

    assert_eq!(checkpoint(&features, session_id, &meta, 0, true).await.unwrap(), Checkpoint::Finish);
    let turn = turn_events(&features, session_id);
    let record = turn.iter().find(|event| event["type"] == TURN_HOOKS).unwrap();
    assert_eq!(record["results"][0]["result"], "error");
    let errors: Vec<&Value> = turn.iter().filter(|event| event["type"] == ERROR).collect();
    assert_eq!(errors.len(), 1);
    assert!(errors[0]["message"].as_str().unwrap().starts_with("Turn-end check gate failed: "), "{}", errors[0]);
  }

  #[tokio::test]
  async fn a_block_without_a_conversation_ends_the_turn_with_a_message() {
    let home = tempfile::tempdir().unwrap();
    let (features, meta) = checkpoint_session(home.path(), &gate_list(GATE_ALWAYS_BLOCKS), "turn-hooks-no-chat").await;
    let session_id = &meta.id;

    assert_eq!(checkpoint(&features, session_id, &meta, 0, false).await.unwrap(), Checkpoint::Finish);
    let turn = turn_events(&features, session_id);
    assert_eq!(count_of(&turn, TURN_HOOKS), 1);
    assert_eq!(count_of(&turn, AGENT_MESSAGE), 1);
    assert_eq!(count_of(&turn, ERROR), 1);
  }

  #[tokio::test]
  async fn each_block_of_a_web_session_is_shown_under_its_own_entry_name() {
    let home = tempfile::tempdir().unwrap();
    let hooks = json!([
      {"name": "test-gate", "command": GATE_ALWAYS_BLOCKS},
      {"name": "test-gate-two", "command": GATE_ALWAYS_BLOCKS},
      {"name": "web-status", "command": GATE_ALWAYS_BLOCKS},
    ])
    .to_string();
    let (features, meta) = checkpoint_session(home.path(), &hooks, "turn-hooks-sender").await;
    let session_id = &meta.id;

    assert!(matches!(checkpoint(&features, session_id, &meta, 0, true).await.unwrap(), Checkpoint::Continue(_)));
    let turn = turn_events(&features, session_id);
    let senders: Vec<&str> = turn
      .iter()
      .filter(|event| event["type"] == AGENT_MESSAGE)
      .map(|event| event["from_session_name"].as_str().unwrap())
      .collect();
    assert_eq!(senders, ["test-gate", "test-gate-two", "web-status"]);
  }

  /// A gate that blocks until the turn holds a continuation's event, then passes with the notice `needs a look`.
  const GATE_NOTICES_AFTER_CONTINUATION: &str = concat!(
    "grep -q continuation_ran && { printf '{\"systemMessage\":\"needs a look\"}'; exit 0; }; ",
    "printf '{\"decision\":\"block\",\"reason\":\"needs work\"}'",
  );

  #[tokio::test]
  async fn a_trigger_turn_notice_is_posted_once_under_the_entry_name() {
    let home = tempfile::tempdir().unwrap();
    let (features, meta) =
      checkpoint_session(home.path(), &gate_list(GATE_NOTICES_AFTER_CONTINUATION), "turn-hooks-trigger-notice").await;
    let session_id = &meta.id;
    append(&features, session_id, json!({"type": SCHEDULED_TRIGGER, "id": "t1", "content": "wake"})).await;

    assert!(matches!(checkpoint(&features, session_id, &meta, 0, true).await.unwrap(), Checkpoint::Continue(_)));
    append(&features, session_id, json!({"type": "continuation_ran", "id": "c1"})).await;
    assert_eq!(checkpoint(&features, session_id, &meta, 0, true).await.unwrap(), Checkpoint::Finish);
    // A restart before the turn ends replays the check: the turn already holds its notice, so none is posted again.
    assert_eq!(checkpoint(&features, session_id, &meta, 0, true).await.unwrap(), Checkpoint::Finish);

    let turn = turn_events(&features, session_id);
    assert_eq!(count_of(&turn, ATTACHED_NOTICE), 0);
    let notices: Vec<&Value> =
      turn.iter().filter(|event| event["type"] == AGENT_MESSAGE && event["content"] == "needs a look").collect();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0]["from_session_name"], "gate");
  }

  #[tokio::test]
  async fn a_user_message_turn_notice_attaches_through_the_contribution_and_posts_none() {
    let home = tempfile::tempdir().unwrap();
    let (features, meta) =
      checkpoint_session(home.path(), &gate_list(GATE_NOTICES_AFTER_CONTINUATION), "turn-hooks-user-notice").await;
    let session_id = &meta.id;
    append(&features, session_id, json!({"type": USER, "id": "u1", "content": "the question"})).await;

    assert!(matches!(checkpoint(&features, session_id, &meta, 0, true).await.unwrap(), Checkpoint::Continue(_)));
    append(&features, session_id, json!({"type": "continuation_ran", "id": "c1"})).await;
    assert_eq!(checkpoint(&features, session_id, &meta, 0, true).await.unwrap(), Checkpoint::Finish);

    let turn = turn_events(&features, session_id);
    assert_eq!(count_of(&turn, ATTACHED_NOTICE), 1);
    let attached = turn.iter().find(|event| event["type"] == ATTACHED_NOTICE).unwrap();
    assert_eq!(attached["content"], "needs a look");
    assert!(turn.iter().all(|event| event["type"] != AGENT_MESSAGE || event["content"] != "needs a look"));
  }
}
