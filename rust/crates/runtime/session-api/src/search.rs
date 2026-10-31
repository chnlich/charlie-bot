//! Read-only session name and chat-content search.

use std::cmp::Reverse;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::num::NonZeroUsize;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use charliebot_store::metadata::{SessionMetadata, SessionStatus};
use charliebot_store::{Store, StoreResult};
use lru::LruCache;

const SEARCH_RESULT_LIMIT: usize = 200;
const SEARCH_FILE_MEMO_LIMIT: usize = 4096;
const SEARCH_ROOTS_PER_FILE: usize = 8;
const SEARCH_MATCH_MEMO_LIMIT: usize = 8;
const SEARCH_CHUNK_SIZE: usize = 1 << 18;
/// The threads one signature batch may split across.
///
/// Four threads cut the batch's wall time by about two thirds on the 7,160-session home while its
/// total CPU stayed inside the run-to-run spread; eight threads raised the total CPU and helped less.
const SIGNATURE_BATCH_WORKERS: usize = 4;
/// The batch length below which the signatures are read on the calling thread.
///
/// Starting a thread costs more than the `stat` calls a short batch makes, so only a batch that a
/// production home produces takes the parallel path.
const SIGNATURE_BATCH_MIN_PATHS: usize = 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileSignature {
  mtime_ns: i128,
  size: u64,
  inode: u64,
}

/// One file's needle roots, newest needles kept: the same bounded per-needle list Python's per-file memo holds.
type RootMemo = LruCache<String, FileSignature>;

struct SearchFileRoots {
  misses: RootMemo,
  hits: RootMemo,
}

impl SearchFileRoots {
  fn new() -> Self {
    let limit = || NonZeroUsize::new(SEARCH_ROOTS_PER_FILE).expect("the per-file root limit is nonzero");
    Self { misses: LruCache::new(limit()), hits: LruCache::new(limit()) }
  }
}

#[derive(Clone)]
struct SearchMatch {
  /// The corpus the matched rows were taken from. A rebuilt listing answers a new `Arc`, so a pointer match names the
  /// same rows.
  corpus: Arc<Vec<Arc<SessionMetadata>>>,
  /// The chat files a repeat query re-checks: every content candidate while the result is short of the cap, and only
  /// the candidates at or above the last returned row's `updated_at` once the result reaches it.
  watched: Vec<PathBuf>,
  signatures: Vec<Option<FileSignature>>,
  /// The matched rows, newest first, capped at [`SEARCH_RESULT_LIMIT`].
  rows: Vec<Arc<SessionMetadata>>,
}

struct SearchMemos {
  files: LruCache<PathBuf, SearchFileRoots>,
  matches: LruCache<String, Arc<SearchMatch>>,
  read_failures: HashSet<(String, String)>,
  #[cfg(test)]
  scan_calls: usize,
}

impl Default for SearchMemos {
  fn default() -> Self {
    let files = LruCache::new(NonZeroUsize::new(SEARCH_FILE_MEMO_LIMIT).expect("the file memo limit is nonzero"));
    let matches = LruCache::new(NonZeroUsize::new(SEARCH_MATCH_MEMO_LIMIT).expect("the match memo limit is nonzero"));
    Self {
      files,
      matches,
      read_failures: HashSet::new(),
      #[cfg(test)]
      scan_calls: 0,
    }
  }
}

/// One verdict helper's root list: needle-and-signature pairs whose order the helpers do not care about.
type RootList = Vec<(String, FileSignature)>;

impl SearchMemos {
  /// The miss and hit roots of one file. Their order is irrelevant: the verdict helpers only scan them.
  fn roots(&mut self, path: &Path) -> (RootList, RootList) {
    let Some(entry) = self.files.get_mut(path) else { return (Vec::new(), Vec::new()) };
    let collect = |roots: &RootMemo| roots.iter().map(|(needle, signature)| (needle.clone(), *signature)).collect();
    (collect(&entry.misses), collect(&entry.hits))
  }

