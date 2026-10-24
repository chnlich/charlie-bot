use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use charliebot_core::{fsutil, pystr};
use serde_json::Value;

use crate::chat_events::is_chat_events_archive;
use crate::compact::CompactValue;
use crate::metadata::{SessionMetadata, UtcDateTime};
use crate::ndjson;
use crate::read_memos::{BoundedMemo, FileSignature, StatSignatureMemo};
use crate::store::{CachedChatEvents, HistoryWork};
use crate::{ControlGuard, Store, StoreError, StoreResult};

/// One parsed chat-log event: the compact interned-key form of its JSON.
pub type ChatEvent = CompactValue;

/// A reference counted parsed event shared by the live cache and task facts.
pub type SharedChatEvent = Arc<ChatEvent>;

// Match src/infra/ndjson.py:_COUNT_MEMO_LIMIT and its (mtime_ns, size) signatures.
const EVENT_COUNT_MEMO_LIMIT: usize = 64;
const EVENT_TAIL_MEMO_LIMIT: usize = 64;
// Match src/runtime/chat_events.py:_ARCHIVE_MEMO_LIMIT for both archive memos.
const ARCHIVE_MEMO_LIMIT: usize = 16;
// Match src/runtime/chat_events.py:_LIVE_RANGE_MEMO_LIMIT and _WALK_BYTE_BUDGET.
const LIVE_RANGE_MEMO_LIMIT: usize = 4;
const TAIL_WINDOW_SIZE: usize = 512 * 1024;
const LIVE_RANGE_WALK_BYTE_BUDGET: usize = 32 * 1024 * 1024;

#[derive(Clone)]
struct LiveRangeMemo {
  mtime_ns: i128,
  size: u64,
  inode: u64,
  // Python's BoundedMemo returns the cached list by reference. Arc keeps a live-range hit just as cheap.
  lines: Arc<Vec<Option<Arc<ChatEvent>>>>,
  covered_byte_end: usize,
  ends_on_boundary: bool,
  first_line_index: usize,
  covered_byte_start: usize,
}

type ParsedLiveLine = Option<Arc<ChatEvent>>;
type SharedLiveLines = Arc<Vec<ParsedLiveLine>>;
type TailPage = (Arc<Vec<ChatEvent>>, usize, bool);

struct TailLineWalk {
  lines: Vec<Vec<u8>>,
  byte_start: usize,
  ends_on_boundary: bool,
  bytes_read: usize,
}

/// Bounded file-signature memos for the Python-equivalent event readers.
pub(crate) struct EventReadCaches {
  count: StatSignatureMemo<PathBuf, usize>,
  tail: StatSignatureMemo<(PathBuf, usize), TailPage>,
  archive_paths: StatSignatureMemo<PathBuf, Arc<Vec<PathBuf>>>,
  archive_events: StatSignatureMemo<PathBuf, Arc<Vec<SharedChatEvent>>>,
  live_ranges: BoundedMemo<PathBuf, LiveRangeMemo>,
  #[cfg(test)]
  tail_bytes_read: usize,
  #[cfg(test)]
  tail_lines_parsed: usize,
  #[cfg(test)]
  live_range_bytes_read: usize,
  #[cfg(test)]
  live_range_lines_parsed: usize,
  #[cfg(test)]
  archive_file_reads: usize,
  #[cfg(test)]
  archive_dir_walks: usize,
  #[cfg(test)]
  count_bytes_read: usize,
}

impl Default for EventReadCaches {
  fn default() -> Self {
    Self {
      count: StatSignatureMemo::new(EVENT_COUNT_MEMO_LIMIT),
      tail: StatSignatureMemo::new(EVENT_TAIL_MEMO_LIMIT),
      archive_paths: StatSignatureMemo::new(ARCHIVE_MEMO_LIMIT),
      archive_events: StatSignatureMemo::new(ARCHIVE_MEMO_LIMIT),
      live_ranges: BoundedMemo::new(LIVE_RANGE_MEMO_LIMIT),
      #[cfg(test)]
      tail_bytes_read: 0,
      #[cfg(test)]
      tail_lines_parsed: 0,
      #[cfg(test)]
      live_range_bytes_read: 0,
      #[cfg(test)]
      live_range_lines_parsed: 0,
      #[cfg(test)]
      archive_file_reads: 0,
      #[cfg(test)]
      archive_dir_walks: 0,
      #[cfg(test)]
      count_bytes_read: 0,
    }
  }
}

/// Archived event values plus the live history's shared cache, traversed without cloning live events.
pub struct ChatEventHistory {
  archived: Vec<SharedChatEvent>,
  live: Arc<Vec<SharedChatEvent>>,
}

impl ChatEventHistory {
  /// Visit archived facts first and the live log second, as `fact_history` does.
  pub fn iter(&self) -> impl DoubleEndedIterator<Item = &ChatEvent> {
    self.archived.iter().map(Arc::as_ref).chain(self.live.iter().map(Arc::as_ref))
  }

  /// Number of archived and live events in this view.
  pub fn len(&self) -> usize {
    self.archived.len() + self.live.len()
  }

  /// Whether the archive and live history are both empty.
  pub fn is_empty(&self) -> bool {
    self.archived.is_empty() && self.live.is_empty()
  }
}

/// The identity of a live event log on disk: its inode, size and modification time, as Python's
/// `_task_state_fast` keys its memo. An append moves the size, and a rewrite publishes a new inode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FileIdentity {
  inode: u64,
  size: u64,
  mtime_ns: i128,
}

impl FileIdentity {
  /// The identity of the file at `path`, or None when no file is there.
  pub(crate) fn read(path: &Path) -> StoreResult<Option<Self>> {
    use std::os::unix::fs::MetadataExt;
    match fs::metadata(path) {
      Ok(metadata) => Ok(Some(Self {
        inode: metadata.ino(),
        size: metadata.size(),
        mtime_ns: i128::from(metadata.mtime()) * 1_000_000_000 + i128::from(metadata.mtime_nsec()),
      })),
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
      Err(error) => Err(StoreError::new(format!("{}: {error}", path.display()))),
    }
  }
}

/// A node's live history as the event cache holds it: the cached list and its generation, or, while the cache is cold,
/// the identity of the log file.
pub(crate) enum LiveHistory {
  Cached { generation: u64, identity: Arc<()>, events: Arc<Vec<SharedChatEvent>> },
  Cold(Option<FileIdentity>),
}

pub(crate) type LiveEventSnapshot = (u64, Arc<()>, Arc<Vec<SharedChatEvent>>);

/// A stable cache identity plus either the full event list or its newly appended suffix.
pub struct ChatEventsDelta {
  pub generation: u64,
  pub start_index: usize,
  pub event_count: usize,
  pub events: Arc<Vec<SharedChatEvent>>,
}

/// A selected event range that borrows the cached list when no archive is present.
pub struct SharedChatEventRange {
  pub events: Arc<Vec<SharedChatEvent>>,
  pub range: std::ops::Range<usize>,
}

impl Store {
  /// Append one event, injecting its id and Python `datetime.isoformat()` timestamp when absent.
  pub async fn append_chat_event(&self, session_id: &str, mut event: Value) -> StoreResult<ChatEvent> {
    let object = event.as_object_mut().ok_or_else(|| StoreError::new("chat event must be a JSON object"))?;
    if !object.contains_key("id") {
      object.insert("id".to_string(), Value::String(uuid::Uuid::new_v4().to_string()));
    }
    if !object.contains_key("timestamp") {
      object.insert("timestamp".to_string(), Value::String(python_event_timestamp(UtcDateTime::now())));
    }
    let cache_entry = self.events_cache_entry(session_id);
    let mut cache = cache_entry.lock().expect("session event cache mutex is not poisoned");
    let path = self.chat_events_path(session_id);
    #[cfg(test)]
    if let Some(hook) = self.event_append_hook.lock().expect("event append hook mutex is not poisoned").clone() {
      hook();
    }
    #[cfg(any(test, feature = "write-faults"))]
    self.fail_chat_append(session_id, &event, &path)?;
    ndjson::append(&path, &event)?;
    let compact = ChatEvent::from_value(&event);
    if let Some(cached) = cache.as_mut() {
      Arc::make_mut(&mut cached.events).push(Arc::new(compact.clone()));
      self
        .run_facts_memo
        .lock()
        .expect("run facts memo mutex is not poisoned")
        .refresh_events(&cached.identity, Arc::clone(&cached.events));
    }
    Ok(compact)
  }

  /// Append one control fact while the caller holds the tree-wide control lock, then notify the sidebar of its type.
  pub async fn append_control_fact(
    &self,
    guard: &ControlGuard,
    session_id: &str,
    event: Value,
  ) -> StoreResult<ChatEvent> {
    self.require_control_guard(guard)?;
    let event = self.append_chat_event(session_id, event).await?;
    let fact_type = event.get("type").and_then(CompactValue::as_str);
    self.notify_task_tree_changed(session_id, fact_type).await;
    Ok(event)
  }

  /// Read the live event log, using the in-memory parsed-event cache after the first read.
  pub fn load_chat_events_sync(&self, session_id: &str) -> StoreResult<Vec<ChatEvent>> {
    let events = self.load_chat_events_shared_sync(session_id)?;
    self.history_work.owned_event_clones.fetch_add(events.len() as u64, Ordering::Relaxed);
    Ok(events.iter().map(|event| event.as_ref().clone()).collect())
  }

  /// Read the cached event list as an immutable shared snapshot.
  pub fn load_chat_events_shared_sync(&self, session_id: &str) -> StoreResult<Arc<Vec<SharedChatEvent>>> {
    let cache_entry = self.events_cache_entry(session_id);
    let mut cache = cache_entry.lock().expect("session event cache mutex is not poisoned");
    self.load_chat_events_shared_locked(session_id, &mut cache)
  }

  /// The cached live events and their cache generation, read under one lock so the pair is consistent.
  pub(crate) fn live_events_with_generation(&self, session_id: &str) -> StoreResult<LiveEventSnapshot> {
    let cache_entry = self.events_cache_entry(session_id);
    let mut cache = cache_entry.lock().expect("session event cache mutex is not poisoned");
    let events = self.load_chat_events_shared_locked(session_id, &mut cache)?;
    let cached = cache.as_ref().expect("event cache entry was loaded above");
    Ok((cached.generation, Arc::clone(&cached.identity), events))
  }

  /// Load the live events for a recovery fold, checking cancellation between physical NDJSON lines.
  pub(crate) fn live_events_with_generation_until(
    &self,
    session_id: &str,
    should_stop: &mut impl FnMut() -> bool,
  ) -> StoreResult<Option<LiveEventSnapshot>> {
    let cache_entry = self.events_cache_entry(session_id);
    let mut cache = cache_entry.lock().expect("session event cache mutex is not poisoned");
    let Some(events) = self.load_chat_events_shared_locked_until(session_id, &mut cache, should_stop)? else {
      return Ok(None);
    };
    let cached = cache.as_ref().expect("event cache entry was loaded above");
    Ok(Some((cached.generation, Arc::clone(&cached.identity), events)))
  }

  /// The cached live events, or the log file's identity while the cache is cold; a cold cache stays cold.
  ///
  /// The identity is read under the session lock that every append holds, so it names a file that holds every event
  /// appended before the read.
  pub(crate) fn live_history(&self, session_id: &str) -> StoreResult<LiveHistory> {
    let cache_entry = self.events_cache_entry(session_id);
    let cache = cache_entry.lock().expect("session event cache mutex is not poisoned");
    match cache.as_ref() {
      Some(cached) => {
        #[cfg(test)]
        self.history_reads.fetch_add(1, Ordering::Relaxed);
        Ok(LiveHistory::Cached {
          generation: cached.generation,
          identity: Arc::clone(&cached.identity),
          events: Arc::clone(&cached.events),
        })
      }
      None => Ok(LiveHistory::Cold(FileIdentity::read(&self.chat_events_path(session_id))?)),
    }
  }

