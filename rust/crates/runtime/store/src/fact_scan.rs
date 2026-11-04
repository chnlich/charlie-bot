//! The boot walk's fact scan of a live event log: the node and run facts a full read and fold of the log gives, read
//! without building the log's chat content.
//!
//! Chat content is nearly all of a long log's bytes, and the facts fold reads none of it. The scan parses each line
//! with serde_json's own grammar, so it keeps and drops exactly the lines a full read keeps and drops. A line of a
//! chat-content type ([`CHAT_CONTENT_TYPES`], and a `user` line whose `content` is not a string) becomes a stand-in
//! event that holds only the line's string `id` and `type`; every other line is built in full. The fold reads nothing
//! but `id` and `type` from a chat-content line, so the stand-in list folds to the full list's facts, and each event
//! keeps its position.
//!
//! [`Store::seal_scanned_facts`] memoizes those facts under the log's file identity, as a recovery eviction seals a
//! fold, so a node pass reads them without loading the log. A reader that needs the chat content loads the log, as any
//! cold read does. The node and run facts and the newest `prompt_changed` fact of each scope answer from the seal.
//!
//! A session with archived events scans its archive files first, with the same line rule, and folds them ahead of the
//! live log. The seal then also keys on the session's `archive_offset`, the count of archived events it folded.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use charliebot_core::event_types;
use serde::de::{DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};

use crate::compact::{CompactValue, KeySource, ObjectEntry, ParseKeys};
use crate::event_types::{FileIdentity, SharedChatEvent};
use crate::events::WHOLE_LOG_LOGGED_BYTES;
use crate::ndjson;
use crate::run_facts::RunFacts;
use crate::task_tree::{PromptScope, fold_scanned_history, newest_prompt_change};
use crate::task_tree_state::{
  CachedNodeFacts, CachedNodeState, CachedRunFactsAt, LivePromptChanges, NodeFacts, NodeFactsKey, NodeStateKey,
  SealedPromptChanges,
};
use crate::{Store, StoreResult};

/// The event types that carry chat content: the facts fold reads only their `id` and `type`.
const CHAT_CONTENT_TYPES: [&str; 9] = [
  event_types::ASSISTANT,
  event_types::SYSTEM,
  event_types::RESULT,
  event_types::TOOL_USE,
  event_types::TOOL_RESULT,
  event_types::THINKING,
  event_types::WEB_STATUS,
  event_types::RATE_LIMIT_EVENT,
  event_types::MASTER_DONE,
];

/// A live log's facts, scanned outside the session lock for a later [`Store::seal_scanned_facts`].
pub struct ScannedLiveFacts {
  identity: FileIdentity,
  /// The archived events the scan folded before the live log, the session's `archive_offset`.
  archived: usize,
  covered: usize,
  facts: NodeFacts,
  runs: RunFacts,
  prompt_changes: LivePromptChanges,
}

impl LivePromptChanges {
  fn of(events: &[SharedChatEvent]) -> Self {
    Self {
      subtree: newest_prompt_change(events, PromptScope::Subtree),
      node: newest_prompt_change(events, PromptScope::Node),
    }
  }

  pub(crate) fn newest(&self, scope: PromptScope) -> Option<SharedChatEvent> {
    match scope {
      PromptScope::Subtree => self.subtree.clone(),
      PromptScope::Node => self.node.clone(),
    }
  }
}

impl ScannedLiveFacts {
  /// The sessions that the scanned log's close events report to, in log order. A log whose task facts failed to fold
  /// names none: its node's own read meets that error.
  pub fn report_recipients(&self) -> impl Iterator<Item = &str> {
    let close_events = self.facts.task().map_or(&[][..], |task| task.close_events.as_slice());
    close_events
      .iter()
      .filter_map(|close| close.get("report_to").and_then(CompactValue::as_str))
      .filter(|recipient| !recipient.is_empty())
  }
}