  fn remember_root(&mut self, path: &Path, needle: &str, signature: FileSignature, hit: bool) {
    let entry = match self.files.get_mut(path) {
      Some(entry) => entry,
      None => {
        self.files.put(path.to_path_buf(), SearchFileRoots::new());
        self.files.get_mut(path).expect("the entry was just inserted")
      }
    };
    let roots = if hit { &mut entry.hits } else { &mut entry.misses };
    roots.put(needle.to_string(), signature);
  }

  fn cached_match(&mut self, query: &str) -> Option<Arc<SearchMatch>> {
    self.matches.get(query).cloned()
  }

  fn remember_match(&mut self, query: &str, entry: SearchMatch) {
    self.matches.put(query.to_string(), Arc::new(entry));
  }
}

static SEARCH_MEMOS: LazyLock<Mutex<SearchMemos>> = LazyLock::new(|| Mutex::new(SearchMemos::default()));

/// Search every session name and active-session chat content, newest first.
pub(crate) fn search_sessions(store: &Store, query: &str) -> StoreResult<Vec<Arc<SessionMetadata>>> {
  search_sessions_with_memos(store, query, &SEARCH_MEMOS)
}

fn search_sessions_with_memos(
  store: &Store,
  query: &str,
  memos: &Mutex<SearchMemos>,
) -> StoreResult<Vec<Arc<SessionMetadata>>> {
  if query.trim().is_empty() {
    return Ok(store.list_sessions(Some(SessionStatus::Active), None, None)?.rows);
  }

  let needle = query.to_lowercase();
  let (all, _) = store.load_session_metas_snapshot_versioned(None)?;
  // Python checks the query result's listing identity, then each stored file signature, before doing name matching or
  // content classification (`src/runtime/session_search.py:293-295`). Keep that hot path ahead of candidate rebuilds.
  if let Some(cached) = memos.lock().expect("session search memo mutex is not poisoned").cached_match(&needle)
    && Arc::ptr_eq(&cached.corpus, &all)
  {
    let watched: Vec<&Path> = cached.watched.iter().map(PathBuf::as_path).collect();
    let signatures: Vec<Option<FileSignature>> = file_signatures(&watched).into_iter().map(Result::ok).collect();
    if cached.signatures == signatures {
      return Ok(cached.rows.clone());
    }
  }

  let mut names = Vec::new();
  let mut content_candidates = Vec::new();
  for meta in all.iter() {
    if meta.name.to_lowercase().contains(&needle) {
      names.push(meta);
    } else if meta.status == SessionStatus::Active {
      content_candidates.push((meta, store.chat_events_path(&meta.id)));
    }
  }
  names.sort_by_key(|left| Reverse(left.updated_at));
  names.truncate(SEARCH_RESULT_LIMIT);
  let candidate_paths: Vec<&Path> = content_candidates.iter().map(|(_, path)| path.as_path()).collect();
  let signatures: Vec<Option<FileSignature>> = file_signatures(&candidate_paths)
    .into_iter()
    .zip(&content_candidates)
    .map(|(result, (meta, _))| match result {
      Ok(signature) => Some(signature),
      Err(error) => {
        log_search_read_failed_once(memos, &meta.id, &error);
        None
      }
    })
    .collect();

  let mut content_hits = Vec::new();
  let mut scan_failures = 0;
  for ((meta, path), signature) in content_candidates.iter().zip(&signatures) {
    let Some(signature) = signature else { continue };
    let (miss_roots, hit_roots) = memos.lock().expect("session search memo mutex is not poisoned").roots(path);
    if hit_root_covers(&hit_roots, *signature, &needle) {
      content_hits.push(*meta);
      continue;
    }
    let Some(start) = absence_rescan_start(&miss_roots, *signature, &needle) else {
      continue;
    };
    #[cfg(test)]
    {
      memos.lock().expect("session search memo mutex is not poisoned").scan_calls += 1;
    }
    match scan_content(path, &needle, start) {
      Ok(true) => {
        memos.lock().expect("session search memo mutex is not poisoned").remember_root(path, &needle, *signature, true);
        content_hits.push(*meta);
      }
      Ok(false) => {
        memos.lock().expect("session search memo mutex is not poisoned").remember_root(path, &needle, *signature, false)
      }
      Err(error) => {
        scan_failures += 1;
        log_search_read_failed_once(memos, &meta.id, &error);
      }
    }
  }

  names.extend(content_hits);
  names.sort_by_key(|left| Reverse(left.updated_at));
  names.truncate(SEARCH_RESULT_LIMIT);
  if scan_failures == 0 {
    // The result holds at most the newest `SEARCH_RESULT_LIMIT` rows, so a candidate older than the last row cannot
    // enter a full result however its chat file changes. A repeat query re-checks only the candidates that could
    // enter it; a short result can still take any candidate, so it keeps watching all of them.
    let cutoff = names.get(SEARCH_RESULT_LIMIT - 1).map(|meta| meta.updated_at);
    let mut watched = Vec::new();
    let mut watched_signatures = Vec::new();
    for ((meta, path), signature) in content_candidates.iter().zip(signatures) {
      if cutoff.is_none_or(|cutoff| meta.updated_at >= cutoff) {
        watched.push(path.clone());
        watched_signatures.push(signature);
      }
    }
    memos.lock().expect("session search memo mutex is not poisoned").remember_match(
      &needle,
      SearchMatch {
        corpus: Arc::clone(&all),
        watched,
        signatures: watched_signatures,
        rows: names.iter().map(|meta| Arc::clone(meta)).collect(),
      },
    );
  }
  Ok(names.into_iter().map(Arc::clone).collect())
}