  /// Work done on node histories since the store opened.
  pub fn history_work(&self) -> HistoryWork {
    HistoryWork {
      file_reads: self.history_work.file_reads.load(Ordering::Relaxed),
      full_folds: self.history_work.full_folds.load(Ordering::Relaxed),
      folded_events: self.history_work.folded_events.load(Ordering::Relaxed),
      owned_event_clones: self.history_work.owned_event_clones.load(Ordering::Relaxed),
      event_history_views: self.history_work.event_history_views.load(Ordering::Relaxed),
      node_state_full_folds: self.history_work.node_state_full_folds.load(Ordering::Relaxed),
      node_state_folded_events: self.history_work.node_state_folded_events.load(Ordering::Relaxed),
      run_event_bytes_read: self.history_work.run_event_bytes_read.load(Ordering::Relaxed),
    }
  }

  /// Read the live event cache from `covered` when its identity still matches, otherwise from the beginning.
  pub fn load_chat_events_delta_sync(
    &self,
    session_id: &str,
    known_generation: Option<u64>,
    covered: usize,
  ) -> StoreResult<ChatEventsDelta> {
    let cache_entry = self.events_cache_entry(session_id);
    let mut cache = cache_entry.lock().expect("session event cache mutex is not poisoned");
    let _snapshot = self.load_chat_events_shared_locked(session_id, &mut cache)?;
    let cached = cache.as_ref().expect("event cache entry was loaded above");
    let start_index =
      if known_generation == Some(cached.generation) && covered <= cached.events.len() { covered } else { 0 };
    Ok(ChatEventsDelta {
      generation: cached.generation,
      start_index,
      event_count: cached.events.len(),
      events: Arc::clone(&cached.events),
    })
  }

  /// Read archived facts and the live events, sharing the live cache when no archive is present.
  pub fn load_all_facts_shared_sync(&self, session_id: &str) -> StoreResult<Arc<Vec<SharedChatEvent>>> {
    let cache_entry = self.events_cache_entry(session_id);
    let mut cache = cache_entry.lock().expect("session event cache mutex is not poisoned");
    let live = self.load_chat_events_shared_locked(session_id, &mut cache)?;
    let archive_offset = self.read_archive_offset(session_id)?;
    if archive_offset == 0 {
      return Ok(live);
    }
    let mut events = self.load_archive_range(session_id, 0, archive_offset)?;
    events.extend(live.iter().cloned());
    Ok(Arc::new(events))
  }

  /// Read archived facts and retain the live event cache as a shared segment.
  ///
  /// Task paths that only scan history use this view so an archived prefix does not force a deep clone of every live
  /// JSON value. Python's `fact_history` returns shared event dictionaries in both its cached and concatenated cases.
  pub fn load_all_facts_view_sync(&self, session_id: &str) -> StoreResult<ChatEventHistory> {
    self.history_work.event_history_views.fetch_add(1, Ordering::Relaxed);
    let cache_entry = self.events_cache_entry(session_id);
    let mut cache = cache_entry.lock().expect("session event cache mutex is not poisoned");
    let live = self.load_chat_events_shared_locked(session_id, &mut cache)?;
    let archive_offset = self.read_archive_offset(session_id)?;
    let archived =
      if archive_offset == 0 { Vec::new() } else { self.load_archive_range(session_id, 0, archive_offset)? };
    Ok(ChatEventHistory { archived, live })
  }

  /// Read one NDJSON file's parsed events in the compact form, blank and malformed lines skipped.
  ///
  /// This is `src/runtime/worker_transcript.py::_read_events`'s parse for the run event logs, which
  /// the worker transcript folds in place rather than retaining a second `Value` tree of. The parsed
  /// byte count rides the history-work census, the count the transcript latency tests pin.
  pub fn read_ndjson_compact(&self, path: &Path) -> StoreResult<Vec<ChatEvent>> {
    let events = ndjson::read_file_compact(path)?;
    let bytes = fs::metadata(path).map(|metadata| metadata.size()).unwrap_or(0);
    self.history_work.run_event_bytes_read.fetch_add(bytes, Ordering::Relaxed);
    Ok(events)
  }

  /// Read the latest live-log events without priming the event cache. It follows
  /// `src/infra/ndjson.py:332-381`: key the page by `(path, limit)` and `(mtime_ns, size)`, then seek from EOF in the
  /// same 512 KiB window.
  pub fn load_chat_events_tail(&self, session_id: &str, limit: usize) -> StoreResult<(Vec<ChatEvent>, usize, bool)> {
    let path = self.chat_events_path(session_id);
    let Some((metadata, signature)) = file_signature(&path)? else { return Ok((Vec::new(), 0, false)) };
    let key = (path.clone(), limit);
    if let Some((events, total, has_more)) =
      self.event_read_caches.lock().expect("event read cache mutex is not poisoned").tail.fresh(&key, signature)
    {
      return Ok((events.as_ref().clone(), total, has_more));
    }
    let total = self.count_chat_event_lines(&path, signature)?;
    let has_more = total > limit;
    if limit == 0 || total == 0 {
      self.event_read_caches.lock().expect("event read cache mutex is not poisoned").tail.record(
        key,
        signature,
        (Arc::new(Vec::new()), total, has_more),
      );
      return Ok((Vec::new(), total, has_more));
    }
    let take = limit.min(total);
    let (lines, bytes_read) = read_ndjson_tail_lines(&path, metadata.len(), take)?;
    let events: Vec<ChatEvent> = lines.iter().filter_map(|line| ndjson::parse_line_compact(line)).collect();
    let events = Arc::new(events);
    {
      let mut caches = self.event_read_caches.lock().expect("event read cache mutex is not poisoned");
      #[cfg(test)]
      {
        caches.tail_bytes_read += bytes_read;
        caches.tail_lines_parsed += lines.len();
      }
      caches.tail.record(key, signature, (Arc::clone(&events), total, has_more));
    }
    #[cfg(not(test))]
    let _ = bytes_read;
    Ok((events.as_ref().clone(), total, has_more))
  }

  /// Read all durable facts, archived segments first and the live log last, composed the way
  /// `task_sessions.fact_history` composes it: the archived count is the total event count minus
  /// the live event count, the archived events are the range read from 0 to that count, and
  /// archive files beyond that count stay unread.
  pub fn load_all_facts(&self, session_id: &str) -> StoreResult<Vec<ChatEvent>> {
    let live = self.load_chat_events_sync(session_id)?;
    let total = self.get_chat_event_count_sync(session_id)?;
    let archived_count = total.saturating_sub(live.len());
    if archived_count == 0 {
      return Ok(live);
    }
    let mut events = self.load_chat_events_range(session_id, 0, archived_count)?.0.into_iter().collect::<Vec<_>>();
    events.extend(live);
    Ok(events)
  }

  /// Read the global event-index range `[start, end)` and say whether earlier indices exist.
  ///
  /// Global indices are `archive_offset + line_in_live_file`. With a positive archive offset the live
  /// half numbers physical lines — a blank or a malformed line consumes its index and parses to nothing,
  /// the same inconsistency `ndjson.parse_ndjson_range`'s islice shows on the unarchived cold path. An
  /// unarchived warm cache instead numbers its own parsed-event indices, matching Python's cache branch.
  pub fn load_chat_events_range(
    &self,
    session_id: &str,
    start: usize,
    end: usize,
  ) -> StoreResult<(Vec<ChatEvent>, bool)> {
    if end <= start {
      return Ok((Vec::new(), start > 0));
    }
    let archive_offset = self.read_archive_offset(session_id)?;
    if end <= archive_offset {
      return Ok((
        self.load_archive_range(session_id, start, end)?.into_iter().map(|event| event.as_ref().clone()).collect(),
        start > 0,
      ));
    }
    if start >= archive_offset {
      let rel_start = start - archive_offset;
      let rel_end = end - archive_offset;
      let events = if archive_offset > 0 {
        let (lines, first_line) = self.live_range_lines(session_id, rel_start)?;
        slice_live_lines(&lines, rel_start.saturating_sub(first_line), rel_end.saturating_sub(first_line))
      } else {
        match self.cached_chat_events(session_id) {
          Some(cached) => cached
            .iter()
            .skip(rel_start)
            .take(rel_end.saturating_sub(rel_start))
            .map(|event| event.as_ref().clone())
            .collect(),
          None => {
            let lines = self.live_lines_newline_split(session_id)?;
            slice_live_lines(&lines, rel_start, rel_end)
          }
        }
      };
      return Ok((events, start > 0));
    }
    let mut out = self
      .load_archive_range(session_id, start, archive_offset)?
      .into_iter()
      .map(|event| event.as_ref().clone())
      .collect::<Vec<_>>();
    let live_end = end - archive_offset;
    let (lines, first_line) = self.live_range_lines(session_id, 0)?;
    out.extend(slice_live_lines(&lines, first_line, live_end.saturating_sub(first_line)));
    Ok((out, start > 0))
  }

  /// Read `[start, end)` without copying cached live events; archived histories retain the existing range reader.
  pub fn load_chat_events_shared_range_sync(
    &self,
    session_id: &str,
    start: usize,
    end: usize,
  ) -> StoreResult<SharedChatEventRange> {
    if self.read_archive_offset(session_id)? > 0 {
      let (events, _) = self.load_chat_events_range(session_id, start, end)?;
      let end = events.len();
      return Ok(SharedChatEventRange { events: Arc::new(events.into_iter().map(Arc::new).collect()), range: 0..end });
    }
    match self.live_history(session_id)? {
      LiveHistory::Cached { events, .. } => {
        let range_start = start.min(events.len());
        let range_end = end.min(events.len()).max(range_start);
        Ok(SharedChatEventRange { events, range: range_start..range_end })
      }
      LiveHistory::Cold(_) => {
        let (events, _) = self.load_chat_events_range(session_id, start, end)?;
        let end = events.len();
        Ok(SharedChatEventRange { events: Arc::new(events.into_iter().map(Arc::new).collect()), range: 0..end })
      }
    }
  }

  /// Read one archive's parsed events. This uses `src/runtime/chat_events.py:354-392`'s directory/file stat memos and
  /// 16-file bound. An unreadable file logs and contributes nothing, the pre-memo reader's behavior on open failure in
  /// `_archive_file_events`.
  fn read_archive_file(&self, path: &Path, session_id: &str) -> Arc<Vec<SharedChatEvent>> {
    let metadata = match fs::metadata(path) {
      Ok(metadata) => metadata,
      Err(error) => {
        tracing::debug!(session_id, path = %path.display(), error = %error, "archive_read_failed");
        return Arc::new(Vec::new());
      }
    };
    let signature = FileSignature::from_metadata(&metadata);
    if let Some(events) = self
      .event_read_caches
      .lock()
      .expect("event read cache mutex is not poisoned")
      .archive_events
      .fresh(&path.to_path_buf(), signature)
    {
      return events;
    }
    #[cfg(test)]
    {
      self.event_read_caches.lock().expect("event read cache mutex is not poisoned").archive_file_reads += 1;
    }
    match ndjson::read_file_compact(path) {
      Ok(events) => {
        let events = Arc::new(events.into_iter().map(Arc::new).collect());
        self.event_read_caches.lock().expect("event read cache mutex is not poisoned").archive_events.record(
          path.to_path_buf(),
          signature,
          Arc::clone(&events),
        );
        events
      }
      Err(error) => {
        tracing::debug!(session_id, path = %path.display(), error = %error, "archive_read_failed");
        Arc::new(Vec::new())
      }
    }
  }

