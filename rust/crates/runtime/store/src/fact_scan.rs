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
use charliebot_core::fsutil;
use serde::de::{DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};

use crate::compact::{CompactValue, KeySource, ObjectEntry, ParseKeys};
use crate::event_types::{FileIdentity, SharedChatEvent};
use crate::events::WHOLE_LOG_LOGGED_BYTES;
use crate::fact_checkpoint::{ArchiveMark, FactCheckpoint};
use crate::ndjson;
use crate::run_facts::RunFacts;
use crate::task_tree::{PromptScope, fold_appended_history, fold_scanned_history, newest_prompt_change};
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
  facts: Arc<NodeFacts>,
  runs: Arc<RunFacts>,
  prompt_changes: LivePromptChanges,
  /// The bytes the scan read from the live log: the whole log on a full scan, the appended tail on a resumed one.
  read_bytes: u64,
  /// The bytes the scan read from the fact checkpoint, 0 on a full scan.
  checkpoint_bytes: u64,
}

impl LivePromptChanges {
  fn of(events: &[SharedChatEvent]) -> Self {
    Self {
      subtree: newest_prompt_change(events, PromptScope::Subtree),
      node: newest_prompt_change(events, PromptScope::Node),
    }
  }

  /// The newest prompt change of each scope across a checkpoint's kept lines and the appended tail.
  fn of_combined(prefix: &[SharedChatEvent], tail: &[SharedChatEvent]) -> Self {
    let mut changes = Self::of(prefix);
    changes.subtree = newest_prompt_change(tail, PromptScope::Subtree).or(changes.subtree);
    changes.node = newest_prompt_change(tail, PromptScope::Node).or(changes.node);
    changes
  }

  pub(crate) fn newest(&self, scope: PromptScope) -> Option<SharedChatEvent> {
    match scope {
      PromptScope::Subtree => self.subtree.clone(),
      PromptScope::Node => self.node.clone(),
    }
  }
}

impl ScannedLiveFacts {
  /// The bytes the scan read from the live log: the whole log on a full scan, the appended tail on a resumed one.
  pub fn read_bytes(&self) -> u64 {
    self.read_bytes
  }

  /// The bytes the scan read from its fact checkpoint, 0 when it scanned the log whole.
  pub fn checkpoint_bytes(&self) -> u64 {
    self.checkpoint_bytes
  }

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

/// One node's fold, ready to memoize: the facts, the run facts, the newest prompt fact of each scope and the history
/// they cover.
struct FoldedNodeFacts {
  session_id: String,
  identity: FileIdentity,
  archived: usize,
  covered: usize,
  facts: Arc<NodeFacts>,
  runs: Arc<RunFacts>,
  prompt_changes: LivePromptChanges,
}

impl Store {
  /// Scan and fold a session's history outside its session lock: its archived events, then its live log. `None`
  /// when the archive does not hold `archive_offset` events, the session has no live log, or the log or the archive
  /// offset changed during the scan; the node's reads then load the log themselves.
  ///
  /// A checkpoint that still covers the log's prefix answers the scan from the kept lines it holds and the bytes past
  /// its offset, so the scan parses the checkpoint's fraction of the log and folds the same events a full fold folds.
  /// A checkpoint that does not cover this log leaves the full scan to run, as a missing one does. Either way the scan
  /// leaves a checkpoint behind for the next boot (unless its tail was too small to justify a rewrite).
  pub fn scan_live_facts(&self, session_id: &str) -> StoreResult<Option<ScannedLiveFacts>> {
    let archived = self.read_archive_offset(session_id)?;
    let path = self.chat_events_path(session_id);
    let Some(identity) = FileIdentity::read(&path)? else {
      return Ok(None);
    };
    let archive_marks = self.archive_marks(session_id)?;
    if let Some(scanned) = self.scan_from_checkpoint(session_id, &path, identity, archived, archive_marks.clone())? {
      return Ok(Some(scanned));
    }
    let Some(archive) = self.scan_archive_events(session_id, archived)? else {
      return Ok(None);
    };
    let started = Instant::now();
    let mapped = read_log(&path)?;
    let file: &[u8] = mapped.as_deref().unwrap_or(&[]);
    let events: Vec<SharedChatEvent> = ndjson::split_lines_of(file, &scan_line).into_iter().map(Arc::new).collect();
    log_large_scan(&path, identity.size(), file.len() as u64, 0, started);
    if FileIdentity::read(&path)? != Some(identity) || self.read_archive_offset(session_id)? != archived {
      return Ok(None);
    }
    let (facts, runs) = fold_scanned_history(&archive, &events, archived)?;
    let prompt_changes = LivePromptChanges::of(&events);
    let kept: Vec<&SharedChatEvent> = archive.iter().chain(events.iter()).collect();
    self.leave_checkpoint(session_id, FactCheckpoint::new(archived, events.len(), archive_marks, file), kept);
    Ok(Some(ScannedLiveFacts {
      identity,
      archived,
      covered: events.len(),
      facts: Arc::new(facts),
      runs: Arc::new(runs),
      prompt_changes,
      read_bytes: file.len() as u64,
      checkpoint_bytes: 0,
    }))
  }