/// The signature of every path, in the argument order.
///
/// Each read is one `stat` and the answers do not depend on each other, so a batch a production home
/// produces runs on several threads. A repeat query re-checks every watched candidate, and on the
/// 7,160-session home those reads took about nine of the route's fourteen milliseconds.
fn file_signatures(paths: &[&Path]) -> Vec<io::Result<FileSignature>> {
  let workers = std::thread::available_parallelism().map_or(1, |count| count.get()).min(SIGNATURE_BATCH_WORKERS);
  if paths.len() < SIGNATURE_BATCH_MIN_PATHS || workers < 2 {
    return paths.iter().map(|path| file_signature_unlogged(path)).collect();
  }
  let chunk = paths.len().div_ceil(workers);
  std::thread::scope(|scope| {
    let chunks: Vec<_> = paths
      .chunks(chunk)
      .map(|chunk| scope.spawn(move || chunk.iter().map(|path| file_signature_unlogged(path)).collect::<Vec<_>>()))
      .collect();
    chunks.into_iter().flat_map(|chunk| chunk.join().expect("a signature chunk only stats files")).collect()
  })
}

fn file_signature_unlogged(path: &Path) -> io::Result<FileSignature> {
  let metadata = fs::metadata(path)?;
  Ok(FileSignature {
    mtime_ns: i128::from(metadata.mtime()) * 1_000_000_000 + i128::from(metadata.mtime_nsec()),
    size: metadata.size(),
    inode: metadata.ino(),
  })
}

fn log_search_read_failed_once(memos: &Mutex<SearchMemos>, session_id: &str, error: &io::Error) {
  let key = (session_id.to_string(), error.to_string());
  if memos.lock().expect("session search memo mutex is not poisoned").read_failures.insert(key) {
    tracing::debug!(session_id, error = %error, "search_read_failed");
  }
}

fn hit_root_covers(roots: &[(String, FileSignature)], signature: FileSignature, needle: &str) -> bool {
  roots.iter().any(|(root, root_signature)| {
    root.contains(needle) && root_signature.inode == signature.inode && signature.size >= root_signature.size
  })
}

/// `None` means a stored absence already proves this query; otherwise return the byte offset to re-scan.
fn absence_rescan_start(roots: &[(String, FileSignature)], signature: FileSignature, needle: &str) -> Option<u64> {
  for (root, root_signature) in roots {
    if needle.starts_with(root) && *root_signature == signature {
      return None;
    }
  }
  let best_size = roots
    .iter()
    .filter(|(root, root_signature)| {
      needle.starts_with(root) && root_signature.inode == signature.inode && signature.size > root_signature.size
    })
    .map(|(_, root_signature)| root_signature.size)
    .max();
  Some(best_size.map_or(0, |size| size.saturating_sub((4 * needle.chars().count() + 8) as u64)))
}