  /// Read one archive file while allowing recovery to discard a cold read when startup is stopped.
  fn read_archive_file_until(
    &self,
    path: &Path,
    session_id: &str,
    should_stop: &mut impl FnMut() -> bool,
  ) -> Option<Arc<Vec<SharedChatEvent>>> {
    if should_stop() {
      return None;
    }
    let metadata = match fs::metadata(path) {
      Ok(metadata) => metadata,
      Err(error) => {
        tracing::debug!(session_id, path = %path.display(), error = %error, "archive_read_failed");
        return Some(Arc::new(Vec::new()));
      }
    };
    let signature = FileSignature::from_metadata(&metadata);
    if let Some(events) = self
      .event_read_caches
      .lock()
      .expect("event read cache mutex is not poisoned")
      .archive_events
      .fresh(&path.to_path_buf(), signature)
    {
      return Some(events);
    }
    #[cfg(test)]
    {
      self.event_read_caches.lock().expect("event read cache mutex is not poisoned").archive_file_reads += 1;
    }
    match ndjson::read_file_compact_until(path, should_stop) {
      Ok(Some(events)) => {
        let events = Arc::new(events.into_iter().map(Arc::new).collect());
        self.event_read_caches.lock().expect("event read cache mutex is not poisoned").archive_events.record(
          path.to_path_buf(),
          signature,
          Arc::clone(&events),
        );
        Some(events)
      }
      Ok(None) => None,
      Err(error) => {
        tracing::debug!(session_id, path = %path.display(), error = %error, "archive_read_failed");
        Some(Arc::new(Vec::new()))
      }
    }
  }

  /// Read the parsed events at global indices `[start, end)` from the archive files in
  /// chronological order, stopping once the covered extent passes `end`.
  pub(crate) fn load_archive_range(
    &self,
    session_id: &str,
    start: usize,
    end: usize,
  ) -> StoreResult<Vec<SharedChatEvent>> {
    let mut events = Vec::new();
    let mut pos = 0usize;
    for path in self.archive_paths(session_id)?.iter() {
      let file_events = self.read_archive_file(path, session_id);
      let file_end = pos + file_events.len();
      if pos < end && file_end > start {
        events.extend(file_events[start.max(pos) - pos..end.min(file_end) - pos].iter().cloned());
      }
      pos = file_end;
      if pos >= end {
        break;
      }
    }
    Ok(events)
  }

  /// Read an archive range for recovery, returning `None` when cancellation interrupts a cold file read.
  pub(crate) fn load_archive_range_until(
    &self,
    session_id: &str,
    start: usize,
    end: usize,
    should_stop: &mut impl FnMut() -> bool,
  ) -> StoreResult<Option<Vec<SharedChatEvent>>> {
    if should_stop() {
      return Ok(None);
    }
    let mut events = Vec::new();
    let mut pos = 0usize;
    for path in self.archive_paths(session_id)?.iter() {
      let Some(file_events) = self.read_archive_file_until(path, session_id, should_stop) else {
        return Ok(None);
      };
      let file_end = pos + file_events.len();
      if pos < end && file_end > start {
        events.extend(file_events[start.max(pos) - pos..end.min(file_end) - pos].iter().cloned());
      }
      pos = file_end;
      if pos >= end {
        break;
      }
    }
    Ok(Some(events))
  }

  /// Read the live log as parsed events with one slot per physical line. This follows
  /// `src/runtime/chat_events.py:395-451`: four path-keyed entries, same-inode append extension, backward 512 KiB
  /// walks, and the same 32 MiB fallback bound. It splits the way Python's
  /// universal-newline reader splits (`\r\n` and a lone `\r` terminate a line too). A read
  /// failure logs and answers an empty page, the `_live_range_lines` behavior on an OSError.
  fn live_range_lines(&self, session_id: &str, rel_start: usize) -> StoreResult<(SharedLiveLines, usize)> {
    let path = self.chat_events_path(session_id);
    let (metadata, signature) = match file_signature(&path) {
      Ok(Some(stat)) => stat,
      Ok(None) => return Ok((Arc::new(Vec::new()), rel_start)),
      Err(error) => {
        tracing::debug!(path = %path.display(), error = %error, "live_range_read_failed");
        return Ok((Arc::new(Vec::new()), rel_start));
      }
    };
    let size = metadata.len() as usize;
    let inode = metadata.ino();
    let cached = self.event_read_caches.lock().expect("event read cache mutex is not poisoned").live_ranges.get(&path);
    if let Some(mut entry) = cached {
      if (entry.mtime_ns != signature.mtime_ns || entry.size != signature.size)
        && entry.inode == inode
        && size >= entry.covered_byte_end
        && entry.ends_on_boundary
      {
        match read_live_range_suffix(&path, entry.covered_byte_end, size) {
          Ok(bytes) => {
            let suffix = parse_universal_newline_lines(&path, &bytes)?;
            #[cfg(test)]
            {
              let mut caches = self.event_read_caches.lock().expect("event read cache mutex is not poisoned");
              caches.live_range_bytes_read += bytes.len();
              caches.live_range_lines_parsed += suffix.len();
            }
            let mut lines = entry.lines.as_ref().clone();
            lines.extend(suffix);
            entry.lines = Arc::new(lines);
            entry.mtime_ns = signature.mtime_ns;
            entry.size = signature.size;
            entry.covered_byte_end += bytes.len();
            entry.ends_on_boundary = bytes.is_empty() || bytes.ends_with(b"\n") || bytes.ends_with(b"\r");
            self
              .event_read_caches
              .lock()
              .expect("event read cache mutex is not poisoned")
              .live_ranges
              .store(path.clone(), entry.clone());
          }
          Err(error) => {
            tracing::debug!(path = %path.display(), error = %error, "live_range_read_failed");
            return Ok((Arc::new(Vec::new()), rel_start));
          }
        }
      }
      if entry.mtime_ns == signature.mtime_ns && entry.size == signature.size {
        if rel_start >= entry.first_line_index {
          return Ok((Arc::clone(&entry.lines), entry.first_line_index));
        }
        if rel_start > 0 {
          let count = entry.first_line_index - rel_start;
          if let Some(walk) = walk_tail_line_texts(&path, entry.covered_byte_start, count)? {
            let prefix = parse_text_lines(&path, walk.lines)?;
            #[cfg(test)]
            {
              let mut caches = self.event_read_caches.lock().expect("event read cache mutex is not poisoned");
              caches.live_range_bytes_read += walk.bytes_read;
              caches.live_range_lines_parsed += prefix.len();
            }
            #[cfg(not(test))]
            let _ = walk.bytes_read;
            let mut lines = prefix;
            lines.extend(entry.lines.iter().cloned());
            let covered = LiveRangeMemo {
              first_line_index: rel_start,
              covered_byte_start: walk.byte_start,
              lines: Arc::new(lines),
              ..entry
            };
            let result = Arc::clone(&covered.lines);
            self
              .event_read_caches
              .lock()
              .expect("event read cache mutex is not poisoned")
              .live_ranges
              .store(path.clone(), covered);
            return Ok((result, rel_start));
          }
        }
      }
    }

    if rel_start > 0 {
      let total = self.count_chat_event_lines(&path, signature)?;
      let after = file_signature(&path)?;
      if after.is_some_and(|(_, after_signature)| after_signature == signature) {
        let count = total.saturating_sub(rel_start);
        if let Some(walk) = walk_tail_line_texts(&path, size, count)? {
          let lines = Arc::new(parse_text_lines(&path, walk.lines)?);
          #[cfg(test)]
          {
            let mut caches = self.event_read_caches.lock().expect("event read cache mutex is not poisoned");
            caches.live_range_bytes_read += walk.bytes_read;
            caches.live_range_lines_parsed += lines.len();
          }
          #[cfg(not(test))]
          let _ = walk.bytes_read;
          let entry = LiveRangeMemo {
            mtime_ns: signature.mtime_ns,
            size: signature.size,
            inode,
            lines: lines.clone(),
            covered_byte_end: size,
            ends_on_boundary: walk.ends_on_boundary,
            first_line_index: rel_start,
            covered_byte_start: walk.byte_start,
          };
          self.event_read_caches.lock().expect("event read cache mutex is not poisoned").live_ranges.store(path, entry);
          return Ok((lines, rel_start));
        }
      }
    }

    let bytes = match fs::read(&path) {
      Ok(bytes) => bytes,
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((Arc::new(Vec::new()), rel_start)),
      Err(error) => {
        tracing::debug!(path = %path.display(), error = %error, "live_range_read_failed");
        return Ok((Arc::new(Vec::new()), rel_start));
      }
    };
    let lines = Arc::new(parse_universal_newline_lines(&path, &bytes)?);
    #[cfg(test)]
    {
      let mut caches = self.event_read_caches.lock().expect("event read cache mutex is not poisoned");
      caches.live_range_bytes_read += bytes.len();
      caches.live_range_lines_parsed += lines.len();
    }
    let ends_on_boundary = bytes.is_empty() || bytes.ends_with(b"\n") || bytes.ends_with(b"\r");
    let entry = LiveRangeMemo {
      mtime_ns: signature.mtime_ns,
      size: signature.size,
      inode,
      lines: Arc::clone(&lines),
      covered_byte_end: bytes.len(),
      ends_on_boundary,
      first_line_index: 0,
      covered_byte_start: 0,
    };
    self.event_read_caches.lock().expect("event read cache mutex is not poisoned").live_ranges.store(path, entry);
    Ok((lines, 0))
  }

  /// Read the live log as parsed events with one slot per physical line, split on `\n` only —
  /// the domain `ndjson.parse_ndjson_range` numbers on the unarchived cold path.
  fn live_lines_newline_split(&self, session_id: &str) -> StoreResult<Vec<Option<Arc<ChatEvent>>>> {
    let path = self.chat_events_path(session_id);
    let bytes = match fs::read(&path) {
      Ok(bytes) => bytes,
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
      Err(error) => return Err(StoreError::new(format!("{}: {error}", path.display()))),
    };
    Ok(bytes.split(|byte| *byte == b'\n').map(ndjson::parse_line_compact).map(|event| event.map(Arc::new)).collect())
  }

  /// Find the first event with `id` in archived and live history.
  pub fn find_event_by_id(&self, session_id: &str, event_id: &str) -> StoreResult<Option<ChatEvent>> {
    Ok(
      self
        .load_all_facts(session_id)?
        .into_iter()
        .find(|event| event.get("id").and_then(CompactValue::as_str) == Some(event_id)),
    )
  }

  /// Return the total event-index extent, using parsed cache length or physical live lines.
  pub fn get_chat_event_count_sync(&self, session_id: &str) -> StoreResult<usize> {
    let offset = self.read_archive_offset(session_id)?;
    if let Some(events) = self.cached_chat_events(session_id) {
      return Ok(offset + events.len());
    }
    let path = self.chat_events_path(session_id);
    let Some((_, signature)) = file_signature(&path)? else { return Ok(offset) };
    Ok(offset + self.count_chat_event_lines(&path, signature)?)
  }

  /// Count physical lines with `src/infra/ndjson.py:312-328`'s 64-file `(mtime_ns, size)` memo.
  fn count_chat_event_lines(&self, path: &Path, signature: FileSignature) -> StoreResult<usize> {
    if let Some(count) = self
      .event_read_caches
      .lock()
      .expect("event read cache mutex is not poisoned")
      .count
      .fresh(&path.to_path_buf(), signature)
    {
      return Ok(count);
    }
    let mut file = match fs::File::open(path) {
      Ok(file) => file,
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
      Err(error) => return Err(StoreError::new(format!("{}: {error}", path.display()))),
    };
    let mut chunk = vec![0; 1024 * 1024];
    let mut total = 0usize;
    #[cfg(test)]
    let mut bytes_read = 0usize;
    let mut last_byte = None;
    loop {
      let read = file.read(&mut chunk).map_err(|error| StoreError::new(format!("{}: {error}", path.display())))?;
      if read == 0 {
        break;
      }
      #[cfg(test)]
      {
        bytes_read += read;
      }
      let bytes = &chunk[..read];
      total += bytes.iter().filter(|byte| **byte == b'\n').count();
      last_byte = bytes.last().copied();
    }
    if last_byte.is_some_and(|byte| byte != b'\n') {
      total += 1;
    }
    {
      let mut caches = self.event_read_caches.lock().expect("event read cache mutex is not poisoned");
      #[cfg(test)]
      {
        caches.count_bytes_read += bytes_read;
      }
      caches.count.record(path.to_path_buf(), signature, total);
    }
    Ok(total)
  }

