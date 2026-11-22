//! The turn-end checks: the mechanical checks a turn passes before its `master_done` is written.
//!
//! The list is `hooks/turn_end.json` in the served repo: an array of check names. The turn owner's contribution answers
//! each name in process (`TurnContribution::run_turn_end_check`) from the shared `TurnInput`. A check passes, passes with
//! a notice, or blocks with a reason. A check that returns an error, or runs past `ENTRY_TIMEOUT`, is an `error`: it is
//! shown, and the turn ends without continuing. A name that no contribution answers fails the checkpoint.
//!
//! A checkpoint runs every name in list order and writes one `turn_hooks` event with all the results. Each block's
//! reason is shown in the session, and the same conversation continues with the reasons, at most `MAX_CONTINUATIONS`
//! times. Every step is derived from the session's event log, so a restart resumes the step a crash interrupted.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use charliebot_core::RepoRoot;
use charliebot_core::event_types::{
  AGENT_MESSAGE, CHILD_REPORT, CHILD_SESSION_ID, INPUT_EVENT_ID, MASTER_DONE, TURN_HOOKS, TURN_HOOKS_ID,
  WEB_REPLY_NOTICE, WEB_STATUS,
};
use charliebot_core::text_repr::field_text;
use charliebot_registry::FeatureContext;
use charliebot_registry::turn::{CheckResult, TurnContribution, TurnInput};
use charliebot_store::MessageProvenance;
use charliebot_store::metadata::{SessionMetadata, UtcDateTime};
use charliebot_store::reply_order::{self, Receipt};
use serde_json::{Value, json};

use crate::completion::TaskCompletion;

/// The file that holds the turn-end check list, relative to the served repo.
pub const HOOKS_FILE: &str = "hooks/turn_end.json";
/// The blocks that continue one turn. The block after them ends the turn with its reason shown.
pub const MAX_CONTINUATIONS: usize = 8;
/// The time limit of one check.
pub const ENTRY_TIMEOUT: Duration = Duration::from_secs(30);

/// One check's outcome after it answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
  Pass { notice: Option<String> },
  Block { reason: String },
  Error { message: String },
}

/// One check's name and outcome.
#[derive(Clone, Debug)]
pub struct Ran {
  pub name: String,
  pub outcome: Outcome,
}

/// Runs every name of the list in order against `input`. The turn owner answers the names it lists, in process, each
/// within `timeout`. A name the owner does not list passes: no check of this turn's owner answers it.
pub async fn run_checks(
  names: &[String],
  owner: Option<&Arc<dyn TurnContribution>>,
  features: &Arc<FeatureContext>,
  input: &TurnInput,
  timeout: Duration,
) -> Vec<Ran> {
  let mut ran = Vec::with_capacity(names.len());
  for name in names {
    let outcome = match owner {
      Some(contribution) if contribution.turn_end_check_names().contains(&name.as_str()) => {
        run_check(contribution.as_ref(), name, features, input, timeout).await
      }
      _ => Outcome::Pass { notice: None },
    };
    ran.push(Ran { name: name.clone(), outcome });
  }
  ran
}

async fn run_check(
  contribution: &dyn TurnContribution,
  name: &str,
  features: &Arc<FeatureContext>,
  input: &TurnInput,
  timeout: Duration,
) -> Outcome {
  match tokio::time::timeout(timeout, contribution.run_turn_end_check(name, features, input)).await {
    Ok(Ok(CheckResult::Pass)) => Outcome::Pass { notice: None },
    Ok(Ok(CheckResult::Notice(message))) => Outcome::Pass { notice: Some(message) },
    Ok(Ok(CheckResult::Block(reason))) => Outcome::Block { reason },
    Ok(Err(error)) => Outcome::Error { message: format!("{error:#}") },
    Err(_) => Outcome::Error { message: format!("timed out after {} s", timeout.as_secs()) },
  }
}