fn scan_content(path: &Path, needle: &str, start: u64) -> io::Result<bool> {
  let mut file = File::open(path)?;
  file.seek(SeekFrom::Start(start))?;
  if needle.is_ascii() {
    return scan_ascii(&mut file, needle.as_bytes());
  }
  scan_unicode(&mut file, needle, start > 0)
}

fn scan_ascii(file: &mut File, needle: &[u8]) -> io::Result<bool> {
  let overlap = needle.len().saturating_sub(1);
  // memchr's memmem is the two-way search behind Python's `needle in window`
  // (src/runtime/session_search.py:188-194), built once per scan over the folded windows.
  let finder = memchr::memmem::Finder::new(needle);
  let mut tail = Vec::with_capacity(overlap);
  let mut chunk = vec![0_u8; SEARCH_CHUNK_SIZE];
  let mut window = Vec::with_capacity(SEARCH_CHUNK_SIZE + overlap);
  loop {
    let count = file.read(&mut chunk)?;
    if count == 0 {
      return Ok(false);
    }
    chunk[..count].make_ascii_lowercase();
    window.clear();
    window.extend_from_slice(&tail);
    window.extend_from_slice(&chunk[..count]);
    if finder.find(&window).is_some() {
      return Ok(true);
    }
    tail.clear();
    tail.extend_from_slice(&window[window.len().saturating_sub(overlap)..]);
  }
}

fn scan_unicode(file: &mut File, needle: &str, replace_invalid: bool) -> io::Result<bool> {
  let overlap = needle.chars().count().saturating_sub(1);
  let mut tail = String::new();
  let mut pending = Vec::new();
  let mut chunk = vec![0_u8; SEARCH_CHUNK_SIZE];
  loop {
    let count = file.read(&mut chunk)?;
    if count == 0 && pending.is_empty() {
      return Ok(false);
    }
    pending.extend_from_slice(&chunk[..count]);
    let decoded = match std::str::from_utf8(&pending) {
      Ok(text) => {
        let text = text.to_string();
        pending.clear();
        text
      }
      Err(error) if error.error_len().is_none() => {
        let complete = std::str::from_utf8(&pending[..error.valid_up_to()]).expect("valid prefix is UTF-8").to_string();
        pending.drain(..error.valid_up_to());
        if count == 0 {
          if replace_invalid {
            let suffix = String::from_utf8_lossy(&pending).into_owned();
            pending.clear();
            complete + &suffix
          } else {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "chat events are not valid UTF-8"));
          }
        } else {
          complete
        }
      }
      Err(_error) if replace_invalid => {
        let text = String::from_utf8_lossy(&pending).into_owned();
        pending.clear();
        text
      }
      Err(_) => return Err(io::Error::new(io::ErrorKind::InvalidData, "chat events are not valid UTF-8")),
    };
    let lowered = decoded.to_lowercase();
    let mut window = std::mem::take(&mut tail);
    window.push_str(&lowered);
    if window.contains(needle) {
      return Ok(true);
    }
    let start = window.char_indices().rev().nth(overlap).map_or(window.len(), |(index, _)| index);
    tail = window[start..].to_string();
    if count == 0 {
      return Ok(false);
    }
  }
}

#[cfg(test)]
mod tests {
  use std::collections::HashMap;
  use std::fs;
  use std::io::{Seek, SeekFrom, Write};
  use std::sync::{Arc, Mutex};

  use charliebot_store::hooks::EmptyRuntimeEventHooks;
  use charliebot_store::metadata::{SessionMetadata, SessionStatus, TaskProfile, UtcDateTime};
  use charliebot_store::{
    AccountLabelKeeper, AnchorFieldReconciler, SequenceArchiveHook, SequenceBindingMover, SequenceListingFields, Store,
    StoreHookFuture, StoreHooks, StoreResult,
  };
  use serde_json::{Map, Value};

  use super::search_sessions;