impl Store {
  /// Scan and fold a session's whole history outside its session lock: its archived events, then its live log. `None`
  /// when the archive does not hold `archive_offset` events, the session has no live log, or the log or the archive
  /// offset changed during the scan; the node's reads then load the log themselves.
  pub fn scan_live_facts(&self, session_id: &str) -> StoreResult<Option<ScannedLiveFacts>> {
    let archived = self.read_archive_offset(session_id)?;
    let Some(archive) = self.scan_archive_events(session_id, archived)? else {
      return Ok(None);
    };
    let path = self.chat_events_path(session_id);
    let Some(identity) = FileIdentity::read(&path)? else {
      return Ok(None);
    };
    let started = Instant::now();
    let events: Vec<SharedChatEvent> =
      ndjson::read_file_lines_split(&path, scan_line)?.into_iter().map(Arc::new).collect();
    log_large_scan(&path, identity.size(), started);
    if FileIdentity::read(&path)? != Some(identity) || self.read_archive_offset(session_id)? != archived {
      return Ok(None);
    }
    let (facts, runs) = fold_scanned_history(&archive, &events, archived)?;
    let prompt_changes = LivePromptChanges::of(&events);
    Ok(Some(ScannedLiveFacts { identity, archived, covered: events.len(), facts, runs, prompt_changes }))
  }

  /// The first `archived` events of the session's archive files, scanned as [`Store::scan_live_facts`] scans a live
  /// log. `None` when the files hold fewer events than `archived`, which the node's read then meets itself.
  fn scan_archive_events(&self, session_id: &str, archived: usize) -> StoreResult<Option<Vec<SharedChatEvent>>> {
    let mut events: Vec<SharedChatEvent> = Vec::with_capacity(archived);
    for path in self.archive_paths(session_id)?.iter() {
      if events.len() >= archived {
        break;
      }
      let scanned = ndjson::read_file_lines_split(path, scan_line)?;
      events.extend(scanned.into_iter().map(Arc::new));
    }
    if events.len() < archived {
      return Ok(None);
    }
    events.truncate(archived);
    Ok(Some(events))
  }

  /// Memoize a [`Store::scan_live_facts`] result under its log identity, as a recovery eviction seals a fold. The seal
  /// happens only while the event cache is cold, the log keeps the scanned identity and the archive keeps the scanned
  /// `archive_offset`, checked under the session lock that every append holds. Returns whether the scan was sealed.
  pub fn seal_scanned_facts(&self, session_id: &str, scanned: ScannedLiveFacts) -> StoreResult<bool> {
    let cache_entry = self.events_cache_entry(session_id);
    let cache = cache_entry.lock().expect("session event cache mutex is not poisoned");
    if cache.is_some()
      || FileIdentity::read(&self.chat_events_path(session_id))? != Some(scanned.identity)
      || self.read_archive_offset(session_id)? != scanned.archived
    {
      return Ok(false);
    }
    let ScannedLiveFacts { identity, archived, covered, facts, runs, prompt_changes } = scanned;
    self.history_work.full_folds.fetch_add(1, Ordering::Relaxed);
    self.history_work.folded_events.fetch_add((archived + covered) as u64, Ordering::Relaxed);
    self.node_facts_cache.lock().expect("node facts cache mutex is not poisoned").insert(
      session_id.to_string(),
      CachedNodeFacts {
        key: NodeFactsKey::File(identity),
        list_identity: Arc::new(()),
        archived,
        covered,
        facts: Arc::new(facts),
      },
    );
    let version = self.next_node_state_version.fetch_add(1, Ordering::Relaxed);
    self
      .node_state_cache
      .lock()
      .expect("node state cache mutex is not poisoned")
      .insert(session_id.to_string(), CachedNodeState { key: NodeStateKey::File(identity), archived, version });
    self
      .run_facts_by_session
      .lock()
      .expect("run facts by session mutex is not poisoned")
      .insert(session_id.to_string(), CachedRunFactsAt { identity: Some(identity), facts: Arc::new(runs) });
    self
      .sealed_prompt_changes
      .lock()
      .expect("sealed prompt changes mutex is not poisoned")
      .insert(session_id.to_string(), SealedPromptChanges { identity, changes: prompt_changes });
    Ok(true)
  }
}

/// Logs a fact scan of [`WHOLE_LOG_LOGGED_BYTES`] or more with its file, size and wall time, so a slow boot names the
/// log it waited on.
fn log_large_scan(path: &Path, bytes: u64, started: Instant) {
  if bytes >= WHOLE_LOG_LOGGED_BYTES {
    tracing::info!(target: "charliebot_store", path = %path.display(), bytes, elapsed_ms = started.elapsed().as_millis() as u64, "chat_events_fact_scan");
  }
}