  /// Discard the parsed live-log cache after a rewrite.
  pub fn clear_chat_events_cache(&self, session_id: &str) {
    let cache_entry = self.events_cache.lock().expect("event cache mutex is not poisoned").get(session_id).cloned();
    let Some(cache_entry) = cache_entry else { return };
    let mut cache = cache_entry.lock().expect("session event cache mutex is not poisoned");
    *cache = None;
  }

  /// Drop a node's parsed live events and folded facts after a node pass releases its local references.
  ///
  /// The per-session cache entry stays in the map: an append or cold read that holds its lock keeps its exclusion, so
  /// the next read sees every event that the file holds. The node's task state stays keyed by the log file's identity
  /// when a fold covers the whole cached list (see [`Store::seal_node_state`]). The run-facts memo keeps the current
  /// event list under its bounded identity key, like Python, so a later reload has a new key.
  pub fn evict_recovery_history(&self, session_id: &str) {
    let cache_entry = self.events_cache.lock().expect("event cache mutex is not poisoned").get(session_id).cloned();
    match cache_entry {
      Some(cache_entry) => {
        // The seal runs under the session lock, so no reload or append moves the file between the take and the seal.
        let mut cache = cache_entry.lock().expect("session event cache mutex is not poisoned");
        let evicted = cache.take().and_then(|cached| match FileIdentity::read(&self.chat_events_path(session_id)) {
          Ok(identity) => {
            identity.map(|identity| (cached.generation, cached.events.len(), identity, Arc::clone(&cached.identity)))
          }
          Err(error) => {
            // The next read of this node reads the file again and returns the error.
            tracing::warn!(session_id = %session_id, error = %error, "node_state_seal_skipped");
            None
          }
        });
        self.seal_node_state(session_id, evicted.clone());
        self.seal_node_facts(
          session_id,
          evicted.map(|(generation, covered, identity, _)| (generation, covered, identity)),
        );
      }
      None => {
        self.seal_node_state(session_id, None);
        self.seal_node_facts(session_id, None);
      }
    }
  }

  fn load_chat_events_shared_locked(
    &self,
    session_id: &str,
    cache: &mut Option<CachedChatEvents>,
  ) -> StoreResult<Arc<Vec<SharedChatEvent>>> {
    #[cfg(test)]
    self.history_reads.fetch_add(1, Ordering::Relaxed);
    if let Some(cached) = cache.as_ref() {
      return Ok(Arc::clone(&cached.events));
    }
    let events =
      Arc::new(self.read_chat_events_file(&self.chat_events_path(session_id))?.into_iter().map(Arc::new).collect());
    *cache = Some(CachedChatEvents {
      generation: self.next_events_cache_generation(),
      identity: Arc::new(()),
      events: Arc::clone(&events),
    });
    Ok(events)
  }

  fn load_chat_events_shared_locked_until(
    &self,
    session_id: &str,
    cache: &mut Option<CachedChatEvents>,
    should_stop: &mut impl FnMut() -> bool,
  ) -> StoreResult<Option<Arc<Vec<SharedChatEvent>>>> {
    #[cfg(test)]
    self.history_reads.fetch_add(1, Ordering::Relaxed);
    if let Some(cached) = cache.as_ref() {
      return Ok(Some(Arc::clone(&cached.events)));
    }
    let Some(events) = self.read_chat_events_file_until(&self.chat_events_path(session_id), should_stop)? else {
      return Ok(None);
    };
    let events = Arc::new(events.into_iter().map(Arc::new).collect());
    *cache = Some(CachedChatEvents {
      generation: self.next_events_cache_generation(),
      identity: Arc::new(()),
      events: Arc::clone(&events),
    });
    Ok(Some(events))
  }

  fn read_chat_events_file_until(
    &self,
    path: &Path,
    should_stop: &mut impl FnMut() -> bool,
  ) -> StoreResult<Option<Vec<ChatEvent>>> {
    self.history_work.file_reads.fetch_add(1, Ordering::Relaxed);
    #[cfg(test)]
    if let Some(hook) = self.event_read_hook.lock().expect("event read hook mutex is not poisoned").clone() {
      hook();
    }
    ndjson::read_file_compact_until(path, should_stop)
  }

  fn read_chat_events_file(&self, path: &Path) -> StoreResult<Vec<ChatEvent>> {
    self.history_work.file_reads.fetch_add(1, Ordering::Relaxed);
    #[cfg(test)]
    if let Some(hook) = self.event_read_hook.lock().expect("event read hook mutex is not poisoned").clone() {
      hook();
    }
    ndjson::read_file_compact(path)
  }

  pub(crate) fn cached_chat_events(&self, session_id: &str) -> Option<Arc<Vec<SharedChatEvent>>> {
    let cache_entry = self.events_cache.lock().expect("event cache mutex is not poisoned").get(session_id).cloned()?;
    let cache = cache_entry.lock().expect("session event cache mutex is not poisoned");
    cache.as_ref().map(|cached| Arc::clone(&cached.events))
  }

  /// Archive the oldest consecutive events before `cutoff` and advance metadata `archive_offset`.
  ///
  /// The live log is decoded as strict UTF-8 before any write, as Python's `open(encoding="utf-8")` does. Invalid
  /// UTF-8, a non-object line, or a truthy non-string `timestamp` before the split point fails before any write, so
  /// the live log, the archive and `archive_offset` stay unchanged.
  pub async fn archive_events_before(&self, session_id: &str, cutoff: UtcDateTime) -> StoreResult<Option<PathBuf>> {
    let live_path = self.chat_events_path(session_id);
    let raw = match fs::read(&live_path) {
      Ok(raw) => raw,
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
      Err(error) => return Err(StoreError::new(format!("{}: {error}", live_path.display()))),
    };
    let text = std::str::from_utf8(&raw).map_err(|error| {
      StoreError::new(format!("{}: invalid UTF-8 at byte {}", live_path.display(), error.valid_up_to()))
    })?;
    // Universal newlines: `\r\n` and a lone `\r` end a line as `\n` does. `split_terminator` keeps a final line
    // without a terminator and drops the empty tail after a terminated one.
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut archived_raw = Vec::new();
    let mut kept_raw = Vec::new();
    let mut split_reached = false;
    for (index, line) in normalized.split_terminator('\n').enumerate() {
      if split_reached {
        kept_raw.push(line);
        continue;
      }
      if pystr::strip(line).is_empty() {
        continue;
      }
      if archives_line(line, cutoff, &live_path, index + 1)? {
        archived_raw.push(line);
      } else {
        split_reached = true;
        kept_raw.push(line);
      }
    }
    if archived_raw.is_empty() {
      return Ok(None);
    }
    let date = cutoff.into_inner().date();
    let (iso_year, iso_week, _) = date.to_iso_week_date();
    let archive_dir = live_path.parent().expect("event log path has a parent").join("archives");
    fs::create_dir_all(&archive_dir).map_err(|error| StoreError::new(format!("{}: {error}", archive_dir.display())))?;
    let archive_path = archive_dir.join(format!("chat_events.{iso_year}-W{iso_week:02}.jsonl"));
    let mut archive = OpenOptions::new()
      .create(true)
      .append(true)
      .open(&archive_path)
      .map_err(|error| StoreError::new(format!("{}: {error}", archive_path.display())))?;
    for line in &archived_raw {
      archive
        .write_all(line.as_bytes())
        .and_then(|()| archive.write_all(b"\n"))
        .map_err(|error| StoreError::new(format!("{}: {error}", archive_path.display())))?;
    }
    drop(archive);
    let mut replacement = String::new();
    for line in kept_raw {
      replacement.push_str(line);
      replacement.push('\n');
    }
    fsutil::atomic_write_text(&live_path, &replacement)
      .map_err(|error| StoreError::new(format!("{}: {error}", live_path.display())))?;
    let archived_count = archived_raw.len() as i64;
    self
      .mutate_fresh(session_id, false, true, |meta: &mut SessionMetadata| {
        meta.archive_offset += archived_count;
        Ok(true)
      })
      .await?;
    self.clear_chat_events_cache(session_id);
    Ok(Some(archive_path))
  }

  pub(crate) fn read_archive_offset(&self, session_id: &str) -> StoreResult<usize> {
    if let Some(meta) = self.metadata_cache.lock().expect("metadata cache mutex is not poisoned").get(session_id) {
      return Ok(meta.value.archive_offset.max(0) as usize);
    }
    let meta = match self.read_metadata_fresh(session_id) {
      Ok(meta) => meta,
      Err(error) => {
        // `read_archive_offset_sync` falls back to 0 when metadata.json is missing or unreadable.
        tracing::debug!(session_id, error = %error, "archive_offset_read_failed");
        return Ok(0);
      }
    };
    Ok(meta.map_or(0, |meta| meta.archive_offset.max(0) as usize))
  }

  /// The session's archive files in chronological (filename) order, memoized on the archive directory's
  /// `(mtime_ns, size)` like `src/runtime/chat_events.py:354-379`.
  pub(crate) fn archive_paths(&self, session_id: &str) -> StoreResult<Arc<Vec<PathBuf>>> {
    let dir = self.session_dir(session_id).join("data").join("archives");
    let metadata = match fs::metadata(&dir) {
      Ok(metadata) => metadata,
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Arc::new(Vec::new())),
      Err(error) => {
        tracing::debug!(path = %dir.display(), error = %error, "archive_dir_stat_failed");
        return Ok(Arc::new(Vec::new()));
      }
    };
    let signature = FileSignature::from_metadata(&metadata);
    if let Some(paths) = self
      .event_read_caches
      .lock()
      .expect("event read cache mutex is not poisoned")
      .archive_paths
      .fresh(&dir, signature)
    {
      return Ok(paths);
    }
    let entries = match fs::read_dir(&dir) {
      Ok(entries) => entries,
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Arc::new(Vec::new())),
      Err(error) => {
        tracing::debug!(path = %dir.display(), error = %error, "archive_dir_stat_failed");
        return Ok(Arc::new(Vec::new()));
      }
    };
    let mut paths = Vec::new();
    for entry in entries {
      let entry = entry.map_err(|error| StoreError::new(format!("{}: {error}", dir.display())))?;
      if is_chat_events_archive(&entry.file_name().to_string_lossy()) {
        paths.push(entry.path());
      }
    }
    paths.sort();
    #[cfg(test)]
    {
      self.event_read_caches.lock().expect("event read cache mutex is not poisoned").archive_dir_walks += 1;
    }
    let paths = Arc::new(paths);
    self.event_read_caches.lock().expect("event read cache mutex is not poisoned").archive_paths.record(
      dir,
      signature,
      Arc::clone(&paths),
    );
    Ok(paths)
  }

  /// Read a live event file into the shared cache from Tokio's blocking pool.
  pub(crate) async fn load_chat_events_shared_async(&self, session_id: &str) -> StoreResult<Arc<Vec<SharedChatEvent>>> {
    let cache_entry = self.events_cache_entry(session_id);
    let path = self.chat_events_path(session_id);
    let generation = self.next_events_cache_generation.fetch_add(1, Ordering::Relaxed);
    #[cfg(test)]
    let read_hook = self.event_read_hook.lock().expect("event read hook mutex is not poisoned").clone();
    let (events, did_read) = tokio::task::spawn_blocking(move || {
      let mut cache = cache_entry.lock().expect("session event cache mutex is not poisoned");
      if let Some(cached) = cache.as_ref() {
        return Ok::<_, StoreError>((Arc::clone(&cached.events), false));
      }
      #[cfg(test)]
      if let Some(hook) = read_hook {
        hook();
      }
      let events = Arc::new(ndjson::read_file_compact(&path)?.into_iter().map(Arc::new).collect());
      *cache = Some(CachedChatEvents { generation, identity: Arc::new(()), events: Arc::clone(&events) });
      Ok((events, true))
    })
    .await
    .map_err(|error| StoreError::new(error.to_string()))??;
    if did_read {
      self.history_work.file_reads.fetch_add(1, Ordering::Relaxed);
    }
    Ok(events)
  }
}