  #[test]
  fn ascii_scan_finds_matches_across_chunk_boundaries() {
    let mut file = tempfile::tempfile().unwrap();
    let mut body = vec![b'x'; super::SEARCH_CHUNK_SIZE - 4];
    body.extend_from_slice(b"Boundary-Needle");
    body.extend_from_slice(b"x");
    file.write_all(&body).unwrap();
    file.seek(SeekFrom::Start(0)).unwrap();

    assert!(super::scan_ascii(&mut file, b"boundary-needle").unwrap());
  }

  #[test]
  fn ascii_scan_reports_absence_and_a_match_at_the_first_byte() {
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(b"needle at the start, then synthetic filler").unwrap();
    file.seek(SeekFrom::Start(0)).unwrap();
    assert!(super::scan_ascii(&mut file, b"needle at").unwrap());
    file.seek(SeekFrom::Start(0)).unwrap();
    assert!(!super::scan_ascii(&mut file, b"absent-needle").unwrap());
  }

  /// A batch keeps the argument order across its threads: a signature that moved to another file's
  /// index would compare one candidate's file against another's and keep a stale verdict forever.
  #[test]
  fn signature_batch_answers_in_argument_order() {
    let home = tempfile::tempdir().unwrap();
    let mut paths = Vec::new();
    for index in 0..super::SIGNATURE_BATCH_MIN_PATHS + 37 {
      let path = home.path().join(format!("chat-{index:05}.jsonl"));
      // Every 97th path stays absent, so a failed read sits between successful answers.
      if index % 97 != 0 {
        fs::write(&path, format!("event {index}\n")).unwrap();
      }
      paths.push(path);
    }
    let refs: Vec<&std::path::Path> = paths.iter().map(std::path::PathBuf::as_path).collect();
    let batch = super::file_signatures(&refs);
    assert_eq!(batch.len(), refs.len());
    for (index, (answer, path)) in batch.iter().zip(&refs).enumerate() {
      if index % 97 == 0 {
        assert!(answer.is_err(), "path {index} is absent and its answer reports the failed read");
        continue;
      }
      let signature = answer.as_ref().expect("a written path reads");
      assert_eq!(signature.size, (format!("event {index}\n").len() as u64), "path {index} answers for itself");
      assert_eq!(*signature, super::file_signature_unlogged(path).unwrap(), "path {index} matches a direct read");
    }
  }

  struct TestLabels;

  impl AccountLabelKeeper for TestLabels {
    fn record_account_label(&self, _meta: &mut SessionMetadata, _label: Option<&str>) -> StoreResult<bool> {
      Ok(false)
    }

    fn account_label(&self, _meta: &SessionMetadata) -> Option<String> {
      None
    }
  }

  struct TestAnchors;

  impl AnchorFieldReconciler for TestAnchors {
    fn reconcile(&self, _disk: &SessionMetadata, _attempted: &mut SessionMetadata) -> StoreResult<()> {
      Ok(())
    }
  }

  struct TestSequence;

  impl SequenceListingFields for TestSequence {
    fn owns_session(&self, _meta: &SessionMetadata) -> bool {
      false
    }

    fn listing_fields(
      &self,
      _session_ids: &[String],
      _now: UtcDateTime,
    ) -> StoreResult<HashMap<String, Map<String, Value>>> {
      Ok(HashMap::new())
    }

    fn sequence_subtree_roots(&self, _metas: &[Arc<SessionMetadata>]) -> StoreResult<HashMap<String, String>> {
      Ok(HashMap::new())
    }

    fn view_subtree_roots(
      &self,
      _metas: &[Arc<SessionMetadata>],
    ) -> StoreResult<HashMap<String, HashMap<String, String>>> {
      Ok(HashMap::new())
    }
  }

  struct TestArchive;

  impl SequenceArchiveHook for TestArchive {
    fn on_archive<'a>(&'a self, _session_id: &'a str) -> StoreHookFuture<'a, ()> {
      Box::pin(async { Ok(()) })
    }
  }

  struct TestBindingMover;

