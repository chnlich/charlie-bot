//! The order rule for answered user messages: which inputs a turn received, and whether a reply follows each receipt.
//!
//! A turn's received inputs are the input ids on its run record. An input's receipt time is the time of its
//! `input_delivered` row under that run, or the run's start time when the turn received it as a start input. A reply
//! answers a received input when the reply's log time is later than its receipt time. The rule reads only records the
//! server already wrote, so a turn that a restart resumed is judged like a turn no restart interrupted.
//!
//! Every reader that asks whether a user message was answered, or whether a message was ever received, calls this
//! module. The web reply package, the chat-thread core, and the Discord and Feishu reads are the callers.

use std::borrow::Borrow;
use std::collections::{HashMap, HashSet};

use charliebot_core::event_types::{EventFields, INPUT_DELIVERED, INPUT_EVENT_ID, MASTER_DONE};

use crate::event_types::ChatEvent;
use crate::message_utils::master_done_input_event_ids;
use crate::metadata::UtcDateTime;
use crate::run_records::{RunKind, RunRecord};
use crate::{CompactValue, Store, StoreError, StoreResult};

/// One input a turn received, with the moment it was received.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Receipt {
  /// The id of the chat event the turn received.
  pub input_event_id: String,
  /// The run that received the input.
  pub run_id: String,
  /// The moment the run received the input.
  pub at: UtcDateTime,
}

/// The inputs `run` received, in the order its record claims them: each of its `input_event_ids`, timed by its
/// `input_delivered` row under this run, or by the run's start time when no row exists.
///
/// The `input_delivered` row of another run never times this run's input.
pub fn receipts<E: Borrow<ChatEvent>>(run: &RunRecord, events: &[E]) -> StoreResult<Vec<Receipt>> {
  let mut receipts = Vec::with_capacity(run.input_event_ids.len());
  for input_event_id in &run.input_event_ids {
    let delivered_at = delivery_time(run, input_event_id, events)?;
    let at = match delivered_at {
      Some(at) => at,
      None => run.started_at.ok_or_else(|| {
        StoreError::new(format!(
          "run {} received input {input_event_id} but records neither a delivery time nor a start time",
          run.id
        ))
      })?,
    };
    receipts.push(Receipt { input_event_id: input_event_id.clone(), run_id: run.id.clone(), at });
  }
  Ok(receipts)
}

/// The time of `run`'s `input_delivered` row for `input_event_id`, when the log holds one.
fn delivery_time<E: Borrow<ChatEvent>>(
  run: &RunRecord,
  input_event_id: &str,
  events: &[E],
) -> StoreResult<Option<UtcDateTime>> {
  for event in events {
    let event: &ChatEvent = event.borrow();
    if event_type(event) != Some(INPUT_DELIVERED) {
      continue;
    }
    if event.get(INPUT_EVENT_ID).and_then(CompactValue::as_str) != Some(input_event_id) {
      continue;
    }
    if event.get("run_id").and_then(CompactValue::as_str) != Some(run.id.as_str()) {
      continue;
    }
    return Ok(Some(event_time(event, INPUT_DELIVERED)?));
  }
  Ok(None)
}

/// Whether the log holds a `reply_type` event later than `receipt`: the order rule itself. A reply at the receipt's
/// own instant does not follow it.
pub fn answered<E: Borrow<ChatEvent>>(receipt: &Receipt, events: &[E], reply_type: &str) -> StoreResult<bool> {
  for event in events {
    let event: &ChatEvent = event.borrow();
    if event_type(event) != Some(reply_type) {
      continue;
    }
    if event_time(event, reply_type)? > receipt.at {
      return Ok(true);
    }
  }
  Ok(false)
}

/// The receipts of the turn that `done` ends. `done` names its run, and a `master_done` without a run id is an error.
pub fn turn_receipts<D: EventFields, E: Borrow<ChatEvent>>(
  store: &Store,
  session_id: &str,
  done: &D,
  events: &[E],
) -> StoreResult<Vec<Receipt>> {
  let run_id = done
    .field("run_id")
    .and_then(EventFields::as_str_field)
    .filter(|run_id| !run_id.is_empty())
    .ok_or_else(|| StoreError::new("the master_done event carries no run_id; its turn's receipts are unknown"))?;
  let run = store
    .read_run_sync(session_id, run_id)?
    .ok_or_else(|| StoreError::not_found(format!("run {run_id} not found in task {session_id}")))?;
  receipts(&run, events)
}

/// The receipts of the session's running manager turn: the newest manager run whose record has no end time, in the run
/// records' canonical order. A session with no such run has no running turn, and its receipts are empty.
pub fn running_receipts<E: Borrow<ChatEvent>>(
  store: &Store,
  session_id: &str,
  events: &[E],
) -> StoreResult<Vec<Receipt>> {
  let running = store
    .list_run_records_sync(session_id)?
    .into_iter()
    .rfind(|run| run.kind == RunKind::ManagerTurn && run.ended_at.is_none());
  match running {
    Some(run) => receipts(&run, events),
    None => Ok(Vec::new()),
  }
}

/// The input ids that the session's finished turns received: each `master_done`'s input ids, united with the
/// receipts of the run it names. A `master_done` written before the run id existed contributes its recorded input ids
/// alone.
pub fn received_by_finished_turns<E: Borrow<ChatEvent>>(
  store: &Store,
  session_id: &str,
  events: &[E],
) -> StoreResult<HashSet<String>> {
  let runs: HashMap<String, RunRecord> =
    store.list_run_records_sync(session_id)?.into_iter().map(|run| (run.id.clone(), run)).collect();
  let mut received = HashSet::new();
  for event in events {
    let event: &ChatEvent = event.borrow();
    if event_type(event) != Some(MASTER_DONE) {
      continue;
    }
    received.extend(master_done_input_event_ids(event));
    let Some(run_id) = event.get("run_id").and_then(CompactValue::as_str).filter(|run_id| !run_id.is_empty()) else {
      continue;
    };
    let run =
      runs.get(run_id).ok_or_else(|| StoreError::not_found(format!("run {run_id} not found in task {session_id}")))?;
    received.extend(receipts(run, events)?.into_iter().map(|receipt| receipt.input_event_id));
  }
  Ok(received)
}