/// One line as the scan keeps it: the full event, a chat-content line's stand-in, or `None` where a full read skips the
/// line.
fn scan_line(raw: &[u8], keys: &ParseKeys) -> Option<CompactValue> {
  ndjson::parse_line(raw, |text| {
    let mut deserializer = serde_json::Deserializer::from_str(text);
    let head = HeadSeed.deserialize(&mut deserializer).ok()?;
    deserializer.end().ok()?;
    match head {
      Some(head) if head.stands_in() => Some(head.stand_in(keys)),
      _ => ndjson::parse_file_text(text, keys),
    }
  })
}

/// The top-level fields of one object line that decide how the scan keeps it. A repeated key takes its last value, as
/// the full parse does.
#[derive(Default)]
struct LineHead {
  /// `type` when it is a string.
  event_type: Option<Box<str>>,
  /// `id` when it is a string.
  id: Option<Box<str>>,
  content_is_str: bool,
}

impl LineHead {
  fn stands_in(&self) -> bool {
    match self.event_type.as_deref() {
      Some(event_types::USER) => !self.content_is_str,
      Some(event_type) => CHAT_CONTENT_TYPES.contains(&event_type),
      None => false,
    }
  }

  fn stand_in(self, keys: &ParseKeys) -> CompactValue {
    let mut entries = Vec::with_capacity(2);
    if let Some(id) = self.id {
      entries.push(ObjectEntry { key: keys.key("id"), value: CompactValue::Str(id) });
    }
    if let Some(event_type) = self.event_type {
      entries.push(ObjectEntry { key: keys.key("type"), value: CompactValue::Str(event_type) });
    }
    CompactValue::Object(entries.into_boxed_slice())
  }
}

/// The visits of the scalar JSON values other than a string, each answering `$value`.
macro_rules! dropped_scalars {
  ($value:expr) => {
    fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Self::Value, E> {
      Ok($value)
    }

    fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Self::Value, E> {
      Ok($value)
    }

    fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Self::Value, E> {
      Ok($value)
    }

    fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Self::Value, E> {
      Ok($value)
    }

    fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
      Ok($value)
    }
  };
}

/// The head of one line's top-level value: `None` when the value is not an object. Every value is parsed through
/// `deserialize_any` and every key through `deserialize_str`, as the full parse does, so a line parses here exactly when
/// it parses there.
struct HeadSeed;

impl<'de> DeserializeSeed<'de> for HeadSeed {
  type Value = Option<LineHead>;

  fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
    struct HeadVisitor;

    impl<'de> Visitor<'de> for HeadVisitor {
      type Value = Option<LineHead>;

      fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("any JSON value")
      }

      dropped_scalars!(None);

      fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Self::Value, E> {
        Ok(None)
      }

      fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        while seq.next_element_seed(Skip)?.is_some() {}
        Ok(None)
      }

      fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
        let mut head = LineHead::default();
        while let Some(key) = access.next_key_seed(HeadKeySeed)? {
          match key {
            HeadKey::Type => head.event_type = access.next_value_seed(StrSeed(true))?,
            HeadKey::Id => head.id = access.next_value_seed(StrSeed(true))?,
            HeadKey::Content => head.content_is_str = access.next_value_seed(StrSeed(false))?.is_some(),
            HeadKey::Other => access.next_value_seed(Skip)?,
          }
        }
        Ok(Some(head))
      }
    }

    deserializer.deserialize_any(HeadVisitor)
  }
}

/// The top-level keys the scan reads.
enum HeadKey {
  Type,
  Id,
  Content,
  Other,
}

struct HeadKeySeed;

impl<'de> DeserializeSeed<'de> for HeadKeySeed {
  type Value = HeadKey;

  fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
    struct HeadKeyVisitor;

    impl<'de> Visitor<'de> for HeadKeyVisitor {
      type Value = HeadKey;

      fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("an object key")
      }

      fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
        Ok(match value {
          "type" => HeadKey::Type,
          "id" => HeadKey::Id,
          "content" => HeadKey::Content,
          _ => HeadKey::Other,
        })
      }
    }

    deserializer.deserialize_str(HeadKeyVisitor)
  }
}

/// `Some` when a value is a string, holding its text when the flag asks for it; any other value is parsed and dropped.
struct StrSeed(bool);

impl<'de> DeserializeSeed<'de> for StrSeed {
  type Value = Option<Box<str>>;

  fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
    struct StrVisitor(bool);

    impl<'de> Visitor<'de> for StrVisitor {
      type Value = Option<Box<str>>;

      fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("any JSON value")
      }

      dropped_scalars!(None);

      fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
        // A presence check keeps no text: an empty boxed str allocates nothing.
        Ok(Some(if self.0 { value.into() } else { Box::default() }))
      }

      fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        while seq.next_element_seed(Skip)?.is_some() {}
        Ok(None)
      }

      fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
        while access.next_key_seed(SkipKey)?.is_some() {
          access.next_value_seed(Skip)?;
        }
        Ok(None)
      }
    }

    deserializer.deserialize_any(StrVisitor(self.0))
  }
}

/// One value parsed and dropped.
struct Skip;

impl<'de> DeserializeSeed<'de> for Skip {
  type Value = ();

  fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
    struct SkipVisitor;

    impl<'de> Visitor<'de> for SkipVisitor {
      type Value = ();

      fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("any JSON value")
      }

      dropped_scalars!(());

      fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Self::Value, E> {
        Ok(())
      }

      fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        while seq.next_element_seed(Skip)?.is_some() {}
        Ok(())
      }

      fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
        while access.next_key_seed(SkipKey)?.is_some() {
          access.next_value_seed(Skip)?;
        }
        Ok(())
      }
    }

    deserializer.deserialize_any(SkipVisitor)
  }
}

/// One object key parsed and dropped.
struct SkipKey;

impl<'de> DeserializeSeed<'de> for SkipKey {
  type Value = ();

  fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
    struct SkipKeyVisitor;

    impl<'de> Visitor<'de> for SkipKeyVisitor {
      type Value = ();

      fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("an object key")
      }

      fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Self::Value, E> {
        Ok(())
      }
    }

    deserializer.deserialize_str(SkipKeyVisitor)
  }
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::*;
  use crate::metadata::{SessionMetadata, TaskProfile};
  use crate::store::test_support::{make_test_store, open_test_store};

  /// A scan taken outside the session lock seals only while the log keeps the identity it scanned and the cache is cold.
  #[tokio::test]
  async fn a_scan_seals_only_while_the_log_keeps_its_scanned_identity_and_the_cache_is_cold() {
    let (_home, store) = make_test_store();
    // An append to a cold cache writes only the log, so the cache stays cold throughout.
    store.append_chat_event("synthetic", json!({"id": "event-a", "type": "user"})).await.unwrap();
    let stale = store.scan_live_facts("synthetic").unwrap().expect("nothing writes during the scan");
    store.append_chat_event("synthetic", json!({"id": "event-b", "type": "user"})).await.unwrap();
    assert!(!store.seal_scanned_facts("synthetic", stale).unwrap(), "a scan that misses an append is refused");

    let fresh = store.scan_live_facts("synthetic").unwrap().expect("nothing writes during the scan");
    assert!(store.seal_scanned_facts("synthetic", fresh).unwrap(), "a scan of the current log seals");
    store.load_chat_events_shared_sync("synthetic").unwrap();
    let again = store.scan_live_facts("synthetic").unwrap().expect("nothing writes during the scan");
    assert!(!store.seal_scanned_facts("synthetic", again).unwrap(), "a warm cache keeps its own fold");
  }

  /// A sealed scan answers every fact a cold read folds, over chat-content lines that carry fact-like fields, a
  /// stand-in whose id a later fact repeats, and the lines a full read drops, and it reads no log to answer them.
  #[tokio::test]
  async fn a_sealed_scan_answers_the_facts_a_cold_read_folds() {
    let (home, writer) = make_test_store();
    for event in [
      json!({"id": "created", "type": "task_created", "actor": "user"}),
      json!({"id": "ask", "type": "user", "content": "do the work"}),
      json!({"id": "tool-out", "type": "user", "content": [{"type": "tool_result", "content": "done"}]}),
      json!({"id": "said", "type": "assistant", "run_id": "run-1", "outcome": "failed", "content": "x"}),
      json!({"id": "said", "type": "run_finished", "run_id": "run-0", "outcome": "success"}),
      json!({"type": "system", "subtype": "init", "input_event_ids": ["ask"]}),
      json!({"id": "subtree-prompt", "type": "prompt_changed", "scope": "subtree", "new_ref": "prompt-a"}),
      json!({"id": "stop-1", "type": "run_stop_requested", "run_id": "run-1", "request_id": "stop-1"}),
      json!({"id": "finish-1", "type": "run_finished", "run_id": "run-1", "outcome": "success", "input_event_ids": ["ask"]}),
      json!({"id": "report", "type": "child_report", "child_session_id": "child", "child_event_id": "close-1"}),
      json!({"id": "close-ask", "type": "task_close_requested", "request_id": "close-1", "owner_run_id": "run-1"}),
      json!({"id": "summon", "type": "agent_message", "content": "hello", "slack": {"channel": "c"}}),
      json!({"id": "result", "type": "result", "is_error": false}),
    ] {
      writer.append_chat_event("synthetic", event).await.unwrap();
    }
    let path = writer.chat_events_path("synthetic");
    let mut log = std::fs::read(&path).unwrap();
    log.extend_from_slice(b"{\"type\": \"assistant\", \"n\": 1e400}\n{broken\n\n[1, 2]\n");
    std::fs::write(&path, log).unwrap();

    let open = || open_test_store(home.path(), Arc::new(crate::EmptyRuntimeEventHooks));
    let (scanned, cold) = (open().unwrap(), open().unwrap());
    let scan = scanned.scan_live_facts("synthetic").unwrap().expect("nothing writes during the scan");
    assert!(scanned.seal_scanned_facts("synthetic", scan).unwrap());

    let (sealed, folded) = (scanned.node_facts("synthetic").unwrap(), cold.node_facts("synthetic").unwrap());
    let (sealed, folded) = (sealed.task().unwrap(), folded.task().unwrap());
    let values = |events: &[SharedChatEvent]| events.iter().map(|event| event.to_value()).collect::<Vec<_>>();
    assert_eq!(sealed.task_state, folded.task_state);
    assert_eq!(sealed.has_user_request, folded.has_user_request);
    assert_eq!(sealed.run_outcomes, folded.run_outcomes);
    assert_eq!(sealed.run_finish_positions, folded.run_finish_positions);
    assert_eq!(sealed.confirmed_input_ids, folded.confirmed_input_ids);
    assert_eq!(values(&sealed.input_candidates), values(&folded.input_candidates));
    assert_eq!((sealed.boundary_index, sealed.boundary_open), (folded.boundary_index, folded.boundary_open));
    assert_eq!(values(&sealed.close_events), values(&folded.close_events));
    assert_eq!(values(&sealed.close_requests), values(&folded.close_requests));
    assert_eq!(sealed.delivered_reports, folded.delivered_reports);
    let id_types = |facts: &crate::TaskFacts| {
      facts.events_by_id.iter().map(|(id, event)| (id.to_string(), event["type"].to_value())).collect::<Vec<_>>()
    };
    assert_eq!(id_types(sealed), id_types(folded));
    assert!(sealed.events_by_id["said"].get("content").is_none(), "the seal holds a chat line's stand-in");
    assert!(folded.events_by_id["said"].get("content").is_some());
    assert_eq!(sealed.covered, folded.covered);
    assert_eq!(scanned.run_facts("synthetic").unwrap(), cold.run_facts("synthetic").unwrap());
    assert_eq!(scanned.task_state("synthetic").unwrap(), cold.task_state("synthetic").unwrap());
    let newest =
      |store: &Store, scope| store.newest_live_prompt_change("synthetic", scope).unwrap().map(|e| e.to_value());
    for scope in [PromptScope::Subtree, PromptScope::Node] {
      assert_eq!(newest(&scanned, scope), newest(&cold, scope));
    }
    assert_eq!(scanned.history_work().file_reads, 0, "the sealed facts answer without reading the log");
  }

  /// A session with archived events seals too: its scan folds the archive before the live log, as a cold read does, and
  /// the sealed facts answer what that cold read folds without reading the log.
  #[tokio::test]
  async fn a_sealed_scan_of_an_archived_session_folds_its_archive_first() {
    let (home, writer) = make_test_store();
    let session = "archived";
    let path = writer.chat_events_path(session);
    let archive_dir = path.parent().unwrap().join("archives");
    std::fs::create_dir_all(&archive_dir).unwrap();
    std::fs::write(
      archive_dir.join("chat_events.2026-W44.jsonl"),
      concat!(
        "{\"id\": \"created\", \"type\": \"task_created\", \"actor\": \"user\"}\n",
        "{\"id\": \"ask\", \"type\": \"user\", \"content\": \"do the work\"}\n",
        "{\"id\": \"said\", \"type\": \"assistant\", \"run_id\": \"run-1\", \"content\": \"x\"}\n",
      ),
    )
    .unwrap();
    std::fs::write(
      &path,
      concat!(
        "{\"id\": \"finish-1\", \"type\": \"run_finished\", \"run_id\": \"run-1\", \"outcome\": \"success\", ",
        "\"input_event_ids\": [\"ask\"]}\n",
        "{\"id\": \"close-1\", \"type\": \"task_closed\", \"request_id\": \"close-1\", \"outcome\": \"completed\"}\n",
      ),
    )
    .unwrap();
    let mut meta = SessionMetadata::new(session, "seed", TaskProfile::Manager);
    meta.archive_offset = 3;
    std::fs::write(writer.metadata_path(session), meta.to_json_string(&writer.slots, true)).unwrap();

    let open = || open_test_store(home.path(), Arc::new(crate::EmptyRuntimeEventHooks));
    let (scanned, cold) = (open().unwrap(), open().unwrap());
    let scan = scanned.scan_live_facts(session).unwrap().expect("nothing writes during the scan");
    assert!(scanned.seal_scanned_facts(session, scan).unwrap());

    let (sealed, folded) = (scanned.node_facts(session).unwrap(), cold.node_facts(session).unwrap());
    let (sealed, folded) = (sealed.task().unwrap(), folded.task().unwrap());
    assert_eq!(sealed.task_state, folded.task_state);
    assert_eq!(sealed.run_outcomes, folded.run_outcomes);
    assert_eq!(sealed.confirmed_input_ids, folded.confirmed_input_ids);
    assert_eq!(sealed.covered, folded.covered);
    let sorted_ids = |facts: &crate::TaskFacts| {
      let mut ids = facts.events_by_id.iter().map(|(id, _)| id.to_string()).collect::<Vec<_>>();
      ids.sort();
      ids
    };
    assert_eq!(sorted_ids(sealed), sorted_ids(folded));
    assert_eq!(scanned.run_facts(session).unwrap(), cold.run_facts(session).unwrap());
    assert_eq!(scanned.history_work().file_reads, 0, "the sealed facts answer without reading the log");
  }

  /// A scanned line is kept exactly when a full read keeps it; a stand-in holds the full event's string id and type,
  /// and every other kept line is the full event.
  #[test]
  fn scanned_lines_keep_what_a_full_read_keeps() {
    let deep = format!("{{\"type\": \"assistant\", \"x\": {}{}}}", "[".repeat(200), "]".repeat(200));
    let lines: Vec<&[u8]> = vec![
      br#"{"id": "a", "type": "assistant", "content": [{"type": "text", "text": "hi"}]}"#,
      br#"{"type": "user", "content": "a request", "id": "u"}"#,
      br#"{"type": "user", "content": [{"type": "tool_result"}], "id": "t"}"#,
      br#"{"type": "user", "content": "a", "content": [1]}"#,
      br#"{"type": "user", "content": [1], "content": "b"}"#,
      br#"{"content": "x", "type": "user", "type": "assistant"}"#,
      br#"{"type": "assistant", "type": 7}"#,
      br#"{"type": "assistant", "id": 5}"#,
      br#"{"type": "assistant", "n": 1e400}"#,
      br#"{"type": "assistant", "s": "bad \x escape"}"#,
      b"{\"type\": \"assistant\", \"text\": \"torn \xff byte\"}",
      deep.as_bytes(),
      br#"{"type": "assistant"} trailing"#,
      br#"{"type": "run_finished", "run_id": "r", "outcome": "success"}"#,
      br#"[1, 2]"#,
      br#""bare""#,
      b"{broken",
      b"   ",
    ];
    let keys = ParseKeys::default();
    for line in lines {
      let full = ndjson::parse_file_line(line, &keys);
      let scanned = scan_line(line, &keys);
      assert_eq!(scanned.is_some(), full.is_some(), "line {:?}", String::from_utf8_lossy(line));
      let (Some(full), Some(scanned)) = (full, scanned) else { continue };
      if scanned != full {
        assert_eq!(scanned.get("id").and_then(CompactValue::as_str), full.get("id").and_then(CompactValue::as_str));
        assert_eq!(scanned.get("type").and_then(CompactValue::as_str), full.get("type").and_then(CompactValue::as_str));
        assert!(scanned.get("content").is_none(), "a stand-in keeps no content: {line:?}");
      }
    }
  }
}