  /// The scan of a log whose checkpoint still covers its prefix: the checkpoint's kept lines and the bytes past its
  /// offset, folded as the full scan folds them. `None` when no checkpoint is there or it does not cover this log.
  fn scan_from_checkpoint(
    &self,
    session_id: &str,
    path: &Path,
    identity: FileIdentity,
    archived: usize,
    archive_marks: Vec<ArchiveMark>,
  ) -> StoreResult<Option<ScannedLiveFacts>> {
    let checkpoint_path = self.fact_checkpoint_path(session_id);
    let raw = match std::fs::read(&checkpoint_path) {
      Ok(raw) => raw,
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
      Err(error) => {
        tracing::warn!(target: "charliebot_store", path = %checkpoint_path.display(), error = %error, "fact_checkpoint_unreadable");
        return Ok(None);
      }
    };
    let Some((checkpoint, events_offset)) = FactCheckpoint::parse(&raw) else {
      return Ok(None);
    };
    let started = Instant::now();
    let mapped = read_log(path)?;
    let file: &[u8] = mapped.as_deref().unwrap_or(&[]);
    if file.len() as u64 != identity.size() || !checkpoint.covers(file, archived, &archive_marks) {
      return Ok(None);
    }
    let prefix: Vec<SharedChatEvent> =
      ndjson::split_lines_of(&raw[events_offset..], &checkpoint_line).into_iter().map(Arc::new).collect();
    if prefix.len() != checkpoint.archived + checkpoint.covered {
      return Ok(None);
    }
    let offset = checkpoint.offset as usize;
    let tail: Vec<SharedChatEvent> =
      ndjson::split_lines_of(&file[offset..], &scan_line).into_iter().map(Arc::new).collect();
    let checkpoint_bytes = raw.len() as u64;
    log_large_scan(path, identity.size(), (file.len() - offset) as u64, checkpoint_bytes, started);
    if FileIdentity::read(path)? != Some(identity) || self.read_archive_offset(session_id)? != archived {
      return Ok(None);
    }
    let (covered, facts, runs) = fold_checkpoint_and_tail(&prefix, &tail, &checkpoint, archived)?;
    let prompt_changes = LivePromptChanges::of_combined(&prefix, &tail);
    // Rewrite the checkpoint only once the appended tail is a sizeable share of it, so a steady state of small appends
    // keeps one checkpoint read per boot and the next full fold amortizes the write.
    let appended = identity.size().saturating_sub(checkpoint.offset);
    if appended > 0 && appended.saturating_mul(8) >= checkpoint.offset.max(1 << 20) {
      let kept: Vec<&SharedChatEvent> = prefix.iter().chain(tail.iter()).collect();
      self.leave_checkpoint(session_id, FactCheckpoint::new(archived, covered, archive_marks, file), kept);
    }
    Ok(Some(ScannedLiveFacts {
      identity,
      archived,
      covered,
      facts: Arc::new(facts),
      runs: Arc::new(runs),
      prompt_changes,
      read_bytes: appended,
      checkpoint_bytes,
    }))
  }

  /// Write the checkpoint these kept events make, logging a failure rather than failing the scan: the file only makes
  /// the next boot faster, and its absence costs that boot a full scan and nothing else.
  fn leave_checkpoint(&self, session_id: &str, checkpoint: FactCheckpoint, kept: Vec<&SharedChatEvent>) {
    let text = checkpoint.to_bytes(&crate::fact_checkpoint::encode_events(kept));
    let path = self.fact_checkpoint_path(session_id);
    if let Err(error) = crate::fact_checkpoint::write(&path, &text) {
      tracing::warn!(target: "charliebot_store", session_id, path = %path.display(), error = %error, "fact_checkpoint_write_failed");
    }
  }