fn event_type(event: &ChatEvent) -> Option<&str> {
  event.get("type").and_then(CompactValue::as_str)
}

/// The event's log time, parsed; a missing or unparsable time is an error naming the event kind.
fn event_time(event: &ChatEvent, kind: &str) -> StoreResult<UtcDateTime> {
  let timestamp = event
    .get("timestamp")
    .and_then(CompactValue::as_str)
    .ok_or_else(|| StoreError::new(format!("a {kind} event carries no timestamp")))?;
  UtcDateTime::parse(timestamp).map_err(|error| StoreError::new(format!("a {kind} event has a bad timestamp: {error}")))
}

#[cfg(test)]
mod tests {
  use serde_json::{Value, json};

  use super::*;

  fn event(value: Value) -> ChatEvent {
    ChatEvent::from_value(&value)
  }

  fn run(input_event_ids: &[&str], started_at: &str) -> RunRecord {
    serde_json::from_value(json!({
      "id": "run-1",
      "session_id": "session",
      "kind": "manager_turn",
      "input_event_ids": input_event_ids,
      "started_at": started_at,
    }))
    .expect("the run record decodes")
  }

  fn delivered(input_event_id: &str, run_id: &str, timestamp: &str) -> ChatEvent {
    event(json!({
      "type": INPUT_DELIVERED,
      "id": format!("delivered-{input_event_id}"),
      "input_event_id": input_event_id,
      "run_id": run_id,
      "timestamp": timestamp,
    }))
  }

  #[test]
  fn a_start_input_is_received_at_the_run_start_and_a_delivered_input_at_its_delivery() {
    let run = run(&["start-input", "delivered-input"], "2026-11-02T08:20:54.838607Z");
    let events = [
      delivered("delivered-input", "run-1", "2026-11-02T08:22:23.103374+00:00"),
      delivered("start-input", "other-run", "2026-11-02T08:30:00Z"),
    ];

    let receipts = receipts(&run, &events).expect("the receipts read");

    assert_eq!(receipts.len(), 2);
    assert_eq!(receipts[0].input_event_id, "start-input");
    assert_eq!(receipts[0].at, UtcDateTime::parse("2026-11-02T08:20:54.838607Z").unwrap());
    assert_eq!(receipts[1].input_event_id, "delivered-input");
    assert_eq!(receipts[1].at, UtcDateTime::parse("2026-11-02T08:22:23.103374Z").unwrap());
    assert_eq!(receipts[1].run_id, "run-1");
  }

  #[test]
  fn a_reply_before_the_receipt_does_not_answer_it_and_one_after_does() {
    let receipt = Receipt {
      input_event_id: "input-1".to_string(),
      run_id: "run-1".to_string(),
      at: UtcDateTime::parse("2026-11-02T08:22:00Z").unwrap(),
    };
    let before = [event(json!({"type": "web_reply", "timestamp": "2026-11-02T08:21:59Z"}))];
    let after = [event(json!({"type": "web_reply", "timestamp": "2026-11-02T08:22:01Z"}))];

    assert!(!answered(&receipt, &before, "web_reply").expect("the order rule reads"));
    assert!(answered(&receipt, &after, "web_reply").expect("the order rule reads"));
  }

  #[test]
  fn another_reply_type_does_not_answer_a_web_receipt() {
    let receipt = Receipt {
      input_event_id: "input-1".to_string(),
      run_id: "run-1".to_string(),
      at: UtcDateTime::parse("2026-11-02T08:22:00Z").unwrap(),
    };
    let events = [event(json!({"type": "slack_reply", "timestamp": "2026-11-02T08:23:00Z"}))];

    assert!(!answered(&receipt, &events, "web_reply").expect("the order rule reads"));
  }

  #[test]
  fn a_finished_turn_received_the_inputs_of_its_master_done_and_of_its_run() {
    let (_home, store) = crate::store::test_support::make_test_store();
    let session_id = "session";
    let path = store.run_metadata_path(session_id, "resumed-run");
    std::fs::create_dir_all(path.parent().expect("run directory")).expect("run directory");
    std::fs::write(
      &path,
      json!({
        "id": "resumed-run",
        "session_id": session_id,
        "kind": "manager_turn",
        "input_event_ids": ["resumed-input"],
        "started_at": "2026-11-02T08:20:54.838607Z",
      })
      .to_string(),
    )
    .expect("run record");
    // A resumed turn loses its master_done input ids; the run record still names what it received.
    let events = [event(json!({"type": MASTER_DONE, "run_id": "resumed-run", "input_event_ids": []}))];

    let received = received_by_finished_turns(&store, session_id, &events).expect("the received ids read");
    assert_eq!(received, HashSet::from(["resumed-input".to_string()]));

    // A master_done written before the run id existed contributes its recorded input ids alone.
    let legacy = [event(json!({"type": MASTER_DONE, "input_event_ids": ["legacy-input"]}))];
    let received = received_by_finished_turns(&store, session_id, &legacy).expect("the legacy ids read");
    assert_eq!(received, HashSet::from(["legacy-input".to_string()]));
  }
}