/// The turn-end check list of the served repo: the names of the checks, in the order they run. A missing or malformed
/// list fails the checkpoint.
pub fn load_check_names(repo: &RepoRoot) -> anyhow::Result<Vec<String>> {
  let path = repo.path().join(HOOKS_FILE);
  let text =
    std::fs::read_to_string(&path).with_context(|| format!("read the turn-end check list {}", path.display()))?;
  serde_json::from_str(&text).with_context(|| format!("parse the turn-end check list {}", path.display()))
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

/// The index of the turn's last input that the running run received: the last event with a `received_at` stamp.
pub fn last_received_index(turn: &[Value]) -> Option<usize> {
  turn.iter().rposition(|event| event["received_at"].is_string())
}

/// The provenance fragment of the turn's last received input when that input is a child report, else `None`. The
/// dispatcher answers it as the prompt builder does, so the reminder quotes the report as the prompt renders it.
async fn last_input_provenance(features: &FeatureContext, turn: &[Value]) -> anyhow::Result<Option<String>> {
  let Some(index) = last_received_index(turn) else { return Ok(None) };
  let input = &turn[index];
  if input["type"] != CHILD_REPORT {
    return Ok(None);
  }
  let child_id = field_text(input.get(CHILD_SESSION_ID));
  Ok(crate::dispatch::dispatcher(features)?.child_report_provenance(&child_id).await?)
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

/// Delivers a passed check's notice. The session's turn contribution attaches it to an unanswered user message of the
/// turn when there is one. Otherwise the runner records it as a `web_reply_notice` event of the session: that event
/// admits no input, so the notice starts no turn. A session with a parent also sends the notice to the parent, which
/// reads it in its own turn. A turn that already holds the runner's notice records no second one, and the parent's
/// input is keyed by the recorded notice's id, so a replayed checkpoint adds nothing.
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
  let notice_id = match recorded_notice_id(turn, message) {
    Some(id) => id.to_string(),
    None => {
      let event = json!({"type": WEB_REPLY_NOTICE, "content": message});
      let persisted = features.store.persist_and_broadcast(session_id, event).await?;
      persisted["id"].as_str().context("the persisted notice carries its id")?.to_string()
    }
  };
  features.store.mark_unread(session_id).await?;
  let completion = features.startup.require::<TaskCompletion>()?;
  completion.notify_initiator_of_turn_notice(session_id, &notice_id, name, message).await?;
  Ok(())
}

/// The id of the turn's runner notice `message`: a `web_reply_notice` that links no user message. A notice attached to
/// a user message links its input and is not this one.
fn recorded_notice_id<'a>(turn: &'a [Value], message: &str) -> Option<&'a str> {
  turn
    .iter()
    .find(|event| {
      event["type"] == WEB_REPLY_NOTICE
        && event["content"] == message
        && event[WEB_REPLY_NOTICE][INPUT_EVENT_ID].is_null()
    })
    .and_then(|event| event["id"].as_str())
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
  let names = load_check_names(&checkout)?;
  if let Some(name) = names.iter().find(|name| !features.registry.answers_turn_end_check(name)) {
    anyhow::bail!("turn-end check {name} in {HOOKS_FILE} is answered by no registered contribution");
  }
  let input = TurnInput {
    session_id: session_id.to_string(),
    kind: owner.as_ref().map(|(kind, _)| (*kind).to_string()),
    exit_code,
    // Past the pending check, a block in this turn has run its continuation, and this checkpoint judges that run.
    continuation_failed: exit_code != 0 && log.turn.iter().any(is_block_record),
    status: log.status.clone(),
    last_input_provenance: last_input_provenance(features, &log.turn).await?,
    turn: log.turn.clone(),
  };
  let contribution = owner.as_ref().map(|(_, contribution)| contribution);
  let mut ran = run_checks(&names, contribution, features, &input, ENTRY_TIMEOUT).await;
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
  // The blocks show their reasons before an error ends the turn, so a block that an error follows is not lost.
  show_reasons(features, session_id, &record_value, 0).await?;

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
  use charliebot_core::event_types::{ERROR, FROM_SESSION, SCHEDULED_TRIGGER, USER};
  use std::path::Path;

  use charliebot_registry::sequence::SequenceCompletion;
  use charliebot_registry::{Registry, RegistryError};
  use charliebot_store::ChatEvent;
  use futures_util::future::BoxFuture;

  use super::*;
  use crate::dispatch::{Dispatcher, RunFinishedCompletion};

  fn input(turn: Vec<Value>) -> TurnInput {
    TurnInput {
      session_id: "synthetic-session".to_string(),
      kind: Some("web".to_string()),
      exit_code: 0,
      continuation_failed: false,
      status: Value::Null,
      last_input_provenance: None,
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

  /// The check list that names `names` in order.
  fn hooks(names: &[&str]) -> String {
    serde_json::to_string(names).unwrap()
  }

  /// The names the test contribution answers. Each answers from the turn's events, as the web checks do.
  const TEST_CHECKS: [&str; 8] = [
    "pass-after-continuation",
    "always-blocks",
    "always-blocks-two",
    "web-status",
    "notice-after-failed-continuation",
    "notice-after-continuation",
    "fails",
    "hangs",
  ];

  /// The event that `AttachingTurns` persists for each notice it attaches.
  const ATTACHED_NOTICE: &str = "test_attached_notice";

  /// The test checks' answers. A block carries the reason `needs work`.
  async fn answer(name: &str, input: &TurnInput) -> anyhow::Result<CheckResult> {
    let block = CheckResult::Block("needs work".to_string());
    let continued = input.turn.iter().any(|event| event["type"] == "continuation_ran");
    match name {
      // Blocks until the turn holds a continuation's event, then passes.
      "pass-after-continuation" => Ok(if continued { CheckResult::Pass } else { block }),
      "always-blocks" | "always-blocks-two" | "web-status" => Ok(block),
      // Blocks, and on a failed continuation answers its second miss with a notice.
      "notice-after-failed-continuation" => {
        Ok(if input.continuation_failed { CheckResult::Notice("failed continuation".to_string()) } else { block })
      }
      // Blocks until the turn holds a continuation's event, then passes with a notice.
      "notice-after-continuation" => {
        Ok(if continued { CheckResult::Notice("needs a look".to_string()) } else { block })
      }
      "fails" => anyhow::bail!("the check failed"),
      "hangs" => {
        tokio::time::sleep(Duration::from_secs(30)).await;
        Ok(CheckResult::Pass)
      }
      other => anyhow::bail!("no test check named {other}"),
    }
  }

  /// A turn contribution that owns every manager session's turn-end checks, as the web contribution does for web
  /// sessions. It attaches a notice when the turn holds a user message, and has no message to attach to otherwise.
  struct AttachingTurns;

  impl TurnContribution for AttachingTurns {
    fn turn_kind(&self, _meta: &SessionMetadata) -> Option<&'static str> {
      Some("web")
    }

    fn turn_end_check_names(&self) -> &'static [&'static str] {
      &TEST_CHECKS
    }

    fn run_turn_end_check<'a>(
      &'a self,
      name: &'a str,
      _features: &'a Arc<FeatureContext>,
      input: &'a TurnInput,
    ) -> BoxFuture<'a, anyhow::Result<CheckResult>> {
      Box::pin(async move { answer(name, input).await })
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
    install_dispatch(&features);
    std::fs::write(home.join(HOOKS_FILE), hooks).unwrap();
    let meta = crate::test_fixtures::manager_session(&features, request_id, "fable", None, "cc-synthetic").await;
    (features, meta)
  }

  /// The completion and the dispatcher that a notice reaches a parent through, installed as recovery installs them.
  fn install_dispatch(features: &Arc<FeatureContext>) {
    features.startup.insert(TaskCompletion::new(Arc::clone(features))).unwrap();
    let completion = features.startup.require::<TaskCompletion>().unwrap();
    let sequence: Arc<dyn SequenceCompletion> = completion.clone();
    let run: Arc<dyn RunFinishedCompletion> = completion;
    features.startup.insert(Dispatcher::new(Arc::clone(&features.store), Arc::clone(features), sequence, run)).unwrap();
  }

  /// The session's pending inputs as the dispatcher reads them.
  fn pending_events(features: &FeatureContext, session_id: &str) -> Vec<Value> {
    let inputs: Vec<ChatEvent> = crate::dispatch::dispatcher(features).unwrap().pending_inputs(session_id).unwrap();
    inputs.into_iter().map(|event| event.to_value()).collect()
  }

  fn pending_ids(features: &FeatureContext, session_id: &str) -> Vec<String> {
    pending_events(features, session_id).iter().map(|event| event["id"].as_str().unwrap().to_string()).collect()
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
  async fn a_check_past_its_time_limit_is_an_error() {
    let home = tempfile::tempdir().unwrap();
    let (features, _meta) = checkpoint_session(home.path(), &hooks(&["hangs"]), "turn-hooks-timeout").await;
    let owner: Arc<dyn TurnContribution> = Arc::new(AttachingTurns);

    let ran =
      run_checks(&["hangs".to_string()], Some(&owner), &features, &input(vec![]), Duration::from_millis(20)).await;

    assert!(matches!(&ran[0].outcome, Outcome::Error { message } if message.starts_with("timed out")), "{:?}", ran[0]);
  }

  #[tokio::test]
  async fn a_list_name_no_contribution_answers_fails_the_checkpoint() {
    let home = tempfile::tempdir().unwrap();
    let (features, meta) = checkpoint_session(home.path(), &hooks(&["no-such-check"]), "turn-hooks-unknown").await;

    let error = checkpoint(&features, &meta.id, &meta, 0, true).await.unwrap_err();
    assert!(format!("{error:#}").contains("no-such-check"), "{error:#}");
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

  #[tokio::test]
  async fn a_block_replays_without_a_second_record_until_its_continuation_runs_then_passes() {
    let home = tempfile::tempdir().unwrap();
    let (features, meta) =
      checkpoint_session(home.path(), &hooks(&["pass-after-continuation"]), "turn-hooks-pass").await;
    let session_id = &meta.id;

    let first = checkpoint(&features, session_id, &meta, 0, true).await.unwrap();
    assert_eq!(first, Checkpoint::Continue("pass-after-continuation: needs work".to_string()));

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
      checkpoint_session(home.path(), &hooks(&["notice-after-failed-continuation"]), "turn-hooks-failed").await;
    let session_id = &meta.id;

    let first = checkpoint(&features, session_id, &meta, 0, true).await.unwrap();
    assert_eq!(first, Checkpoint::Continue("notice-after-failed-continuation: needs work".to_string()));
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
    let (features, meta) = checkpoint_session(home.path(), &hooks(&["always-blocks"]), "turn-hooks-cap").await;
    let session_id = &meta.id;

    for continuation in 1..=MAX_CONTINUATIONS {
      let answer = checkpoint(&features, session_id, &meta, 0, true).await.unwrap();
      assert_eq!(answer, Checkpoint::Continue("always-blocks: needs work".to_string()), "continuation {continuation}");
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
    let (features, meta) = checkpoint_session(home.path(), &hooks(&["fails"]), "turn-hooks-error").await;
    let session_id = &meta.id;

    assert_eq!(checkpoint(&features, session_id, &meta, 0, true).await.unwrap(), Checkpoint::Finish);
    let turn = turn_events(&features, session_id);
    let record = turn.iter().find(|event| event["type"] == TURN_HOOKS).unwrap();
    assert_eq!(record["results"][0]["result"], "error");
    let errors: Vec<&Value> = turn.iter().filter(|event| event["type"] == ERROR).collect();
    assert_eq!(errors.len(), 1);
    assert!(errors[0]["message"].as_str().unwrap().starts_with("Turn-end check fails failed: "), "{}", errors[0]);
  }

  #[tokio::test]
  async fn a_block_reason_shows_before_a_later_entry_errors_and_ends_the_turn() {
    let home = tempfile::tempdir().unwrap();
    let (features, meta) =
      checkpoint_session(home.path(), &hooks(&["always-blocks", "fails"]), "turn-hooks-block-then-error").await;
    let session_id = &meta.id;

    assert_eq!(checkpoint(&features, session_id, &meta, 0, true).await.unwrap(), Checkpoint::Finish);
    let turn = turn_events(&features, session_id);
    let reason = turn
      .iter()
      .position(|event| {
        event["type"] == AGENT_MESSAGE
          && event["from_session_name"] == "always-blocks"
          && event["content"] == "needs work"
      })
      .expect("the block's reason is shown to the agent");
    let error = turn.iter().position(|event| event["type"] == ERROR).expect("the failing check's error is shown");
    assert!(reason < error, "the reason shows before the error that ends the turn");
    assert_eq!(count_of(&turn, TURN_HOOKS), 1);
  }

  #[tokio::test]
  async fn a_block_without_a_conversation_ends_the_turn_with_a_message() {
    let home = tempfile::tempdir().unwrap();
    let (features, meta) = checkpoint_session(home.path(), &hooks(&["always-blocks"]), "turn-hooks-no-chat").await;
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
    let (features, meta) = checkpoint_session(
      home.path(),
      &hooks(&["always-blocks", "always-blocks-two", "web-status"]),
      "turn-hooks-sender",
    )
    .await;
    let session_id = &meta.id;

    assert!(matches!(checkpoint(&features, session_id, &meta, 0, true).await.unwrap(), Checkpoint::Continue(_)));
    let turn = turn_events(&features, session_id);
    let senders: Vec<&str> = turn
      .iter()
      .filter(|event| event["type"] == AGENT_MESSAGE)
      .map(|event| event["from_session_name"].as_str().unwrap())
      .collect();
    assert_eq!(senders, ["always-blocks", "always-blocks-two", "web-status"]);
  }

  #[tokio::test]
  async fn a_trigger_turn_notice_is_recorded_once_and_admits_no_input() {
    let home = tempfile::tempdir().unwrap();
    let (features, meta) =
      checkpoint_session(home.path(), &hooks(&["notice-after-continuation"]), "turn-hooks-trigger-notice").await;
    let session_id = &meta.id;
    append(&features, session_id, json!({"type": SCHEDULED_TRIGGER, "id": "t1", "content": "wake"})).await;
    let pending_before = pending_ids(&features, session_id);

    assert!(matches!(checkpoint(&features, session_id, &meta, 0, true).await.unwrap(), Checkpoint::Continue(_)));
    append(&features, session_id, json!({"type": "continuation_ran", "id": "c1"})).await;
    assert_eq!(checkpoint(&features, session_id, &meta, 0, true).await.unwrap(), Checkpoint::Finish);
    // A restart before the turn ends replays the check: the turn already holds its notice, so none is recorded again.
    assert_eq!(checkpoint(&features, session_id, &meta, 0, true).await.unwrap(), Checkpoint::Finish);

    let turn = turn_events(&features, session_id);
    assert_eq!(count_of(&turn, ATTACHED_NOTICE), 0);
    let notices: Vec<&Value> = turn.iter().filter(|event| event["content"] == "needs a look").collect();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0]["type"], WEB_REPLY_NOTICE);
    assert!(notices[0][WEB_REPLY_NOTICE][INPUT_EVENT_ID].is_null(), "no user message is linked");
    // The notice admits no input, so the session holds the inputs it held before, and no further turn starts.
    assert_eq!(pending_ids(&features, session_id), pending_before);
  }

  #[tokio::test]
  async fn a_turn_notice_propagates_up_three_levels_without_waking_its_source() {
    let home = tempfile::tempdir().unwrap();
    let (features, grandparent) =
      checkpoint_session(home.path(), &hooks(&["notice-after-continuation"]), "turn-hooks-parent").await;
    let parent =
      crate::test_fixtures::child_manager_session(&features, &grandparent.id, "turn-hooks-parent-child", "cc-parent")
        .await;
    let child =
      crate::test_fixtures::child_manager_session(&features, &parent.id, "turn-hooks-child", "cc-child").await;
    let session_id = &child.id;
    append(&features, session_id, json!({"type": SCHEDULED_TRIGGER, "id": "t1", "content": "wake"})).await;
    let child_pending = pending_ids(&features, session_id);

    assert!(matches!(checkpoint(&features, session_id, &child, 0, true).await.unwrap(), Checkpoint::Continue(_)));
    append(&features, session_id, json!({"type": "continuation_ran", "id": "c1"})).await;
    assert_eq!(checkpoint(&features, session_id, &child, 0, true).await.unwrap(), Checkpoint::Finish);
    // A restart before the turn ends replays the check: the child keeps one notice and the parent one input.
    assert_eq!(checkpoint(&features, session_id, &child, 0, true).await.unwrap(), Checkpoint::Finish);

    assert_eq!(pending_ids(&features, session_id), child_pending, "the child's notice starts no child turn");
    let from_child: Vec<Value> = pending_events(&features, &parent.id)
      .into_iter()
      .filter(|event| event["type"] == AGENT_MESSAGE && event[FROM_SESSION] == child.id)
      .collect();
    assert_eq!(from_child.len(), 1, "the parent holds the notice once");
    assert!(from_child[0]["content"].as_str().unwrap().contains("needs a look"), "{}", from_child[0]);

    // Run the parent's checkpoint against the child's input; its own second miss sends one notice to its parent.
    let parent_pending = pending_ids(&features, &parent.id);
    assert!(matches!(checkpoint(&features, &parent.id, &parent, 0, true).await.unwrap(), Checkpoint::Continue(_)));
    append(&features, &parent.id, json!({"type": "continuation_ran", "id": "parent-c1"})).await;
    assert_eq!(checkpoint(&features, &parent.id, &parent, 0, true).await.unwrap(), Checkpoint::Finish);
    assert_eq!(checkpoint(&features, &parent.id, &parent, 0, true).await.unwrap(), Checkpoint::Finish);

    let parent_turn = turn_events(&features, &parent.id);
    let parent_notices: Vec<&Value> = parent_turn
      .iter()
      .filter(|event| event["type"] == WEB_REPLY_NOTICE && event["content"] == "needs a look")
      .collect();
    assert_eq!(parent_notices.len(), 1, "the parent records its own notice once");
    assert!(parent_notices[0][WEB_REPLY_NOTICE][INPUT_EVENT_ID].is_null(), "the notice does not link an input");
    assert_eq!(pending_ids(&features, &parent.id), parent_pending, "the parent's notice starts no parent turn");
    assert_eq!(pending_ids(&features, session_id), child_pending, "the child's notice starts no child turn");

    let from_parent: Vec<Value> = pending_events(&features, &grandparent.id)
      .into_iter()
      .filter(|event| event["type"] == AGENT_MESSAGE && event[FROM_SESSION] == parent.id)
      .collect();
    assert_eq!(from_parent.len(), 1, "the grandparent holds the parent's notice once");
    assert!(from_parent[0]["content"].as_str().unwrap().contains("needs a look"), "{}", from_parent[0]);
  }

  #[tokio::test]
  async fn a_user_message_turn_notice_attaches_through_the_contribution_and_posts_none() {
    let home = tempfile::tempdir().unwrap();
    let (features, meta) =
      checkpoint_session(home.path(), &hooks(&["notice-after-continuation"]), "turn-hooks-user-notice").await;
    let session_id = &meta.id;
    append(&features, session_id, json!({"type": USER, "id": "u1", "content": "the question"})).await;

    assert!(matches!(checkpoint(&features, session_id, &meta, 0, true).await.unwrap(), Checkpoint::Continue(_)));
    append(&features, session_id, json!({"type": "continuation_ran", "id": "c1"})).await;
    assert_eq!(checkpoint(&features, session_id, &meta, 0, true).await.unwrap(), Checkpoint::Finish);

    let turn = turn_events(&features, session_id);
    assert_eq!(count_of(&turn, ATTACHED_NOTICE), 1);
    let attached = turn.iter().find(|event| event["type"] == ATTACHED_NOTICE).unwrap();
    assert_eq!(attached["content"], "needs a look");
    assert!(turn.iter().all(|event| event["content"] != "needs a look" || event["type"] == ATTACHED_NOTICE));
  }
}