  impl SequenceBindingMover for TestBindingMover {
    fn move_bindings<'a>(&'a self, _source: &'a str, _new: &'a str) -> StoreHookFuture<'a, bool> {
      Box::pin(async { Ok(false) })
    }
  }

  fn test_store(home: &std::path::Path) -> Store {
    let config = charliebot_core::config::load_config(home, &charliebot_core::config::ConfigSchema::default()).unwrap();
    Store::new(
      config,
      Vec::new(),
      StoreHooks {
        account_labels: Some(Arc::new(TestLabels)),
        anchor_reconciler: Some(Arc::new(TestAnchors)),
        sequence_listing: Some(Arc::new(TestSequence)),
        runtime_events: Some(Arc::new(EmptyRuntimeEventHooks)),
        sequence_archive: Some(Arc::new(TestArchive)),
        sequence_binding_mover: Some(Arc::new(TestBindingMover)),
      },
    )
    .unwrap()
  }

  fn write_session(store: &Store, id: &str, name: &str, status: SessionStatus, updated_at: &str) {
    let mut meta = SessionMetadata::new(id, name, TaskProfile::Manager);
    meta.status = status;
    meta.updated_at = UtcDateTime::parse(updated_at).unwrap();
    let path = store.metadata_path(id);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, meta.to_json_string(&[], true)).unwrap();
  }

  #[test]
  fn blank_search_returns_active_sessions_newest_first() {
    let home = tempfile::tempdir().unwrap();
    let store = test_store(home.path());
    write_session(&store, "old", "old", SessionStatus::Active, "2026-08-01T12:00:00Z");
    write_session(&store, "archived", "archived", SessionStatus::Archived, "2026-08-01T12:02:00Z");
    write_session(&store, "new", "new", SessionStatus::Active, "2026-08-01T12:01:00Z");

    let rows = search_sessions(&store, "  \t").unwrap();
    assert_eq!(rows.iter().map(|meta| meta.id.as_str()).collect::<Vec<_>>(), ["new", "old"]);
  }

  /// `test_archived_pagination.py::test_search_cap_keeps_content_hits_above_the_cap_line`.
  #[test]
  fn content_hits_compete_with_name_hits_at_the_two_hundred_row_cap() {
    let home = tempfile::tempdir().unwrap();
    let store = test_store(home.path());
    for index in 0..200 {
      write_session(
        &store,
        &format!("archived-{index:03}"),
        &format!("needle-{index:03}"),
        SessionStatus::Archived,
        &format!("2026-08-01T{:02}:{:02}:00Z", 12 + index / 60, index % 60),
      );
    }
    write_session(&store, "above", "unrelated above", SessionStatus::Active, "2026-08-02T04:40:00Z");
    write_session(&store, "below", "unrelated below", SessionStatus::Active, "2026-08-01T11:59:00Z");
    for id in ["above", "below"] {
      let path = store.chat_events_path(id);
      fs::create_dir_all(path.parent().unwrap()).unwrap();
      fs::write(path, "{\"content\":\"needle in the events\"}\n").unwrap();
    }

    let rows = search_sessions(&store, "needle").unwrap();
    let ids: Vec<&str> = rows.iter().map(|meta| meta.id.as_str()).collect();
    assert_eq!(rows.len(), 200);
    assert!(ids.contains(&"above"));
    assert!(!ids.contains(&"below"));
    assert!(!ids.contains(&"archived-000"));
  }

  /// `test_archived_pagination.py::test_search_match_memo_refreshes_when_chat_content_or_names_move`.
  #[test]
  fn search_reads_new_chat_content_and_names_on_the_next_request() {
    let home = tempfile::tempdir().unwrap();
    let store = test_store(home.path());
    write_session(&store, "quiet", "quiet", SessionStatus::Active, "2026-08-01T12:00:00Z");
    write_session(&store, "carrier", "carrier", SessionStatus::Active, "2026-08-01T12:01:00Z");
    assert!(search_sessions(&store, "needle").unwrap().is_empty());

    let events = store.chat_events_path("carrier");
    fs::create_dir_all(events.parent().unwrap()).unwrap();
    fs::write(&events, "{\"content\":\"needle in the events\"}\n").unwrap();
    let content_hits = search_sessions(&store, "needle").unwrap();
    assert_eq!(content_hits.iter().map(|meta| meta.id.as_str()).collect::<Vec<_>>(), ["carrier"]);

    write_session(&store, "named", "needle name", SessionStatus::Archived, "2026-08-01T12:02:00Z");
    let all_hits = search_sessions(&store, "needle").unwrap();
    assert_eq!(all_hits.iter().map(|meta| meta.id.as_str()).collect::<Vec<_>>(), ["named", "carrier"]);
  }

  #[test]
  fn search_reuses_query_and_file_verdicts_until_the_chat_file_changes() {
    let home = tempfile::tempdir().unwrap();
    let store = test_store(home.path());
    write_session(&store, "content", "content session", SessionStatus::Active, "2026-08-01T12:00:00Z");
    let path = store.chat_events_path("content");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "no match yet").unwrap();
    let memos = Mutex::new(super::SearchMemos::default());

    assert!(super::search_sessions_with_memos(&store, "needle", &memos).unwrap().is_empty());
    assert!(super::search_sessions_with_memos(&store, "needle", &memos).unwrap().is_empty());
    assert_eq!(memos.lock().unwrap().scan_calls, 1, "the repeated query does no second file scan");

    fs::write(&path, "no match yet; needle appears").unwrap();
    let hit = super::search_sessions_with_memos(&store, "needle", &memos).unwrap();
    assert_eq!(hit.iter().map(|meta| meta.id.as_str()).collect::<Vec<_>>(), ["content"]);
    let repeat = super::search_sessions_with_memos(&store, "needle", &memos).unwrap();
    assert!(Arc::ptr_eq(&hit[0], &repeat[0]), "the repeat query answers with the rows the memo holds");
    assert_eq!(super::search_sessions_with_memos(&store, "eed", &memos).unwrap().len(), 1);
    assert_eq!(memos.lock().unwrap().scan_calls, 2, "the changed file rescans once and its hit covers substrings");
  }

  /// A full result re-checks only the candidates at or above its last row: a new match in an older session cannot
  /// enter the 200-row result, while a new match in a newer candidate does.
  #[test]
  fn a_full_result_watches_only_candidates_at_or_above_its_last_row() {
    let home = tempfile::tempdir().unwrap();
    let store = test_store(home.path());
    let write_content = |id: &str, content: &str| {
      let path = store.chat_events_path(id);
      fs::create_dir_all(path.parent().unwrap()).unwrap();
      fs::write(&path, content).unwrap();
    };
    for index in 0..super::SEARCH_RESULT_LIMIT {
      let id = format!("hit-{index:03}");
      write_session(&store, &id, &format!("hit {index:03}"), SessionStatus::Active, "2026-08-02T12:00:00Z");
      write_content(&id, "{\"content\":\"walrus in the events\"}\n");
    }
    write_session(&store, "old", "old session", SessionStatus::Active, "2026-08-01T12:00:00Z");
    write_session(&store, "newer", "newer session", SessionStatus::Active, "2026-08-03T12:00:00Z");

    let rows = search_sessions(&store, "walrus").unwrap();
    assert_eq!(rows.len(), super::SEARCH_RESULT_LIMIT);
    assert!(rows.iter().all(|meta| meta.id.starts_with("hit-")));

    write_content("old", "{\"content\":\"walrus in the events\"}\n");
    let rows = search_sessions(&store, "walrus").unwrap();
    assert_eq!(rows.len(), super::SEARCH_RESULT_LIMIT);
    assert!(!rows.iter().any(|meta| meta.id == "old"), "a match older than the last row stays out of the full result");

    write_content("newer", "{\"content\":\"walrus in the events\"}\n");
    let rows = search_sessions(&store, "walrus").unwrap();
    assert_eq!(rows.len(), super::SEARCH_RESULT_LIMIT);
    assert_eq!(rows[0].id, "newer", "a new match in a watched candidate enters the result");
    assert!(!rows.iter().any(|meta| meta.id == "old"));
  }
}
