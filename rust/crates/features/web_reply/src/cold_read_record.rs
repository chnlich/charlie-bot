//! The one record of a command-side cold read: the `cold_read_check` event and the mapping that keeps the legacy
//! `web_reply_check` and `web_ask_check` records readable.
//!
//! A recorded check is named by its row in `charliebot_tasks::cold_read::REGISTRY`: `reply`, `ask`, `settle` or
//! `remaining`. Every
//! writer writes one `cold_read_check` event carrying that name, and every reader asks
//! [`cold_read_record_name`] what a stored record was, so no log is migrated: a `web_reply_check` record reads as
//! `reply` and a `web_ask_check` record reads as `ask`. The HTTP route that writes the event lives in `api`, so this
//! module stays a leaf that `reply` and `status` can both read.

use charliebot_core::event_types::{COLD_READ_CHECK, WEB_ASK_CHECK, WEB_REPLY_CHECK};
use charliebot_registry::FeatureContext;
use charliebot_store::CompactValue;
use charliebot_tasks::cold_read;
use serde_json::{Map, Value, json};

/// The event fields the record-name mapping reads, so the JSON form and the store's compact form share one mapping.
pub trait RecordFields {
  fn record_type(&self) -> Option<&str>;
  fn record_name_field(&self) -> Option<&str>;
}

impl RecordFields for Value {
  fn record_type(&self) -> Option<&str> {
    self.get("type").and_then(Value::as_str)
  }

  fn record_name_field(&self) -> Option<&str> {
    self.get("name").and_then(Value::as_str)
  }
}

impl RecordFields for CompactValue {
  fn record_type(&self) -> Option<&str> {
    self.get("type").and_then(CompactValue::as_str)
  }

  fn record_name_field(&self) -> Option<&str> {
    self.get("name").and_then(CompactValue::as_str)
  }
}

/// The check a cold-read record belongs to: the event's own `name` for a `cold_read_check`, and the mapped name of a
/// legacy type. `None` for any other event.
pub fn cold_read_record_name<E: RecordFields + ?Sized>(event: &E) -> Option<&str> {
  match event.record_type()? {
    COLD_READ_CHECK => event.record_name_field(),
    WEB_REPLY_CHECK => Some(cold_read::REPLY),
    WEB_ASK_CHECK => Some(cold_read::ASK),
    _ => None,
  }
}

/// The `content` line of each recorded check's event.
fn content(name: &str) -> &'static str {
  match name {
    cold_read::REPLY => "web reply check",
    cold_read::ASK => "Cold read of a Waiting on you item",
    cold_read::SETTLE => "Cold read of the kept Waiting on you items",
    cold_read::REMAINING => "Cold read of the Remaining list",
    other => unreachable!("no cold-read record is named {other}"),
  }
}

/// One cold read to record: the check's name, the digest field and value it read, its backend and its answer, with the
/// reply's findings and run position when it read a reply.
pub struct CheckRecord<'a> {
  pub name: &'a str,
  pub backend: &'a str,
  pub answer: Option<&'a str>,
  pub digest_field: &'a str,
  pub digest: &'a str,
  pub findings: Option<&'a [String]>,
  pub run: Option<(&'a str, usize)>,
}

/// Record one `cold_read_check` event and name its check.
pub async fn record(features: &FeatureContext, session_id: &str, record: CheckRecord<'_>) -> anyhow::Result<()> {
  let mut event = Map::new();
  event.insert("type".to_string(), json!(COLD_READ_CHECK));
  event.insert("content".to_string(), json!(content(record.name)));
  event.insert("name".to_string(), json!(record.name));
  event.insert(record.digest_field.to_string(), json!(record.digest));
  event.insert("backend".to_string(), json!(record.backend));
  if let Some(answer) = record.answer {
    event.insert("answer".to_string(), json!(answer));
  }
  if let Some(findings) = record.findings {
    event.insert("findings".to_string(), json!(findings));
  }
  if let Some((run_id, reply_index)) = record.run {
    event.insert("run_id".to_string(), json!(run_id));
    event.insert("reply_index".to_string(), json!(reply_index));
  }
  features.store.persist_and_broadcast(session_id, Value::Object(event)).await?;
  tracing::info!(
    session = %session_id,
    name = record.name,
    backend = record.backend,
    "cold_read_check_recorded"
  );
  Ok(())
}

#[cfg(test)]
mod tests {
  use charliebot_core::event_types::COLD_READ_CHECK;
  use serde_json::json;

  use super::*;

  #[test]
  fn the_mapping_names_a_unified_record_and_both_legacy_types() {
    let unified = json!({"type": COLD_READ_CHECK, "name": cold_read::SETTLE});
    assert_eq!(cold_read_record_name(&unified), Some(cold_read::SETTLE));
    let remaining = json!({"type": COLD_READ_CHECK, "name": cold_read::REMAINING});
    assert_eq!(cold_read_record_name(&remaining), Some(cold_read::REMAINING));
    let reply = json!({"type": WEB_REPLY_CHECK, "reply_sha256": "d"});
    assert_eq!(cold_read_record_name(&reply), Some(cold_read::REPLY));
    let ask = json!({"type": WEB_ASK_CHECK, "item_sha256": "d"});
    assert_eq!(cold_read_record_name(&ask), Some(cold_read::ASK));
    assert_eq!(cold_read_record_name(&json!({"type": "web_status"})), None);
    assert_eq!(cold_read_record_name(&json!({"type": COLD_READ_CHECK})), None, "a unified record names its check");
  }

  #[test]
  fn the_mapping_reads_the_stores_compact_form_the_same_way() {
    let unified = CompactValue::from_value(&json!({"type": COLD_READ_CHECK, "name": cold_read::ASK}));
    assert_eq!(cold_read_record_name(&unified), Some(cold_read::ASK));
    let reply = CompactValue::from_value(&json!({"type": WEB_REPLY_CHECK}));
    assert_eq!(cold_read_record_name(&reply), Some(cold_read::REPLY));
  }
}