/// Take the physical-line slots in `[start, end)`, dropping the slots that parsed to nothing.
fn slice_live_lines(lines: &[Option<Arc<ChatEvent>>], start: usize, end: usize) -> Vec<ChatEvent> {
  lines.iter().skip(start).take(end.saturating_sub(start)).flatten().map(|event| (**event).clone()).collect()
}

fn file_signature(path: &Path) -> StoreResult<Option<(fs::Metadata, FileSignature)>> {
  match fs::metadata(path) {
    Ok(metadata) => Ok(Some((metadata.clone(), FileSignature::from_metadata(&metadata)))),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
    Err(error) => Err(StoreError::new(format!("{}: {error}", path.display()))),
  }
}

fn read_ndjson_tail_lines(path: &Path, file_size: u64, take: usize) -> StoreResult<(Vec<Vec<u8>>, usize)> {
  let mut file = fs::File::open(path).map_err(|error| StoreError::new(format!("{}: {error}", path.display())))?;
  let mut tail_bytes_read = 0;
  if file_size > TAIL_WINDOW_SIZE as u64 {
    let start = file_size - TAIL_WINDOW_SIZE as u64;
    file.seek(SeekFrom::Start(start)).map_err(|error| StoreError::new(format!("{}: {error}", path.display())))?;
    let mut window = Vec::with_capacity(TAIL_WINDOW_SIZE);
    file.read_to_end(&mut window).map_err(|error| StoreError::new(format!("{}: {error}", path.display())))?;
    tail_bytes_read = window.len();
    let mut lines: Vec<&[u8]> = window.split(|byte| *byte == b'\n').skip(1).collect();
    if lines.last().is_some_and(|line| line.is_empty()) {
      lines.pop();
    }
    let nonblank: Vec<&[u8]> = lines.into_iter().filter(|line| !line.iter().all(u8::is_ascii_whitespace)).collect();
    if nonblank.len() >= take {
      return Ok((nonblank[nonblank.len() - take..].iter().map(|line| line.to_vec()).collect(), window.len()));
    }
  }
  file.seek(SeekFrom::Start(0)).map_err(|error| StoreError::new(format!("{}: {error}", path.display())))?;
  let mut bytes = Vec::new();
  file.read_to_end(&mut bytes).map_err(|error| StoreError::new(format!("{}: {error}", path.display())))?;
  let mut lines: Vec<&[u8]> = bytes.split(|byte| *byte == b'\n').collect();
  if bytes.ends_with(b"\n") {
    lines.pop();
  }
  let nonblank: Vec<&[u8]> = lines.into_iter().filter(|line| !line.iter().all(u8::is_ascii_whitespace)).collect();
  let start = nonblank.len().saturating_sub(take);
  Ok((nonblank[start..].iter().map(|line| line.to_vec()).collect(), tail_bytes_read + bytes.len()))
}

fn read_live_range_suffix(path: &Path, start: usize, size: usize) -> std::io::Result<Vec<u8>> {
  let mut file = fs::File::open(path)?;
  file.seek(SeekFrom::Start(start as u64))?;
  let mut bytes = Vec::with_capacity(size.saturating_sub(start));
  file.read_to_end(&mut bytes)?;
  Ok(bytes)
}

fn parse_universal_newline_lines(path: &Path, bytes: &[u8]) -> StoreResult<Vec<Option<Arc<ChatEvent>>>> {
  let text =
    String::from_utf8(bytes.to_vec()).map_err(|error| StoreError::new(format!("{}: {error}", path.display())))?;
  let mut lines = Vec::new();
  let mut segment = String::new();
  let mut chars = text.chars().peekable();
  while let Some(character) = chars.next() {
    match character {
      '\n' => {
        lines.push(ndjson::parse_line_compact(segment.as_bytes()).map(Arc::new));
        segment.clear();
      }
      '\r' => {
        if chars.peek() == Some(&'\n') {
          chars.next();
        }
        lines.push(ndjson::parse_line_compact(segment.as_bytes()).map(Arc::new));
        segment.clear();
      }
      _ => segment.push(character),
    }
  }
  if !segment.is_empty() || (!text.is_empty() && !text.ends_with('\n') && !text.ends_with('\r')) {
    lines.push(ndjson::parse_line_compact(segment.as_bytes()).map(Arc::new));
  }
  Ok(lines)
}

fn parse_text_lines(path: &Path, lines: Vec<Vec<u8>>) -> StoreResult<Vec<ParsedLiveLine>> {
  lines
    .into_iter()
    .map(|line| {
      let text = String::from_utf8(line).map_err(|error| StoreError::new(format!("{}: {error}", path.display())))?;
      Ok(ndjson::parse_line_compact(text.as_bytes()).map(Arc::new))
    })
    .collect()
}

fn walk_tail_line_texts(path: &Path, size: usize, count: usize) -> StoreResult<Option<TailLineWalk>> {
  if count == 0 {
    return Ok(Some(TailLineWalk { lines: Vec::new(), byte_start: size, ends_on_boundary: true, bytes_read: 0 }));
  }
  let mut file = match fs::File::open(path) {
    Ok(file) => file,
    Err(error) => {
      tracing::debug!(path = %path.display(), error = %error, "live_range_read_failed");
      return Ok(None);
    }
  };
  let mut parts = Vec::new();
  let mut accumulated = 0usize;
  let mut bound = 0usize;
  let mut pos = size;
  let (spans, ends_on_boundary) = loop {
    if bound >= count + 2 || pos == 0 {
      let bytes = parts.iter().rev().flat_map(|part: &Vec<u8>| part.iter().copied()).collect::<Vec<_>>();
      let (spans, ends_on_boundary) = raw_line_spans(&bytes);
      let complete = spans.len().saturating_sub(usize::from(pos > 0));
      if complete >= count {
        break (spans, ends_on_boundary);
      }
      if pos == 0 {
        return Ok(None);
      }
    }
    if accumulated > LIVE_RANGE_WALK_BYTE_BUDGET {
      return Ok(None);
    }
    let step = TAIL_WINDOW_SIZE.min(pos);
    pos -= step;
    file.seek(SeekFrom::Start(pos as u64)).map_err(|error| StoreError::new(format!("{}: {error}", path.display())))?;
    let mut chunk = vec![0; step];
    file.read_exact(&mut chunk).map_err(|error| StoreError::new(format!("{}: {error}", path.display())))?;
    bound += chunk.iter().filter(|byte| **byte == b'\n' || **byte == b'\r').count();
    accumulated += step;
    parts.push(chunk);
  };
  let bytes = parts.iter().rev().flat_map(|part| part.iter().copied()).collect::<Vec<_>>();
  if spans.len() < count {
    return Ok(None);
  }
  let first = spans.len() - count;
  let selected = spans[first..].iter().map(|(start, end)| bytes[*start..*end].to_vec()).collect::<Vec<_>>();
  let byte_start = pos + spans[first].0;
  Ok(Some(TailLineWalk { lines: selected, byte_start, ends_on_boundary, bytes_read: accumulated }))
}

fn raw_line_spans(bytes: &[u8]) -> (Vec<(usize, usize)>, bool) {
  let mut spans = Vec::new();
  let mut start = 0;
  let mut pos = 0;
  while pos < bytes.len() {
    let newline = bytes[pos..].iter().position(|byte| *byte == b'\n').map(|offset| pos + offset);
    let carriage = bytes[pos..].iter().position(|byte| *byte == b'\r').map(|offset| pos + offset);
    match (newline, carriage) {
      (None, None) => break,
      (Some(index), Some(carriage)) if carriage < index => {
        spans.push((start, carriage));
        pos = if bytes.get(carriage + 1) == Some(&b'\n') { carriage + 2 } else { carriage + 1 };
        start = pos;
      }
      (None, Some(carriage)) => {
        spans.push((start, carriage));
        pos = if bytes.get(carriage + 1) == Some(&b'\n') { carriage + 2 } else { carriage + 1 };
        start = pos;
      }
      (Some(index), _) => {
        spans.push((start, index));
        pos = index + 1;
        start = pos;
      }
    }
  }
  let ends_on_boundary = start == bytes.len();
  if !ends_on_boundary {
    spans.push((start, bytes.len()));
  }
  (spans, ends_on_boundary)
}

fn python_event_timestamp(value: UtcDateTime) -> String {
  let datetime = value.into_inner();
  let base = format!(
    "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
    datetime.year(),
    u8::from(datetime.month()),
    datetime.day(),
    datetime.hour(),
    datetime.minute(),
    datetime.second(),
  );
  if datetime.microsecond() == 0 {
    format!("{base}+00:00")
  } else {
    format!("{base}.{:06}+00:00", datetime.microsecond())
  }
}

/// Whether a non-blank line before the split point is an event older than `cutoff`. A line that is not JSON, or
/// whose `timestamp` is missing, falsy or unparsable, is not archived and starts the kept part. A JSON value that
/// is not an object, or a truthy `timestamp` that is not a string, fails the archive, as Python raises on it.
fn archives_line(line: &str, cutoff: UtcDateTime, live_path: &Path, line_number: usize) -> StoreResult<bool> {
  let Ok(event) = serde_json::from_str::<Value>(pystr::strip(line)) else {
    tracing::debug!(path = %live_path.display(), line = line_number, "chat_event_archive_parse_skip");
    return Ok(false);
  };
  let Value::Object(event) = event else {
    return Err(StoreError::new(format!("{}:{line_number}: chat event is not a JSON object", live_path.display())));
  };
  let timestamp = match event.get("timestamp") {
    Some(timestamp) if python_truthy(timestamp) => timestamp,
    _ => return Ok(false),
  };
  let Value::String(timestamp) = timestamp else {
    return Err(StoreError::new(format!(
      "{}:{line_number}: chat event timestamp is not a string",
      live_path.display()
    )));
  };
  let Ok(parsed) = UtcDateTime::parse(timestamp) else {
    tracing::debug!(path = %live_path.display(), line = line_number, "chat_event_archive_ts_parse_skip");
    return Ok(false);
  };
  Ok(parsed < cutoff)
}