  /// The identity of each archive file the session's history folds, in filename order.
  fn archive_marks(&self, session_id: &str) -> StoreResult<Vec<ArchiveMark>> {
    let mut marks = Vec::new();
    for path in self.archive_paths(session_id)?.iter() {
      match ArchiveMark::of(path)? {
        Some(mark) => marks.push(mark),
        // A file that vanished between the listing and the stat leaves no mark, so a checkpoint that folded it fails
        // validation and the log is scanned whole.
        None => return Ok(Vec::new()),
      }
    }
    Ok(marks)
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
  pub fn seal_scanned_facts(&self, session_id: &str, scanned: &ScannedLiveFacts) -> StoreResult<bool> {
    let cache_entry = self.events_cache_entry(session_id);
    let cache = cache_entry.lock().expect("session event cache mutex is not poisoned");
    if cache.is_some()
      || FileIdentity::read(&self.chat_events_path(session_id))? != Some(scanned.identity)
      || self.read_archive_offset(session_id)? != scanned.archived
    {
      return Ok(false);
    }
    drop(cache);
    self.insert_facts(FoldedNodeFacts {
      session_id: session_id.to_string(),
      identity: scanned.identity,
      archived: scanned.archived,
      covered: scanned.covered,
      facts: Arc::clone(&scanned.facts),
      runs: Arc::clone(&scanned.runs),
      prompt_changes: LivePromptChanges {
        subtree: scanned.prompt_changes.subtree.clone(),
        node: scanned.prompt_changes.node.clone(),
      },
    });
    Ok(true)
  }

  /// A node's facts from its checkpoint, for a reader whose event cache is cold: the checkpoint's facts folded with the
  /// bytes past its offset, with no whole-log load. `None` when no checkpoint covers this log, which leaves the
  /// caller's fold to load the log. The folded facts are memoized under the log identity, as a seal is.
  pub(crate) fn checkpoint_facts_until(
    &self,
    session_id: &str,
    identity: FileIdentity,
    archived: usize,
    should_stop: &mut impl FnMut() -> bool,
  ) -> StoreResult<Option<Arc<NodeFacts>>> {
    let path = self.chat_events_path(session_id);
    let checkpoint_path = self.fact_checkpoint_path(session_id);
    let raw = match std::fs::read(&checkpoint_path) {
      Ok(raw) => raw,
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
      Err(error) => {
        tracing::warn!(target: "charliebot_store", path = %checkpoint_path.display(), error = %error, "fact_checkpoint_unreadable");
        return Ok(None);
      }
    };
    let Some((checkpoint, events_offset)) = FactCheckpoint::parse(&raw) else {
      return Ok(None);
    };
    let marks = self.archive_marks(session_id)?;
    let Some(mapped) = read_log(&path)? else {
      return Ok(None);
    };
    let file: &[u8] = &mapped;
    if file.len() as u64 != identity.size() || !checkpoint.covers(file, archived, &marks) || should_stop() {
      return Ok(None);
    }
    let prefix: Vec<SharedChatEvent> =
      ndjson::split_lines_of(&raw[events_offset..], &checkpoint_line).into_iter().map(Arc::new).collect();
    if prefix.len() != checkpoint.archived + checkpoint.covered {
      return Ok(None);
    }
    let offset = checkpoint.offset as usize;
    let tail: Vec<SharedChatEvent> =
      ndjson::split_lines_of(&file[offset..], &scan_line).into_iter().map(Arc::new).collect();
    if should_stop() || FileIdentity::read(&path)? != Some(identity) {
      return Ok(None);
    }
    let (covered, facts, runs) = fold_checkpoint_and_tail(&prefix, &tail, &checkpoint, archived)?;
    let prompt_changes = LivePromptChanges::of_combined(&prefix, &tail);
    let facts = Arc::new(facts);
    let runs = Arc::new(runs);
    self.insert_facts(FoldedNodeFacts {
      session_id: session_id.to_string(),
      identity,
      archived,
      covered,
      facts: Arc::clone(&facts),
      runs,
      prompt_changes,
    });
    Ok(Some(facts))
  }

  /// Memoize one node's folded facts, run facts and newest prompt facts under the log identity they cover.
  fn insert_facts(&self, folded: FoldedNodeFacts) {
    let FoldedNodeFacts { session_id, identity, archived, covered, facts, runs, prompt_changes } = folded;
    self.history_work.full_folds.fetch_add(1, Ordering::Relaxed);
    self.history_work.folded_events.fetch_add((archived + covered) as u64, Ordering::Relaxed);
    self.node_facts_cache.lock().expect("node facts cache mutex is not poisoned").insert(
      session_id.to_string(),
      CachedNodeFacts { key: NodeFactsKey::File(identity), list_identity: Arc::new(()), archived, covered, facts },
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
      .insert(session_id.to_string(), CachedRunFactsAt { identity: Some(identity), facts: runs });
    self
      .sealed_prompt_changes
      .lock()
      .expect("sealed prompt changes mutex is not poisoned")
      .insert(session_id.to_string(), SealedPromptChanges { identity, changes: prompt_changes });
  }
}

/// The bytes of one event log, read whole when small and mapped otherwise.
fn read_log(path: &Path) -> StoreResult<Option<fsutil::FileBytes>> {
  match fsutil::read_or_map_file(path) {
    Ok(bytes) => Ok(bytes),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
    Err(error) => Err(crate::StoreError::new(format!("{}: {error}", path.display()))),
  }
}

/// Fold a checkpoint's kept lines and the appended tail exactly as a full scan folds the whole history: the archived
/// events at their positions, the live prefix at the archive offset, and the tail past it. Returns the live event
/// count the fold covers and the facts.
fn fold_checkpoint_and_tail(
  prefix: &[SharedChatEvent],
  tail: &[SharedChatEvent],
  checkpoint: &FactCheckpoint,
  archived: usize,
) -> StoreResult<(usize, NodeFacts, RunFacts)> {
  let mut facts = NodeFacts::default();
  let mut runs = RunFacts::default();
  fold_appended_history(&mut facts, &mut runs, &prefix[..checkpoint.archived], 0)?;
  fold_appended_history(&mut facts, &mut runs, &prefix[checkpoint.archived..], archived)?;
  fold_appended_history(&mut facts, &mut runs, tail, archived + checkpoint.covered)?;
  Ok((checkpoint.covered + tail.len(), facts, runs))
}

/// Logs a fact scan of a log of [`WHOLE_LOG_LOGGED_BYTES`] or more with its file and wall time, so a slow boot names
/// the log it waited on. `bytes` is what the scan read from the log and `checkpoint_bytes` what it read from the
/// checkpoint, so a start that resumes reports the appended bytes it folded rather than the log's size.
fn log_large_scan(path: &Path, log_bytes: u64, read_bytes: u64, checkpoint_bytes: u64, started: Instant) {
  if log_bytes >= WHOLE_LOG_LOGGED_BYTES {
    tracing::info!(
      target: "charliebot_store",
      path = %path.display(),
      bytes = read_bytes,
      log_bytes,
      checkpoint_bytes,
      elapsed_ms = started.elapsed().as_millis() as u64,
      "chat_events_fact_scan"
    );
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
    stand_in_object(keys, self.id.as_deref(), self.event_type.as_deref().unwrap_or_default())
  }
}

/// One event's stand-in: its string `id` when it has one, then its `type`. A chat-content line keeps nothing else.
fn stand_in_object(keys: &ParseKeys, id: Option<&str>, event_type: &str) -> CompactValue {
  let mut entries = Vec::with_capacity(2);
  if let Some(id) = id {
    entries.push(ObjectEntry { key: keys.key("id"), value: CompactValue::Str(id.into()) });
  }
  entries.push(ObjectEntry { key: keys.key("type"), value: CompactValue::Str(event_type.into()) });
  CompactValue::Object(entries.into_boxed_slice())
}

/// One checkpoint line as the scan kept it: a stand-in line ([`crate::fact_checkpoint::STAND_IN_TAG`] with the id, a
/// tab and the type, or [`crate::fact_checkpoint::TYPE_ONLY_TAG`] with the type), or a compact JSON event line, which
/// the scan's own line rule parses.
pub(crate) fn checkpoint_line(raw: &[u8], keys: &ParseKeys) -> Option<CompactValue> {
  match raw.first().copied() {
    Some(crate::fact_checkpoint::STAND_IN_TAG) => {
      let body = std::str::from_utf8(&raw[1..]).ok()?;
      let (id, event_type) = body.split_once('\t')?;
      Some(stand_in_object(keys, Some(id), event_type))
    }
    Some(crate::fact_checkpoint::TYPE_ONLY_TAG) => {
      let event_type = std::str::from_utf8(&raw[1..]).ok()?;
      Some(stand_in_object(keys, None, event_type))
    }
    _ => scan_line(raw, keys),
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
    assert!(!store.seal_scanned_facts("synthetic", &stale).unwrap(), "a scan that misses an append is refused");

    let fresh = store.scan_live_facts("synthetic").unwrap().expect("nothing writes during the scan");
    assert!(store.seal_scanned_facts("synthetic", &fresh).unwrap(), "a scan of the current log seals");
    store.load_chat_events_shared_sync("synthetic").unwrap();
    let again = store.scan_live_facts("synthetic").unwrap().expect("nothing writes during the scan");
    assert!(!store.seal_scanned_facts("synthetic", &again).unwrap(), "a warm cache keeps its own fold");
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
    let scanned = open().unwrap();
    let scan = scanned.scan_live_facts("synthetic").unwrap().expect("nothing writes during the scan");
    assert!(scanned.seal_scanned_facts("synthetic", &scan).unwrap());
    // The scan left a checkpoint; removing it makes the reference store fold the log itself, which this test compares.
    std::fs::remove_file(scanned.fact_checkpoint_path("synthetic")).unwrap();
    let cold = open().unwrap();

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
    assert!(scanned.seal_scanned_facts(session, &scan).unwrap());

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

  /// A checkpoint line decodes to the event the scan kept: a stand-in with an id, a type-only stand-in, a stand-in whose
  /// id holds a tab (which keeps its JSON line) and a full event.
  #[test]
  fn checkpoint_lines_decode_to_the_events_the_scan_kept() {
    let lines: Vec<&[u8]> = vec![
      br#"{"id": "a", "type": "assistant", "content": [{"type": "text", "text": "hi"}]}"#,
      br#"{"type": "user", "content": [1]}"#,
      br#"{"id": "tab\tid", "type": "user", "content": [1]}"#,
      br#"{"type": "run_finished", "run_id": "r", "outcome": "success"}"#,
    ];
    let keys = ParseKeys::default();
    let kept: Vec<SharedChatEvent> = lines.iter().map(|line| Arc::new(scan_line(line, &keys).unwrap())).collect();
    let text = crate::fact_checkpoint::encode_events(&kept);
    let decoded: Vec<CompactValue> =
      text.lines().map(|line| checkpoint_line(line.as_bytes(), &keys).unwrap()).collect();
    assert_eq!(decoded.len(), kept.len(), "{text}");
    for (decoded, kept) in decoded.iter().zip(&kept) {
      assert_eq!(decoded, kept.as_ref());
    }
    let written: Vec<&str> = text.lines().collect();
    assert!(written[0].starts_with('S') && written[1].starts_with('T'), "{text}");
    assert!(written[2].starts_with('{') && written[3].starts_with('{'), "{text}");
  }

  fn sorted_strings(values: &std::collections::HashSet<String>) -> Vec<String> {
    let mut list: Vec<String> = values.iter().cloned().collect();
    list.sort();
    list
  }

  /// The task facts as one comparable value. Objects compare by their members, so the collections' iteration order
  /// does not matter.
  fn task_value(task: &crate::task_tree_state::TaskFacts) -> serde_json::Value {
    let events: Vec<serde_json::Value> = task.events_by_id.values().map(|event| event.to_value()).collect();
    let input_candidates: Vec<serde_json::Value> = task.input_candidates.iter().map(|event| event.to_value()).collect();
    let close_events: Vec<serde_json::Value> = task.close_events.iter().map(|event| event.to_value()).collect();
    let close_requests: Vec<serde_json::Value> = task.close_requests.iter().map(|event| event.to_value()).collect();
    let first_close: Vec<serde_json::Value> = {
      let mut pairs: Vec<(&String, serde_json::Value)> =
        task.first_close_by_request.iter().map(|(id, event)| (id, event.to_value())).collect();
      pairs.sort_by(|left, right| left.0.cmp(right.0));
      pairs.into_iter().map(|(id, event)| json!([id, event])).collect()
    };
    let mut delivered: Vec<Vec<String>> =
      task.delivered_reports.iter().map(|(child, event)| vec![child.clone(), event.clone()]).collect();
    delivered.sort();
    json!({
      "task_state": task.task_state,
      "has_user_request": task.has_user_request,
      "run_outcomes": task.run_outcomes,
      "run_outcome_string_none_ids": sorted_strings(&task.run_outcome_string_none_ids),
      "run_finish_positions": task.run_finish_positions,
      "confirmed_input_ids": sorted_strings(&task.confirmed_input_ids),
      "input_candidates": input_candidates,
      "imported_pending_ids": sorted_strings(&task.imported_pending_ids),
      "boundary_index": task.boundary_index,
      "boundary_open": task.boundary_open,
      "close_events": close_events,
      "close_requests": close_requests,
      "close_request_ids": sorted_strings(&task.close_request_ids),
      "first_close_by_request": first_close,
      "delivered_reports": delivered,
      "events": events,
      "covered": task.covered,
    })
  }

  /// The run facts as one comparable value.
  fn runs_value(runs: &RunFacts) -> serde_json::Value {
    let stop_requests: std::collections::BTreeMap<&String, Vec<Option<String>>> = runs
      .stop_requests
      .iter()
      .map(|(run, ids)| {
        let mut ids: Vec<Option<String>> = ids.iter().cloned().collect();
        ids.sort();
        (run, ids)
      })
      .collect();
    json!({"outcomes": runs.outcomes, "stop_requests": stop_requests})
  }

  /// The facts a scan produced: the task facts, the run facts, the newest prompt fact of each scope and the history
  /// they cover.
  fn scanned_value(scan: &ScannedLiveFacts) -> serde_json::Value {
    json!({
      "task": task_value(scan.facts.task().expect("the test histories fold")),
      "runs": runs_value(&scan.runs),
      "subtree": scan.prompt_changes.subtree.as_ref().map(|event| event.to_value()),
      "node": scan.prompt_changes.node.as_ref().map(|event| event.to_value()),
      "covered": scan.covered,
      "archived": scan.archived,
    })
  }

  /// One history whose lines cross every fact the checkpoint must carry: a task boundary, chat content, run
  /// outcomes, a report, a close request and its close.
  async fn append_first_leg(store: &Store) -> u64 {
    for event in [
      json!({"id": "created", "type": "task_created", "actor": "user"}),
      json!({"id": "ask", "type": "user", "content": "do the work"}),
      json!({"id": "said", "type": "assistant", "run_id": "run-1", "content": "working"}),
      json!({"id": "tool-out", "type": "user", "content": [{"type": "tool_result", "content": "done"}]}),
      json!({"id": "finish-1", "type": "run_finished", "run_id": "run-1", "outcome": "success",
             "input_event_ids": ["ask"]}),
      json!({"id": "report", "type": "child_report", "child_session_id": "child", "child_event_id": "close-1"}),
      json!({"id": "subtree-prompt", "type": "prompt_changed", "scope": "subtree", "new_ref": "prompt-a"}),
      json!({"id": "close-ask", "type": "task_close_requested", "request_id": "close-1", "owner_run_id": "run-1"}),
      json!({"id": "close-1", "type": "task_closed", "request_id": "close-1", "outcome": "completed"}),
    ] {
      store.append_chat_event("synthetic", event).await.unwrap();
    }
    std::fs::metadata(store.chat_events_path("synthetic")).unwrap().len()
  }

  /// The events a later boot appends past the checkpoint.
  async fn append_second_leg(store: &Store) {
    for event in [
      json!({"id": "reopen-1", "type": "task_reopened"}),
      json!({"id": "ask-2", "type": "user", "content": "carry on"}),
      json!({"id": "slip", "type": "assistant", "run_id": "run-2", "content": "more"}),
      json!({"id": "finish-2", "type": "run_finished", "run_id": "run-2", "outcome": "failed"}),
      json!({"id": "node-prompt", "type": "prompt_changed", "scope": "node", "new_ref": "prompt-b"}),
    ] {
      store.append_chat_event("synthetic", event).await.unwrap();
    }
  }

  /// A boot whose checkpoint still covers the log reads the appended bytes alone, and the facts it folds equal the
  /// facts a full scan of the same log folds.
  #[tokio::test]
  async fn a_matching_checkpoint_reads_the_appended_bytes_and_folds_a_full_scans_facts() {
    let (home, writer) = make_test_store();
    let checkpoint_leg = append_first_leg(&writer).await;
    let open = || open_test_store(home.path(), Arc::new(crate::EmptyRuntimeEventHooks)).unwrap();
    let first = open();
    let scan = first.scan_live_facts("synthetic").unwrap().expect("nothing writes during the scan");
    assert_eq!(scan.read_bytes(), checkpoint_leg, "a full scan reads the whole log");
    assert!(first.seal_scanned_facts("synthetic", &scan).unwrap());
    let checkpoint_bytes = std::fs::metadata(writer.fact_checkpoint_path("synthetic")).unwrap().len();

    append_second_leg(&writer).await;
    let total = std::fs::metadata(writer.chat_events_path("synthetic")).unwrap().len();
    let resumed = open().scan_live_facts("synthetic").unwrap().expect("nothing writes during the scan");
    assert_eq!(resumed.read_bytes(), total - checkpoint_leg, "the resumed scan reads only the appended bytes");
    assert_eq!(resumed.checkpoint_bytes(), checkpoint_bytes, "the scan reads its checkpoint");
    let full_scan = {
      std::fs::remove_file(writer.fact_checkpoint_path("synthetic")).unwrap();
      let store = open();
      let scan = store.scan_live_facts("synthetic").unwrap().expect("nothing writes during the scan");
      assert_eq!(scan.read_bytes(), total, "the reference scan reads the whole log");
      scan
    };
    assert_eq!(scanned_value(&resumed), scanned_value(&full_scan));
  }

  /// Every checkpoint that does not match its source log leaves the full scan to run: a missing file, a torn file, a
  /// truncated log and a rewritten log.
  #[tokio::test]
  async fn a_missing_torn_truncated_or_rewritten_checkpoint_scans_the_log_whole() {
    let (home, writer) = make_test_store();
    let checkpoint_path = writer.fact_checkpoint_path("synthetic");
    let log_path = writer.chat_events_path("synthetic");
    let open = || open_test_store(home.path(), Arc::new(crate::EmptyRuntimeEventHooks)).unwrap();
    append_first_leg(&writer).await;
    let first = open();
    let scan = first.scan_live_facts("synthetic").unwrap().expect("nothing writes during the scan");
    assert!(scan.checkpoint_bytes() == 0);
    let _ = scan;
    append_second_leg(&writer).await;
    let total = std::fs::metadata(&log_path).unwrap().len();

    // The checkpoint of a full scan of the current log, and that scan's facts as the reference.
    std::fs::remove_file(&checkpoint_path).unwrap();
    let reference = open();
    let reference_scan = reference.scan_live_facts("synthetic").unwrap().expect("nothing writes during the scan");
    let intact = std::fs::read(&checkpoint_path).unwrap();
    let full_facts = scanned_value(&reference_scan);
    let full_scan_of = |store: &ScannedLiveFacts| scanned_value(store);

    // Absent.
    std::fs::remove_file(&checkpoint_path).unwrap();
    let scan = open().scan_live_facts("synthetic").unwrap().expect("nothing writes during the scan");
    assert_eq!(scan.read_bytes(), total, "a missing checkpoint scans the whole log");
    assert_eq!(full_scan_of(&scan), full_facts);

    // Torn: the checkpoint's bytes cut in half.
    std::fs::write(&checkpoint_path, &intact[..intact.len() / 2]).unwrap();
    let scan = open().scan_live_facts("synthetic").unwrap().expect("nothing writes during the scan");
    assert_eq!(scan.read_bytes(), total, "a torn checkpoint scans the whole log");
    assert_eq!(full_scan_of(&scan), full_facts);

    // Truncated log: the file is shorter than the checkpoint's offset.
    let log = std::fs::read(&log_path).unwrap();
    std::fs::write(&log_path, &log[..log.len() / 2]).unwrap();
    std::fs::write(&checkpoint_path, &intact).unwrap();
    let scan = open().scan_live_facts("synthetic").unwrap().expect("nothing writes during the scan");
    std::fs::remove_file(&checkpoint_path).unwrap();
    let truncated = open().scan_live_facts("synthetic").unwrap().expect("nothing writes during the scan");
    assert_eq!(full_scan_of(&scan), full_scan_of(&truncated), "a truncated log folds as a full scan of it does");

    // Rewritten log: the same length, different bytes at its head.
    let mut rewritten = log.clone();
    rewritten[0] = b'[';
    std::fs::write(&log_path, &rewritten).unwrap();
    std::fs::write(&checkpoint_path, &intact).unwrap();
    let scan = open().scan_live_facts("synthetic").unwrap().expect("nothing writes during the scan");
    assert_eq!(scan.read_bytes(), rewritten.len() as u64, "a rewritten log scans whole");
    std::fs::remove_file(&checkpoint_path).unwrap();
    let rewritten_full = open().scan_live_facts("synthetic").unwrap().expect("nothing writes during the scan");
    assert_eq!(full_scan_of(&scan), full_scan_of(&rewritten_full));
  }

  /// A rewritten or rotated archive invalidates the checkpoint, because the facts it holds folded that archive.
  #[tokio::test]
  async fn an_archive_change_invalidates_the_checkpoint() {
    let (home, writer) = make_test_store();
    let session = "archived";
    let path = writer.chat_events_path(session);
    let archive_dir = path.parent().unwrap().join("archives");
    std::fs::create_dir_all(&archive_dir).unwrap();
    let archive = archive_dir.join("chat_events.2026-W44.jsonl");
    std::fs::write(
      &archive,
      concat!(
        "{\"id\": \"created\", \"type\": \"task_created\", \"actor\": \"user\"}\n",
        "{\"id\": \"ask\", \"type\": \"user\", \"content\": \"do the work\"}\n",
      ),
    )
    .unwrap();
    std::fs::write(
      &path,
      "{\"id\": \"finish-1\", \"type\": \"run_finished\", \"run_id\": \"run-1\", \"outcome\": \"success\"}\n",
    )
    .unwrap();
    let mut meta = SessionMetadata::new(session, "seed", TaskProfile::Manager);
    meta.archive_offset = 2;
    std::fs::write(writer.metadata_path(session), meta.to_json_string(&writer.slots, true)).unwrap();
    let checkpoint_path = writer.fact_checkpoint_path(session);
    let open = || open_test_store(home.path(), Arc::new(crate::EmptyRuntimeEventHooks)).unwrap();

    let whole = std::fs::metadata(&path).unwrap().len();
    let first = open();
    let scan = first.scan_live_facts(session).unwrap().expect("nothing writes during the scan");
    let _ = scan;

    let resumed = open().scan_live_facts(session).unwrap().expect("nothing writes during the scan");
    assert_eq!(resumed.read_bytes(), 0, "the appended bytes of an archived session resume from the checkpoint");

    std::fs::write(
      &archive,
      concat!(
        "{\"id\": \"created\", \"type\": \"task_created\", \"actor\": \"user\"}\n",
        "{\"id\": \"ask\", \"type\": \"user\", \"content\": \"different work\"}\n",
      ),
    )
    .unwrap();
    let scan = open().scan_live_facts(session).unwrap().expect("nothing writes during the scan");
    assert_eq!(scan.read_bytes(), whole, "a changed archive scans the log whole");
    assert_eq!(scanned_value(&scan), {
      std::fs::remove_file(&checkpoint_path).unwrap();
      let scan = open().scan_live_facts(session).unwrap().expect("nothing writes during the scan");
      scanned_value(&scan)
    });
  }

  /// A cold node-facts read answers from the checkpoint without loading the log, and folds the same facts a full
  /// scan of the log folds.
  #[tokio::test]
  async fn a_cold_facts_read_answers_from_the_checkpoint_without_a_whole_log_load() {
    let (home, writer) = make_test_store();
    append_first_leg(&writer).await;
    let open = || open_test_store(home.path(), Arc::new(crate::EmptyRuntimeEventHooks)).unwrap();
    let first = open();
    let scan = first.scan_live_facts("synthetic").unwrap().expect("nothing writes during the scan");
    let _ = scan;
    append_second_leg(&writer).await;

    let reading = open();
    let facts = reading.node_facts("synthetic").unwrap();
    assert_eq!(reading.history_work().file_reads, 0, "the checkpoint answers without a whole-log load");
    let checkpointed = task_value(facts.task().unwrap());

    std::fs::remove_file(writer.fact_checkpoint_path("synthetic")).unwrap();
    let reference = open();
    let full = reference.scan_live_facts("synthetic").unwrap().expect("nothing writes during the scan");
    assert!(reference.seal_scanned_facts("synthetic", &full).unwrap());
    let folded = reference.node_facts("synthetic").unwrap();
    assert_eq!(task_value(folded.task().unwrap()), checkpointed);
  }

  /// A cold node-state read folds the fact checkpoint and the bytes past it, and reaches the task state, run facts and
  /// task facts a whole-log fold reaches. A checkpoint that is absent, torn or stale folds nothing, and the read falls
  /// back to the whole-log load that answers the same value.
  #[tokio::test]
  async fn a_cold_node_state_read_folds_the_checkpoint_and_its_mismatches_load_the_whole_log() {
    let (home, writer) = make_test_store();
    let open = || open_test_store(home.path(), Arc::new(crate::EmptyRuntimeEventHooks)).unwrap();
    append_first_leg(&writer).await;
    let first = open();
    let scan = first.scan_live_facts("synthetic").unwrap().expect("nothing writes during the scan");
    assert!(first.seal_scanned_facts("synthetic", &scan).unwrap());
    append_second_leg(&writer).await;
    let checkpoint_path = writer.fact_checkpoint_path("synthetic");
    let log_path = writer.chat_events_path("synthetic");
    let intact = std::fs::read(&checkpoint_path).unwrap();

    // The state, its run facts and its task facts as one comparable value. Each read runs on a fresh store, so its
    // event cache is cold and the whole-log load it may make is counted.
    //
    // The checkpoint keeps each chat-content line as its id and type, so the facts' event index compares by each
    // event's id and type; every decision field, the task state and the run facts must match the whole-log fold.
    let state_value = |store: &Store| {
      let state = store.node_state("synthetic").unwrap();
      let mut task = task_value(&store.facts_of("synthetic").unwrap());
      let id_and_type: Vec<serde_json::Value> = task["events"]
        .as_array()
        .expect("the event index is an array")
        .iter()
        .map(|event| json!([event["id"], event["type"]]))
        .collect();
      task["events"] = json!(id_and_type);
      json!({"state": state.task_state().unwrap(), "runs": runs_value(&state.runs), "task": task})
    };
    let cold_value = |store: &Store| {
      let before = store.history_work().file_reads;
      let value = state_value(store);
      (value, store.history_work().file_reads - before)
    };

    // Reference: no checkpoint, so the whole log answers.
    std::fs::remove_file(&checkpoint_path).unwrap();
    let (reference, whole_log_reads) = cold_value(&open());
    assert_eq!(whole_log_reads, 1, "a missing checkpoint loads the whole log once");

    // Matching: the checkpoint covers the first leg and the read folds the appended second leg onto it.
    std::fs::write(&checkpoint_path, &intact).unwrap();
    let (from_checkpoint, reads) = cold_value(&open());
    assert_eq!(reads, 0, "the checkpoint answers without a whole-log load");
    assert_eq!(from_checkpoint, reference, "the checkpoint's fold equals the whole-log fold");

    // Torn: the checkpoint's bytes cut in half.
    std::fs::write(&checkpoint_path, &intact[..intact.len() / 2]).unwrap();
    let (torn, reads) = cold_value(&open());
    assert_eq!(reads, 1, "a torn checkpoint loads the whole log");
    assert_eq!(torn, reference);

    // Stale: a byte inside the covered prefix was rewritten, with the file's length unchanged.
    let mut rewritten = std::fs::read(&log_path).unwrap();
    let at = rewritten.windows(11).position(|window| window == b"do the work").expect("the first leg holds the text");
    rewritten[at] = b'D';
    std::fs::write(&log_path, &rewritten).unwrap();
    std::fs::write(&checkpoint_path, &intact).unwrap();
    let (stale, reads) = cold_value(&open());
    assert_eq!(reads, 1, "a stale checkpoint loads the whole log");
    std::fs::remove_file(&checkpoint_path).unwrap();
    assert_eq!(stale, state_value(&open()), "the rewritten log folds as a whole-log load of it folds");
  }
}