/// Python's truth value for a parsed JSON value: `None`, `False`, zero, and empty strings, lists and objects are false.
fn python_truthy(value: &Value) -> bool {
  match value {
    Value::Null => false,
    Value::Bool(flag) => *flag,
    Value::Number(number) => number.as_f64() != Some(0.0),
    Value::String(text) => !text.is_empty(),
    Value::Array(items) => !items.is_empty(),
    Value::Object(fields) => !fields.is_empty(),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::store::test_support::make_test_store;
  use serde_json::json;
  use std::collections::BTreeSet;
  use std::sync::{Mutex, mpsc};
  use std::time::Duration;

  #[test]
  fn event_timestamp_matches_python_isoformat_offset() {
    let value = UtcDateTime::parse("2026-01-01T00:00:00.123400Z").unwrap();
    assert_eq!(python_event_timestamp(value), "2026-01-01T00:00:00.123400+00:00");
  }

  #[test]
  fn archive_path_uses_the_cutoff_iso_week() {
    let value = UtcDateTime::parse("2021-01-01T12:00:00Z").unwrap();
    let (year, week, _) = value.into_inner().date().to_iso_week_date();
    assert_eq!((year, week), (2020, 53));
  }

  #[tokio::test]
  async fn event_count_tail_and_global_range_reads_agree() {
    let (_home, store) = make_test_store();
    let mut expected = Vec::new();
    for index in 0..5 {
      expected.push(store.append_chat_event("synthetic", json!({"type": "user", "content": index})).await.unwrap());
    }

    assert_eq!(store.get_chat_event_count_sync("synthetic").unwrap(), expected.len());
    let (tail, total, has_more) = store.load_chat_events_tail("synthetic", 2).unwrap();
    assert_eq!(tail, expected[3..]);
    assert_eq!(total, 5);
    assert!(has_more);

    let (range, has_earlier) = store.load_chat_events_range("synthetic", 1, 4).unwrap();
    assert_eq!(range, expected[1..4]);
    assert!(has_earlier);
    let (full, has_earlier) = store.load_chat_events_range("synthetic", 0, 5).unwrap();
    assert_eq!(full, expected);
    assert!(!has_earlier);
  }

  #[tokio::test]
  async fn event_cache_generation_keeps_identity_on_append_and_changes_after_clear() {
    let (_home, store) = make_test_store();
    store.append_chat_event("synthetic", json!({"id": "event-a", "type": "user"})).await.unwrap();
    let (initial_generation, initial_identity, initial_events) =
      store.live_events_with_generation("synthetic").unwrap();
    assert_eq!(initial_events.len(), 1);

    store.append_chat_event("synthetic", json!({"id": "event-b", "type": "user"})).await.unwrap();
    let (append_generation, append_identity, appended_events) = store.live_events_with_generation("synthetic").unwrap();
    assert_eq!(append_generation, initial_generation, "append preserves the Python event-list identity");
    assert!(Arc::ptr_eq(&append_identity, &initial_identity), "append preserves the event-list memo key");
    assert_eq!(appended_events.len(), 2);

    store.clear_chat_events_cache("synthetic");
    let (replacement_generation, replacement_identity, replacement_events) =
      store.live_events_with_generation("synthetic").unwrap();
    assert_ne!(replacement_generation, append_generation, "cache replacement gives the Python-style new list identity");
    assert!(!Arc::ptr_eq(&replacement_identity, &append_identity), "replacement changes the event-list memo key");
    assert_eq!(replacement_events.len(), 2);
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn cold_read_of_one_session_does_not_block_another_sessions_append() {
    let (_home, store) = make_test_store();
    let store = Arc::new(store);
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Arc::new(Mutex::new(release_rx));
    *store.event_read_hook.lock().unwrap() = Some(Arc::new(move || {
      entered_tx.send(()).unwrap();
      release_rx.lock().unwrap().recv().unwrap();
    }));

    let reader_store = Arc::clone(&store);
    let reader = tokio::task::spawn_blocking(move || reader_store.load_chat_events_shared_sync("synthetic-a"));
    let entered = entered_rx.recv_timeout(Duration::from_secs(1)).is_ok();
    if !entered {
      let _ = release_tx.send(());
      reader.await.unwrap().unwrap();
      panic!("cold read did not reach its file-read hook");
    }

    let append_store = Arc::clone(&store);
    let mut append = tokio::spawn(async move {
      append_store.append_chat_event("synthetic-b", json!({"id": "event-b", "type": "user"})).await
    });
    let append_result = tokio::time::timeout(Duration::from_secs(1), &mut append).await;
    let appended_while_read_held = append_result.is_ok();
    release_tx.send(()).unwrap();
    reader.await.unwrap().unwrap();
    let appended = match append_result {
      Ok(result) => result.unwrap().unwrap(),
      Err(_) => append.await.unwrap().unwrap(),
    };

    assert!(appended_while_read_held, "session B append waited for session A file read");
    assert_eq!(appended["id"].as_str(), Some("event-b"));
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  async fn concurrent_appends_during_cold_reads_land_once_in_cache_and_file() {
    let (_home, store) = make_test_store();
    let store = Arc::new(store);
    let session_id = "synthetic-same-session";
    let path = store.chat_events_path(session_id);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();

    for iteration in 0..200 {
      store.clear_chat_events_cache(session_id);
      std::fs::write(&path, b"").unwrap();
      let (entered_tx, entered_rx) = mpsc::channel();
      let (release_tx, release_rx) = mpsc::channel();
      let release_rx = Arc::new(Mutex::new(release_rx));
      *store.event_read_hook.lock().unwrap() = Some(Arc::new(move || {
        entered_tx.send(()).unwrap();
        release_rx.lock().unwrap().recv().unwrap();
      }));

      let reader_store = Arc::clone(&store);
      let reader = tokio::task::spawn_blocking(move || reader_store.load_chat_events_shared_sync(session_id));
      let entered = entered_rx.recv_timeout(Duration::from_secs(1)).is_ok();
      if !entered {
        let _ = release_tx.send(());
        reader.await.unwrap().unwrap();
        panic!("iteration {iteration}: cold read did not reach its file-read hook");
      }

      let (started_tx, started_rx) = mpsc::channel();
      let mut appends = Vec::new();
      let expected: BTreeSet<String> = (0..2).map(|index| format!("event-{iteration}-{index}")).collect();
      for index in 0..2 {
        let append_store = Arc::clone(&store);
        let started_tx = started_tx.clone();
        let event_id = format!("event-{iteration}-{index}");
        appends.push(tokio::spawn(async move {
          started_tx.send(()).unwrap();
          append_store.append_chat_event(session_id, json!({"id": event_id, "type": "user"})).await
        }));
      }
      drop(started_tx);
      for _ in 0..2 {
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
      }
      release_tx.send(()).unwrap();
      reader.await.unwrap().unwrap();
      for append in appends {
        append.await.unwrap().unwrap();
      }

      let cached = store.load_chat_events_shared_sync(session_id).unwrap();
      let cache_ids: BTreeSet<String> = cached.iter().map(|event| event["id"].as_str().unwrap().to_string()).collect();
      let file_events = ndjson::read_file(&path).unwrap();
      let file_ids: BTreeSet<String> =
        file_events.iter().map(|event| event["id"].as_str().unwrap().to_string()).collect();
      assert_eq!(cached.len(), expected.len(), "iteration {iteration}: cached event count");
      assert_eq!(file_events.len(), expected.len(), "iteration {iteration}: file event count");
      assert_eq!(cache_ids, expected, "iteration {iteration}: cache ids");
      assert_eq!(file_ids, expected, "iteration {iteration}: file ids");
      assert_eq!(
        cached.iter().map(|event| event["id"].to_value()).collect::<Vec<_>>(),
        file_events.iter().map(|event| event["id"].clone()).collect::<Vec<_>>(),
        "iteration {iteration}: cache and file order"
      );
    }
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn eviction_waits_for_an_append_that_holds_the_session_lock() {
    let (_home, store) = make_test_store();
    let store = Arc::new(store);
    let session_id = "synthetic-evicted";
    store.append_chat_event(session_id, json!({"id": "before", "type": "user"})).await.unwrap();
    store.load_chat_events_shared_sync(session_id).unwrap();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Arc::new(Mutex::new(release_rx));
    *store.event_append_hook.lock().unwrap() = Some(Arc::new(move || {
      entered_tx.send(()).unwrap();
      release_rx.lock().unwrap().recv().unwrap();
    }));

    let append_store = Arc::clone(&store);
    let append =
      tokio::spawn(
        async move { append_store.append_chat_event(session_id, json!({"id": "during", "type": "user"})).await },
      );
    entered_rx.recv_timeout(Duration::from_secs(1)).expect("the append reaches its hook while it holds the lock");
    let evict_store = Arc::clone(&store);
    let mut evict = tokio::task::spawn_blocking(move || {
      evict_store.evict_recovery_history(session_id);
      evict_store.load_chat_events_shared_sync(session_id)
    });
    let evict_result = tokio::time::timeout(Duration::from_millis(300), &mut evict).await;
    let evicted_while_append_held = evict_result.is_ok();
    release_tx.send(()).unwrap();
    append.await.unwrap().unwrap();
    match evict_result {
      Ok(result) => drop(result.unwrap().unwrap()),
      Err(_) => drop(evict.await.unwrap().unwrap()),
    }
    *store.event_append_hook.lock().unwrap() = None;

    let cached = store.load_chat_events_shared_sync(session_id).unwrap();
    let ids: Vec<&str> = cached.iter().map(|event| event["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["before", "during"], "the cache after the eviction lost the append that held the lock");
    assert!(!evicted_while_append_held, "the eviction ran while an append held the session lock");
  }

  #[tokio::test]
  async fn shared_reads_reuse_the_snapshot_until_append() {
    let (_home, store) = make_test_store();
    store.append_chat_event("synthetic-shared", json!({"id": "first", "type": "user"})).await.unwrap();

    let first = store.load_chat_events_shared_sync("synthetic-shared").unwrap();
    let second = store.load_chat_events_shared_sync("synthetic-shared").unwrap();
    assert!(Arc::ptr_eq(&first, &second));

    store.append_chat_event("synthetic-shared", json!({"id": "second", "type": "user"})).await.unwrap();
    let appended = store.load_chat_events_shared_sync("synthetic-shared").unwrap();
    assert!(!Arc::ptr_eq(&first, &appended));
    assert_eq!(first.len(), 1);
    assert_eq!(appended.last().unwrap()["id"].as_str(), Some("second"));
  }

  #[tokio::test]
  async fn cancelled_archive_read_checks_lines_and_does_not_memoize_a_prefix() {
    let (_home, store) = make_test_store();
    let session_id = "synthetic-cancelled-archive";
    let archive_path =
      store.chat_events_path(session_id).parent().unwrap().join("archives").join("chat_events.2026-W01.jsonl");
    fs::create_dir_all(archive_path.parent().unwrap()).unwrap();
    fs::write(&archive_path, b"{\"id\":\"first\"}\n{\"id\":\"second\"}\n{\"id\":\"third\"}\n").unwrap();

    let mut checks = 0;
    let interrupted = store
      .load_archive_range_until(session_id, 0, 3, &mut || {
        checks += 1;
        checks == 6
      })
      .unwrap();
    assert!(interrupted.is_none(), "the archive range stops before returning a parsed prefix");
    assert_eq!(checks, 6, "the archive reader checks cancellation between physical event lines");
    assert_eq!(
      store.event_read_caches.lock().unwrap().archive_file_reads,
      1,
      "the interrupted file was opened once and its partial events were not cached"
    );

    let complete = store.load_archive_range(session_id, 0, 3).unwrap();
    assert_eq!(
      complete.iter().map(|event| event["id"].as_str().unwrap()).collect::<Vec<_>>(),
      ["first", "second", "third"]
    );
    assert_eq!(store.event_read_caches.lock().unwrap().archive_file_reads, 2, "the uncached file was read again");
  }

  #[tokio::test]
  async fn event_deltas_share_the_cached_vector_and_expose_the_suffix_index() {
    let (_home, store) = make_test_store();
    store.append_chat_event("synthetic-delta", json!({"id": "first", "type": "user"})).await.unwrap();
    store.append_chat_event("synthetic-delta", json!({"id": "second", "type": "user"})).await.unwrap();
    let events = store.load_chat_events_shared_sync("synthetic-delta").unwrap();

    let first = store.load_chat_events_delta_sync("synthetic-delta", None, 0).unwrap();
    assert!(Arc::ptr_eq(&events, &first.events));
    assert_eq!(first.start_index, 0);
    let suffix = store.load_chat_events_delta_sync("synthetic-delta", Some(first.generation), 1).unwrap();
    assert!(Arc::ptr_eq(&events, &suffix.events));
    assert_eq!(suffix.start_index, 1);
    assert_eq!(suffix.events[suffix.start_index]["id"].as_str(), Some("second"));
  }

  mod parity {
    use super::*;
    use crate::{StoreHooks, StoreResult};
    use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot, SlotField};
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::Arc;

    struct Labels;
    impl crate::AccountLabelKeeper for Labels {
      fn record_account_label(&self, meta: &mut SessionMetadata, label: Option<&str>) -> StoreResult<bool> {
        let value = label.map_or(Value::Null, |label| Value::String(label.to_string()));
        if meta.extra("claude_account") == Some(&value) {
          return Ok(false);
        }
        meta.set_extra("claude_account", value);
        Ok(true)
      }

      fn account_label(&self, meta: &SessionMetadata) -> Option<String> {
        meta.extra("claude_account").and_then(Value::as_str).map(str::to_string)
      }
    }

    struct Anchors;
    impl crate::AnchorFieldReconciler for Anchors {
      fn reconcile(&self, disk: &SessionMetadata, attempted: &mut SessionMetadata) -> StoreResult<()> {
        attempted.cc_session_id.clone_from(&disk.cc_session_id);
        attempted.native_backend.clone_from(&disk.native_backend);
        Ok(())
      }
    }

    struct Listing;
    impl crate::SequenceListingFields for Listing {
      fn owns_session(&self, _meta: &SessionMetadata) -> bool {
        false
      }
      fn listing_fields(
        &self,
        _: &[String],
        _: UtcDateTime,
      ) -> StoreResult<HashMap<String, serde_json::Map<String, Value>>> {
        Ok(HashMap::new())
      }
      fn sequence_subtree_roots(&self, _: &[SessionMetadata]) -> StoreResult<HashMap<String, String>> {
        Ok(HashMap::new())
      }
      fn view_subtree_roots(&self, _: &[SessionMetadata]) -> StoreResult<HashMap<String, HashMap<String, String>>> {
        Ok(HashMap::new())
      }
    }

    fn test_store(home: &std::path::Path) -> Store {
      let cfg = charliebot_core::config::load_config(home, &charliebot_core::config::ConfigSchema::default()).unwrap();
      Store::new(
        cfg,
        vec![MetadataSlot {
          owner: "probe",
          on: MetadataFile::Session,
          after: Some("group"),
          fields: vec![SlotField { name: "probe_note", default: Value::Null }],
        }],
        StoreHooks {
          account_labels: Some(Arc::new(Labels)),
          anchor_reconciler: Some(Arc::new(Anchors)),
          sequence_listing: Some(Arc::new(Listing)),
          runtime_events: Some(Arc::new(crate::hooks::EmptyRuntimeEventHooks)),
          sequence_archive: Some(Arc::new(crate::store::test_support::SequenceArchive)),
          sequence_binding_mover: Some(Arc::new(crate::store::test_support::BindingMover)),
        },
      )
      .unwrap()
    }

    fn seed_metadata(store: &Store, session_id: &str) {
      let meta = SessionMetadata::new(session_id, "seed", crate::metadata::TaskProfile::Manager);
      let path = store.metadata_path(session_id);
      std::fs::create_dir_all(path.parent().unwrap()).unwrap();
      std::fs::write(&path, meta.to_json_string(&store.slots, true)).unwrap();
    }

    fn ids(events: &[ChatEvent]) -> Vec<String> {
      events.iter().map(|event| event["id"].as_str().unwrap().to_string()).collect()
    }

    fn shared_ids(events: &[SharedChatEvent]) -> Vec<String> {
      events.iter().map(|event| event["id"].as_str().unwrap().to_string()).collect()
    }

    /// `test_recycle.py::test_recycle_archives_old_chat_events_and_advances_offset` at store level:
    /// the old head lands in a weekly archive file, the live file keeps the retained tail, the
    /// offset advances, and the next append continues the global numbering.
    #[tokio::test]
    async fn archiving_the_old_head_advances_the_offset_and_continues_global_indexes() {
      let home = tempfile::tempdir().unwrap();
      let store = test_store(home.path());
      let session_id = "synthetic-session";
      seed_metadata(&store, session_id);
      let path = store.chat_events_path(session_id);
      std::fs::create_dir_all(path.parent().unwrap()).unwrap();
      let mut body = String::new();
      for index in 0..6 {
        body.push_str(&format!("{{\"id\": \"old-{index}\", \"timestamp\": \"2025-02-01T00:00:00+00:00\"}}\n"));
      }
      for index in 0..3 {
        body.push_str(&format!("{{\"id\": \"new-{index}\", \"timestamp\": \"2025-02-05T00:00:0{}+00:00\"}}\n", index));
      }
      std::fs::write(&path, body).unwrap();

      let archive = store
        .archive_events_before(session_id, UtcDateTime::parse("2025-02-04T00:00:00Z").unwrap())
        .await
        .unwrap()
        .unwrap();
      assert!(archive.ends_with("chat_events.2025-W06.jsonl"), "{}", archive.display());
      let archive_text = std::fs::read_to_string(&archive).unwrap();
      let archive_ids: Vec<String> = archive_text
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap()["id"].as_str().unwrap().to_string())
        .collect();
      assert_eq!(archive_ids, (0..6).map(|index| format!("old-{index}")).collect::<Vec<_>>());
      let live_text = std::fs::read_to_string(&path).unwrap();
      let live_ids: Vec<String> = live_text
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap()["id"].as_str().unwrap().to_string())
        .collect();
      assert_eq!(live_ids, (0..3).map(|index| format!("new-{index}")).collect::<Vec<_>>());
      assert_eq!(store.read_metadata_fresh(session_id).unwrap().unwrap().archive_offset, 6);
      assert_eq!(store.get_chat_event_count_sync(session_id).unwrap(), 9);
      assert_eq!(ids(&store.load_all_facts(session_id).unwrap()), {
        let mut all: Vec<String> = (0..6).map(|index| format!("old-{index}")).collect();
        all.extend((0..3).map(|index| format!("new-{index}")));
        all
      });
      let appended = store
        .append_chat_event(session_id, json!({"type": "user", "content": "after", "id": "after-1"}))
        .await
        .unwrap();
      let _ = appended;
      // Global index 9 is the first appended event after six archived rows and three retained rows.
      assert_eq!(store.load_chat_events_range(session_id, 9, 10).unwrap().0[0]["id"].as_str(), Some("after-1"));
    }

    /// `test_recycle.py::test_recycle_noop_when_nothing_old` at store level: nothing archived, no
    /// archive file, the offset stays 0.
    #[tokio::test]
    async fn archiving_without_old_events_is_a_noop() {
      let home = tempfile::tempdir().unwrap();
      let store = test_store(home.path());
      let session_id = "synthetic-session";
      seed_metadata(&store, session_id);
      let path = store.chat_events_path(session_id);
      std::fs::create_dir_all(path.parent().unwrap()).unwrap();
      std::fs::write(&path, "{\"id\": \"future\", \"timestamp\": \"2026-05-10T01:00:00+00:00\"}\n").unwrap();
      let archive =
        store.archive_events_before(session_id, UtcDateTime::parse("2026-05-10T00:00:00Z").unwrap()).await.unwrap();
      assert!(archive.is_none());
      assert_eq!(store.read_metadata_fresh(session_id).unwrap().unwrap().archive_offset, 0);
      assert_eq!(store.load_chat_events_sync(session_id).unwrap().len(), 1);
    }

    /// `test_recycle.py::test_live_range_walk_matches_full_build_across_line_shapes`: blank and
    /// malformed lines consume an index, a CRLF pair terminates one line, and the final line
    /// carries no newline.
    #[tokio::test]
    async fn range_reads_number_physical_lines_when_archive_offset_is_positive() {
      let home = tempfile::tempdir().unwrap();
      let store = test_store(home.path());
      let session_id = "synthetic-session";
      seed_metadata(&store, session_id);
      let path = store.chat_events_path(session_id);
      std::fs::create_dir_all(path.parent().unwrap()).unwrap();
      let mut body = String::new();
      for index in 0..6 {
        body.push_str(&format!("{{\"id\": \"old-{index}\", \"timestamp\": \"2025-02-01T00:00:00+00:00\"}}\n"));
      }
      std::fs::write(&path, body).unwrap();
      store.archive_events_before(session_id, UtcDateTime::parse("2025-02-03T00:00:00Z").unwrap()).await.unwrap();

      std::fs::write(&path, b"{\"id\": \"a0\"}\n\n{bad json\n{\"id\": \"a1\"}\r\n{\"id\": \"a2\"}\n{\"id\": \"a3\"}")
        .unwrap();
      let spec: Vec<Option<&str>> = vec![Some("a0"), None, None, Some("a1"), Some("a2"), Some("a3")];
      let expected = |start: usize, end: usize| -> Vec<String> {
        spec
          .iter()
          .skip(start.max(6) - 6)
          .take(end.saturating_sub(start))
          .filter_map(|slot| *slot)
          .map(str::to_string)
          .collect()
      };
      for (start, end) in [(6usize, 7usize), (7, 9), (8, 10), (6, 10), (9, 12), (11, 15), (7, 7)] {
        let (events, has_more) = store.load_chat_events_range(session_id, start, end).unwrap();
        assert_eq!(ids(&events), expected(start, end), "window [{start}, {end})");
        assert_eq!(has_more, start > 0, "window [{start}, {end})");
      }
    }

    /// The unarchived cold path numbers physical lines (`ndjson.parse_ndjson_range`'s islice),
    /// and the warm cache numbers its own parsed-event indices.
    #[tokio::test]
    async fn unarchived_range_reads_cold_numbers_physical_lines_and_warm_numbers_parsed_events() {
      let home = tempfile::tempdir().unwrap();
      let store = test_store(home.path());
      let session_id = "synthetic-session";
      seed_metadata(&store, session_id);
      let path = store.chat_events_path(session_id);
      std::fs::create_dir_all(path.parent().unwrap()).unwrap();
      std::fs::write(&path, b"{\"id\": \"a\"}\n\n{bad\n{\"id\": \"b\"}\n{\"id\": \"c\"}").unwrap();

      let (cold, has_more) = store.load_chat_events_range(session_id, 3, 5).unwrap();
      assert_eq!(ids(&cold), vec!["b", "c"]);
      assert!(has_more);
      // The blank and the malformed line each consumed a physical index.
      let (cold_head, _) = store.load_chat_events_range(session_id, 1, 3).unwrap();
      assert!(cold_head.is_empty());

      store.load_chat_events_sync(session_id).unwrap();
      let (warm, _) = store.load_chat_events_range(session_id, 1, 2).unwrap();
      assert_eq!(ids(&warm), vec!["b"]);
      let (warm_end, _) = store.load_chat_events_range(session_id, 2, 6).unwrap();
      assert_eq!(ids(&warm_end), vec!["c"]);
    }

    /// `fact_history` composition: the archived count is the total event count minus the live
    /// count, archive files beyond that count stay unread — one stale archive row beyond the
    /// offset never reaches the facts.
    #[tokio::test]
    async fn load_all_facts_stops_reading_archives_at_the_archive_offset() {
      let home = tempfile::tempdir().unwrap();
      let store = test_store(home.path());
      let session_id = "synthetic-session";
      seed_metadata(&store, session_id);
      let path = store.chat_events_path(session_id);
      let archive_dir = path.parent().unwrap().join("archives");
      std::fs::create_dir_all(&archive_dir).unwrap();
      let mut archive_body = String::new();
      for index in 0..4 {
        archive_body.push_str(&format!("{{\"id\": \"archived-{index}\"}}\n"));
      }
      std::fs::write(archive_dir.join("chat_events.2025-W06.jsonl"), archive_body).unwrap();
      std::fs::write(&path, b"{\"id\": \"live-0\"}\n{\"id\": \"live-1\"}\n{\"id\": \"live-2\"}\n").unwrap();

      let mut meta = store.read_metadata_fresh(session_id).unwrap().unwrap();
      meta.archive_offset = 2;
      let metadata_path = store.metadata_path(session_id);
      std::fs::write(&metadata_path, meta.to_json_string(&store.slots, true)).unwrap();

      let facts = store.load_all_facts(session_id).unwrap();
      assert_eq!(ids(&facts), ["archived-0", "archived-1", "live-0", "live-1", "live-2"]);
    }

    #[tokio::test]
    async fn history_view_chains_archived_and_live_events_without_cloning_live_values() {
      let home = tempfile::tempdir().unwrap();
      let store = test_store(home.path());
      let session_id = "synthetic-session";
      seed_metadata(&store, session_id);
      let path = store.chat_events_path(session_id);
      let archive_dir = path.parent().unwrap().join("archives");
      std::fs::create_dir_all(&archive_dir).unwrap();
      std::fs::write(
        archive_dir.join("chat_events.2025-W06.jsonl"),
        b"{\"id\":\"archived-0\"}\n{\"id\":\"archived-1\"}\n",
      )
      .unwrap();
      std::fs::write(&path, b"{\"id\":\"live-0\"}\n{\"id\":\"live-1\"}\n{\"id\":\"live-2\"}\n").unwrap();
      let mut meta = store.read_metadata_fresh(session_id).unwrap().unwrap();
      meta.archive_offset = 2;
      let metadata_path = store.metadata_path(session_id);
      std::fs::write(&metadata_path, meta.to_json_string(&store.slots, true)).unwrap();
      let before = store.history_work();

      let history = store.load_all_facts_view_sync(session_id).unwrap();

      assert_eq!(history.len(), 5);
      let event_ids = history.iter().map(|event| event["id"].as_str().unwrap()).collect::<Vec<_>>();
      assert_eq!(event_ids, ["archived-0", "archived-1", "live-0", "live-1", "live-2"]);
      assert_eq!(store.history_work().owned_event_clones - before.owned_event_clones, 0);
    }

    /// `core/test_ndjson.py`'s count contract through the store: the total is the archive offset
    /// plus the live file's physical lines, an unterminated final line included.
    #[tokio::test]
    async fn event_count_is_the_offset_plus_physical_live_lines() {
      let home = tempfile::tempdir().unwrap();
      let store = test_store(home.path());
      let session_id = "synthetic-session";
      seed_metadata(&store, session_id);
      let path = store.chat_events_path(session_id);
      std::fs::create_dir_all(path.parent().unwrap()).unwrap();
      std::fs::write(&path, b"{\"id\": \"a\"}\n{bad}\n\n{\"id\": \"b\"}").unwrap();
      assert_eq!(store.get_chat_event_count_sync(session_id).unwrap(), 4);
    }

    /// `core/test_ndjson.py::test_count_ndjson_lines_memo_recounts_after_append` at store level: the
    /// count follows a line appended outside the store.
    #[tokio::test]
    async fn event_count_recounts_after_an_external_append() {
      let home = tempfile::tempdir().unwrap();
      let store = test_store(home.path());
      let session_id = "synthetic-session";
      seed_metadata(&store, session_id);
      let path = store.chat_events_path(session_id);
      std::fs::create_dir_all(path.parent().unwrap()).unwrap();
      std::fs::write(&path, "{\"i\": 0}\n{\"i\": 1}\n{\"i\": 2}\n{\"i\": 3}\n{\"i\": 4}\n").unwrap();
      assert_eq!(store.get_chat_event_count_sync(session_id).unwrap(), 5);
      let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
      std::io::Write::write_all(&mut file, b"{\"i\": 5}\n").unwrap();
      assert_eq!(store.get_chat_event_count_sync(session_id).unwrap(), 6);
    }

    #[test]
    fn tail_reads_and_parses_only_its_bounded_window_then_hits_the_stat_memo() {
      let home = tempfile::tempdir().unwrap();
      let store = test_store(home.path());
      let session_id = "synthetic-session";
      let path = store.chat_events_path(session_id);
      std::fs::create_dir_all(path.parent().unwrap()).unwrap();
      let mut body = String::new();
      for index in 0..30_000 {
        body.push_str(&format!("{{\"id\": \"event-{index}\", \"content\": \"{}\"}}\n", "x".repeat(48)));
      }
      std::fs::write(&path, body).unwrap();

      let (tail, total, has_more) = store.load_chat_events_tail(session_id, 10).unwrap();
      assert_eq!(total, 30_000);
      assert!(has_more);
      assert_eq!(tail.first().unwrap()["id"].as_str(), Some("event-29990"));
      assert_eq!(tail.last().unwrap()["id"].as_str(), Some("event-29999"));
      let (bytes, parsed) = {
        let caches = store.event_read_caches.lock().unwrap();
        (caches.tail_bytes_read, caches.tail_lines_parsed)
      };
      assert!(bytes <= TAIL_WINDOW_SIZE);
      assert_eq!(parsed, 10);

      assert_eq!(store.load_chat_events_tail(session_id, 10).unwrap().0, tail);
      let caches = store.event_read_caches.lock().unwrap();
      assert_eq!(caches.tail_bytes_read, bytes);
      assert_eq!(caches.tail_lines_parsed, parsed);
    }

    #[test]
    fn first_page_after_recovery_eviction_reuses_state_and_parses_only_the_tail() {
      let home = tempfile::tempdir().unwrap();
      let store = test_store(home.path());
      let session_id = "synthetic-session";
      let path = store.chat_events_path(session_id);
      std::fs::create_dir_all(path.parent().unwrap()).unwrap();
      let mut body = String::new();
      for index in 0..20_000 {
        body.push_str(&format!("{{\"id\":\"event-{index}\",\"text\":\"{}\"}}\n", "x".repeat(40)));
      }
      std::fs::write(&path, body).unwrap();

      store.node_facts(session_id).unwrap();
      assert_eq!(store.get_chat_event_count_sync(session_id).unwrap(), 20_000);
      store.evict_recovery_history(session_id);
      let reads = store.history_work().file_reads;

      let (tail, total, more) = store.load_chat_events_tail(session_id, 10).unwrap();
      assert_eq!(total, 20_000);
      assert!(more);
      assert_eq!(tail.first().unwrap()["id"].as_str(), Some("event-19990"));
      let (bytes, parsed) = {
        let caches = store.event_read_caches.lock().unwrap();
        (caches.tail_bytes_read, caches.tail_lines_parsed)
      };
      assert!(bytes <= TAIL_WINDOW_SIZE);
      assert_eq!(parsed, 10);

      store.node_state(session_id).unwrap();
      assert_eq!(
        store.history_work().file_reads,
        reads,
        "the sealed state answers after the first page without a full read"
      );
    }

    #[test]
    fn archive_range_reuses_file_parse_and_directory_walk_memos() {
      let home = tempfile::tempdir().unwrap();
      let store = test_store(home.path());
      let session_id = "synthetic-session";
      let archive_dir = store.session_dir(session_id).join("data").join("archives");
      std::fs::create_dir_all(&archive_dir).unwrap();
      std::fs::write(archive_dir.join("chat_events.2025-W05.jsonl"), "{\"id\": \"first\"}\n").unwrap();
      std::fs::write(archive_dir.join("chat_events.2025-W06.jsonl"), "{\"id\": \"second\"}\n").unwrap();

      assert_eq!(shared_ids(&store.load_archive_range(session_id, 0, 1).unwrap()), ["first"]);
      assert_eq!(shared_ids(&store.load_archive_range(session_id, 0, 1).unwrap()), ["first"]);
      let caches = store.event_read_caches.lock().unwrap();
      assert_eq!(caches.archive_file_reads, 1);
      assert_eq!(caches.archive_dir_walks, 1);
    }

    #[test]
    fn archived_live_range_parses_only_requested_suffix_and_reuses_it() {
      let home = tempfile::tempdir().unwrap();
      let store = test_store(home.path());
      let session_id = "synthetic-session";
      let mut meta = SessionMetadata::new(session_id, "seed", crate::metadata::TaskProfile::Manager);
      meta.archive_offset = 1;
      let metadata_path = store.metadata_path(session_id);
      std::fs::create_dir_all(metadata_path.parent().unwrap()).unwrap();
      std::fs::write(&metadata_path, meta.to_json_string(&store.slots, true)).unwrap();
      let path = store.chat_events_path(session_id);
      std::fs::create_dir_all(path.parent().unwrap()).unwrap();
      let mut body = String::new();
      for index in 0..30_000 {
        body.push_str(&format!("{{\"id\": \"live-{index}\", \"content\": \"{}\"}}\n", "x".repeat(48)));
      }
      std::fs::write(&path, body).unwrap();

      let (page, _) = store.load_chat_events_range(session_id, 1 + 29_990, 1 + 30_000).unwrap();
      assert_eq!(ids(&page).first().unwrap(), "live-29990");
      assert_eq!(page.len(), 10);
      let (bytes, parsed) = {
        let caches = store.event_read_caches.lock().unwrap();
        (caches.live_range_bytes_read, caches.live_range_lines_parsed)
      };
      assert!(bytes <= TAIL_WINDOW_SIZE);
      assert_eq!(parsed, 10);

      assert_eq!(store.load_chat_events_range(session_id, 1 + 29_990, 1 + 30_000).unwrap().0, page);
      let caches = store.event_read_caches.lock().unwrap();
      assert_eq!(caches.live_range_bytes_read, bytes);
      assert_eq!(caches.live_range_lines_parsed, parsed);
    }

    #[test]
    fn live_range_memo_returns_the_shared_parsed_lines() {
      let home = tempfile::tempdir().unwrap();
      let store = test_store(home.path());
      let session_id = "synthetic-session";
      let path = store.chat_events_path(session_id);
      std::fs::create_dir_all(path.parent().unwrap()).unwrap();
      std::fs::write(&path, b"{\"id\":\"first\"}\n{\"id\":\"second\"}\n").unwrap();

      let first = store.live_range_lines(session_id, 1).unwrap().0;
      let second = store.live_range_lines(session_id, 1).unwrap().0;

      assert!(Arc::ptr_eq(&first, &second), "memo hits share the parsed suffix instead of cloning its events");
    }

    #[test]
    fn physical_event_count_memo_reads_bytes_once_and_recounts_after_append() {
      let home = tempfile::tempdir().unwrap();
      let store = test_store(home.path());
      let session_id = "synthetic-session";
      seed_metadata(&store, session_id);
      let path = store.chat_events_path(session_id);
      std::fs::create_dir_all(path.parent().unwrap()).unwrap();
      std::fs::write(&path, "{\"i\": 0}\n".repeat(10_000)).unwrap();

      assert_eq!(store.get_chat_event_count_sync(session_id).unwrap(), 10_000);
      let bytes = store.event_read_caches.lock().unwrap().count_bytes_read;
      assert!(bytes > 0);
      assert_eq!(store.get_chat_event_count_sync(session_id).unwrap(), 10_000);
      assert_eq!(store.event_read_caches.lock().unwrap().count_bytes_read, bytes);

      let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
      std::io::Write::write_all(&mut file, b"{\"i\": 10000}\n").unwrap();
      assert_eq!(store.get_chat_event_count_sync(session_id).unwrap(), 10_001);
      assert!(store.event_read_caches.lock().unwrap().count_bytes_read > bytes);
    }
  }
}
