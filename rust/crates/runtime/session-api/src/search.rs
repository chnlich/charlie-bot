//! Read-only session name and chat-content search.

use std::cmp::Reverse;
use std::collections::HashSet;
use std::ffi::CString;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::num::NonZeroUsize;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, LazyLock, Mutex};

use charliebot_core::timefmt;
use charliebot_store::metadata::{SessionMetadata, SessionStatus};
use charliebot_store::{Store, StoreResult};
use lru::LruCache;

const SEARCH_RESULT_LIMIT: usize = 200;
const SEARCH_FILE_MEMO_LIMIT: usize = 4096;
const SEARCH_ROOTS_PER_FILE: usize = 8;
const SEARCH_MATCH_MEMO_LIMIT: usize = 8;
const SEARCH_CHUNK_SIZE: usize = 1 << 18;
/// The number of lowercase ASCII letters a trigram position may take.
const TRIGRAM_LETTERS: u8 = 26;
/// The code of a byte that is not a lowercase ASCII letter: the index cannot hold its trigrams.
const TRIGRAM_NONE: u8 = TRIGRAM_LETTERS;
/// Every three-letter sequence's bit in one file's content index.
const TRIGRAM_BITS: usize = 26 * 26 * 26;
/// The bytes one file's content index takes.
const TRIGRAM_BYTES: usize = TRIGRAM_BITS.div_ceil(8);
/// The files whose content indexes the memo keeps.
///
/// One index is 2,197 bytes and the 7,160-session mock home takes about 16 MB. The cap exceeds that count, so a
/// fresh query does not evict the indexes its next query would reuse.
const SEARCH_TRIGRAM_MEMO_LIMIT: usize = 16_384;
/// The threads one signature batch may split across.
///
/// The batch is one `stat` per watched file and the route waits on its slowest chunk. On the 7,160-session home,
/// sixteen threads cut the sweep's wall time by about two fifths against eight, and more threads than that gained
/// nothing.
const SIGNATURE_BATCH_WORKERS: usize = 16;
/// The fewest paths one pool worker takes, so a short batch does not pay a message per worker.
///
/// A repeat query watches one file per returned row, 200 on the mock home. A chunk wakes its worker and the reply
/// wakes the caller, so four chunks of 50 cost less CPU than eight of 25 while still cutting the batch's wall time.
const SIGNATURE_BATCH_CHUNK_FLOOR: usize = 50;
/// The batch length below which the signatures are read on the calling thread.
///
/// A dispatched chunk costs a message round trip, so only a batch long enough to pay it takes the pool.
const SIGNATURE_BATCH_MIN_PATHS: usize = 32;
/// The threads one content-scan batch may split across.
///
/// A first query scans every active session's chat file: 5.5 GB on the 7,160-session mock home, which one
/// thread spends about 0.6 s on. The scans read different files and do not depend on each other.
const SCAN_BATCH_WORKERS: usize = 16;
/// The fewest scans a batch runs on its own threads: starting them costs more than a short batch saves.
const SCAN_BATCH_MIN_JOBS: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileSignature {
  mtime_ns: i128,
  size: u64,
  inode: u64,
}

/// One watched chat file: the path a content scan opens, and its path below the sessions directory,
/// which a signature read resolves from that directory's open handle.
///
/// `below_sessions` is None when a store's sessions directory does not contain the path, so the read
/// falls back to the path form.
#[derive(Clone)]
struct WatchedFile {
  path: PathBuf,
  below_sessions: Option<CString>,
}

/// The content index's code per byte: 0 to 25 for `a` to `z`, and [`TRIGRAM_NONE`] for every other byte.
static TRIGRAM_CODES: LazyLock<[u8; 256]> = LazyLock::new(|| {
  let mut codes = [TRIGRAM_NONE; 256];
  for (value, code) in codes.iter_mut().enumerate() {
    let byte = value as u8;
    if byte.is_ascii_lowercase() {
      *code = byte - b'a';
    }
  }
  codes
});

/// One file's content index: a bit per three-letter sequence its lowercase ASCII text holds.
///
/// `signature` is the file's identity when the index was built. A file whose identity moved needs its index rebuilt,
/// because text that grew or changed can hold trigrams the old index lacks.
struct TrigramEntry {
  signature: FileSignature,
  bits: Box<[u8]>,
}

/// One file's content index and whether the needle occurs in it, or the read that failed.
type ContentIndexAnswer = io::Result<(Box<[u8]>, bool)>;

/// What a file's content index says about a query.
enum TrigramVerdict {
  /// No index holds this file at its current signature.
  Missing,
  /// The index cannot rule the file out, so the file is scanned.
  Possible,
  /// A trigram of the needle is absent from the file, so the needle is absent from the file too.
  Absent,
}

/// One file's needle roots, newest needles kept: the same bounded per-needle list the per-file memo holds.
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

pub(crate) struct SearchMatch {
  /// The corpus the matched rows were taken from. A rebuilt listing answers a new `Arc`, so a pointer match names the
  /// same rows.
  corpus: Arc<Vec<Arc<SessionMetadata>>>,
  /// The chat files a repeat query re-checks: every content candidate while the result is short of the cap, and only
  /// the candidates at or above the last returned row's `updated_at` once the result reaches it.
  ///
  /// The batch shares this list instead of copying it, so a worker thread reads the memo's own paths.
  watched: Arc<Vec<WatchedFile>>,
  signatures: Vec<Option<FileSignature>>,
  /// The matched rows, newest first, capped at [`SEARCH_RESULT_LIMIT`].
  rows: Vec<Arc<SessionMetadata>>,
  /// The response body rendered from `rows`, with the derived-state generation it holds for.
  body: Mutex<Option<SearchBody>>,
}

/// One rendered search response and the derived-state generation its overlay fields came from.
struct SearchBody {
  generation: u64,
  bytes: Arc<Vec<u8>>,
}

impl SearchMatch {
  /// The body this match built for `generation`, when it built one for that generation.
  pub(crate) fn body(&self, generation: u64) -> Option<Arc<Vec<u8>>> {
    let body = self.body.lock().expect("search body mutex is not poisoned");
    body.as_ref().filter(|body| body.generation == generation).map(|body| Arc::clone(&body.bytes))
  }

  /// Keep the body built for `generation` for the repeat requests that generation still answers.
  pub(crate) fn remember_body(&self, generation: u64, bytes: Arc<Vec<u8>>) {
    *self.body.lock().expect("search body mutex is not poisoned") = Some(SearchBody { generation, bytes });
  }
}

/// One search answer: its rows and the memoized result they belong to.
pub(crate) struct SearchAnswer {
  pub rows: Vec<Arc<SessionMetadata>>,
  /// The memoized result whose signature check held, or the one this pass just stored. `None` when a read failure kept
  /// this pass out of the memo, and for a blank query, which lists sessions instead of searching them.
  pub source: Option<Arc<SearchMatch>>,
}

struct SearchMemos {
  files: LruCache<PathBuf, SearchFileRoots>,
  trigrams: LruCache<PathBuf, TrigramEntry>,
  matches: LruCache<String, Arc<SearchMatch>>,
  read_failures: HashSet<(String, String)>,
  #[cfg(test)]
  scan_calls: usize,
}

impl Default for SearchMemos {
  fn default() -> Self {
    let files = LruCache::new(NonZeroUsize::new(SEARCH_FILE_MEMO_LIMIT).expect("the file memo limit is nonzero"));
    let trigrams =
      LruCache::new(NonZeroUsize::new(SEARCH_TRIGRAM_MEMO_LIMIT).expect("the trigram memo limit is nonzero"));
    let matches = LruCache::new(NonZeroUsize::new(SEARCH_MATCH_MEMO_LIMIT).expect("the match memo limit is nonzero"));
    Self {
      files,
      trigrams,
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

  /// What this file's content index says about a needle, given the needle's trigrams.
  fn trigram_verdict(&mut self, path: &Path, signature: FileSignature, trigrams: &[usize]) -> TrigramVerdict {
    let Some(entry) = self.trigrams.get(path) else { return TrigramVerdict::Missing };
    if entry.signature != signature {
      return TrigramVerdict::Missing;
    }
    let present = trigrams.iter().all(|index| entry.bits[index >> 3] & (1u8 << (index & 7)) != 0);
    if present { TrigramVerdict::Possible } else { TrigramVerdict::Absent }
  }

  fn remember_trigrams(&mut self, path: &Path, signature: FileSignature, bits: Box<[u8]>) {
    self.trigrams.put(path.to_path_buf(), TrigramEntry { signature, bits });
  }

  fn cached_match(&mut self, query: &str) -> Option<Arc<SearchMatch>> {
    self.matches.get(query).cloned()
  }

  fn remember_match(&mut self, query: &str, entry: SearchMatch) -> Arc<SearchMatch> {
    let entry = Arc::new(entry);
    self.matches.put(query.to_string(), Arc::clone(&entry));
    entry
  }
}

static SEARCH_MEMOS: LazyLock<Mutex<SearchMemos>> = LazyLock::new(|| Mutex::new(SearchMemos::default()));

/// Search every session name and active-session chat content, newest first.
pub(crate) fn search_sessions(store: &Store, query: &str) -> StoreResult<SearchAnswer> {
  search_sessions_with_memos(store, query, &SEARCH_MEMOS)
}

fn search_sessions_with_memos(store: &Store, query: &str, memos: &Mutex<SearchMemos>) -> StoreResult<SearchAnswer> {
  if query.trim().is_empty() {
    return Ok(SearchAnswer { rows: store.list_sessions(Some(SessionStatus::Active), None, None)?.rows, source: None });
  }

  let needle = query.to_lowercase();
  let sessions_dir = store.config().sessions_dir();
  let sessions_handle = sessions_dir_handle(&sessions_dir);
  let (all, _) = store.load_session_metas_snapshot_versioned(None)?;
  // The query result's listing identity is checked first, then each stored file signature, before name matching or
  // content classification. Keep that hot path ahead of candidate rebuilds.
  if let Some(cached) = memos.lock().expect("session search memo mutex is not poisoned").cached_match(&needle)
    && Arc::ptr_eq(&cached.corpus, &all)
  {
    let signatures: Vec<Option<FileSignature>> =
      file_signatures(&cached.watched, sessions_handle.as_ref()).into_iter().map(Result::ok).collect();
    if cached.signatures == signatures {
      return Ok(SearchAnswer { rows: cached.rows.clone(), source: Some(cached) });
    }
  }

  let mut names = Vec::new();
  let mut content_metas = Vec::new();
  let mut candidate_paths = Vec::new();
  for meta in all.iter() {
    if meta.name.to_lowercase().contains(&needle) {
      names.push(meta);
    } else if meta.status == SessionStatus::Active {
      content_metas.push(meta);
      candidate_paths.push(watched_file(&sessions_dir, store.chat_events_path(&meta.id)));
    }
  }
  names.sort_by_key(|left| Reverse(left.updated_at));
  names.truncate(SEARCH_RESULT_LIMIT);
  let candidate_paths = Arc::new(candidate_paths);
  let signatures: Vec<Option<FileSignature>> = file_signatures(&candidate_paths, sessions_handle.as_ref())
    .into_iter()
    .zip(&content_metas)
    .map(|(result, meta)| match result {
      Ok(signature) => Some(signature),
      Err(error) => {
        log_search_read_failed_once(memos, &meta.id, &error);
        None
      }
    })
    .collect();

  // The per-file verdicts are read here, on the request's thread, and the files that need a scan are collected
  // so one batch can read them across threads. A candidate's place in `content_hits` stays its place in the
  // corpus, because the result's order breaks ties on insertion order.
  let trigrams = needle_trigrams(&needle);
  let mut content_hits = vec![false; content_metas.len()];
  let mut scans: Vec<(usize, u64)> = Vec::new();
  let mut unindexed: Vec<usize> = Vec::new();
  for (index, (_, signature)) in content_metas.iter().zip(&signatures).enumerate() {
    let path = &candidate_paths[index].path;
    let Some(signature) = signature else { continue };
    // The content index settles most fresh queries before the per-file roots are read, and reading those roots
    // copies every needle they hold.
    let needs_index = if trigrams.is_empty() {
      false
    } else {
      match memos
        .lock()
        .expect("session search memo mutex is not poisoned")
        .trigram_verdict(path, *signature, &trigrams)
      {
        TrigramVerdict::Absent => continue,
        TrigramVerdict::Possible => false,
        TrigramVerdict::Missing => true,
      }
    };
    let (miss_roots, hit_roots) = memos.lock().expect("session search memo mutex is not poisoned").roots(path);
    if hit_root_covers(&hit_roots, *signature, &needle) {
      content_hits[index] = true;
      continue;
    }
    let Some(start) = absence_rescan_start(&miss_roots, *signature, &needle) else {
      continue;
    };
    if needs_index {
      unindexed.push(index);
    }
    #[cfg(test)]
    {
      memos.lock().expect("session search memo mutex is not poisoned").scan_calls += 1;
    }
    scans.push((index, start));
  }
  // A file with no fresh content index gets one, and the same pass searches it, so the query reads it once. A file
  // whose index rules the needle out leaves the scan batch here, while a file the index cannot rule out is searched
  // by the pass itself and leaves it too.
  if !unindexed.is_empty() {
    let answers = index_and_search_files(&candidate_paths, &unindexed, &signatures, &needle);
    let mut indexed = HashSet::new();
    for (index, answer) in answers {
      let signature = signatures[index].expect("an indexed candidate holds its signature");
      match answer {
        Ok((bits, found)) => {
          let path = &candidate_paths[index].path;
          memos.lock().expect("session search memo mutex is not poisoned").remember_trigrams(path, signature, bits);
          memos
            .lock()
            .expect("session search memo mutex is not poisoned")
            .remember_root(path, &needle, signature, found);
          content_hits[index] = found;
          indexed.insert(index);
        }
        Err(error) => log_search_read_failed_once(memos, &content_metas[index].id, &error),
      }
    }
    if !indexed.is_empty() {
      scans.retain(|(index, _)| !indexed.contains(index));
    }
  }
  let mut scan_failures = 0;
  for ((index, _), answer) in scans.iter().zip(scan_contents(&candidate_paths, &scans, &needle)) {
    let path = &candidate_paths[*index].path;
    let signature = signatures[*index].expect("a scanned candidate holds its signature");
    match answer {
      Ok(true) => {
        memos.lock().expect("session search memo mutex is not poisoned").remember_root(path, &needle, signature, true);
        content_hits[*index] = true;
      }
      Ok(false) => {
        memos.lock().expect("session search memo mutex is not poisoned").remember_root(path, &needle, signature, false)
      }
      Err(error) => {
        scan_failures += 1;
        log_search_read_failed_once(memos, &content_metas[*index].id, &error);
      }
    }
  }

  names.extend(content_metas.iter().zip(&content_hits).filter(|(_, hit)| **hit).map(|(meta, _)| *meta));
  names.sort_by_key(|left| Reverse(left.updated_at));
  names.truncate(SEARCH_RESULT_LIMIT);
  if scan_failures == 0 {
    // The result holds at most the newest `SEARCH_RESULT_LIMIT` rows, so a candidate older than the last row cannot
    // enter a full result however its chat file changes. A repeat query re-checks only the candidates that could
    // enter it; a short result can still take any candidate, so it keeps watching all of them.
    let cutoff = names.get(SEARCH_RESULT_LIMIT - 1).map(|meta| meta.updated_at);
    let mut watched = Vec::new();
    let mut watched_signatures = Vec::new();
    for (index, (meta, signature)) in content_metas.iter().zip(signatures).enumerate() {
      if cutoff.is_none_or(|cutoff| meta.updated_at >= cutoff) {
        watched.push(candidate_paths[index].clone());
        watched_signatures.push(signature);
      }
    }
    let watched = Arc::new(watched);
    let rows: Vec<Arc<SessionMetadata>> = names.into_iter().cloned().collect();
    let source = memos.lock().expect("session search memo mutex is not poisoned").remember_match(
      &needle,
      SearchMatch {
        corpus: Arc::clone(&all),
        watched,
        signatures: watched_signatures,
        rows: rows.clone(),
        body: Mutex::new(None),
      },
    );
    return Ok(SearchAnswer { rows, source: Some(source) });
  }
  Ok(SearchAnswer { rows: names.into_iter().map(Arc::clone).collect(), source: None })
}

/// The signature of every path, in the argument order.
///
/// Each read is one `stat` and the answers do not depend on each other, so a batch reads on the signature pool's
/// threads. The pool lives for the process: a repeat query re-checks one `stat` per watched file, 200 on the mock
/// home, and starting that many threads costs more than the reads they share.
///
/// When `sessions_dir` is open, a read resolves the file below it instead of walking the home's whole path to the
/// session's `data` directory, which is most of the route's kernel time on a repeat query.
fn file_signatures(paths: &Arc<Vec<WatchedFile>>, sessions_dir: Option<&Arc<File>>) -> Vec<io::Result<FileSignature>> {
  let pool = &*SIGNATURE_POOL;
  if paths.len() < SIGNATURE_BATCH_MIN_PATHS || pool.senders.len() < 2 {
    return paths.iter().map(|file| file_signature(file, sessions_dir.map(Arc::as_ref))).collect();
  }
  let chunk = paths.len().div_ceil(pool.senders.len()).max(SIGNATURE_BATCH_CHUNK_FLOOR);
  let mut chunks: Vec<Option<Vec<io::Result<FileSignature>>>> = paths.chunks(chunk).map(|_| None).collect();
  let (reply, answers) = mpsc::channel();
  for (index, _) in chunks.iter().enumerate() {
    let start = index * chunk;
    let end = (start + chunk).min(paths.len());
    pool.dispatch(SignatureJob {
      paths: Arc::clone(paths),
      sessions_dir: sessions_dir.cloned(),
      start,
      end,
      reply: reply.clone(),
    });
  }
  drop(reply);
  for _ in 0..chunks.len() {
    let (chunk_start, signatures) = answers.recv().expect("a signature worker answers every dispatched chunk");
    chunks[chunk_start / chunk] = Some(signatures);
  }
  let mut out = Vec::with_capacity(paths.len());
  for chunk in chunks {
    out.extend(chunk.expect("every dispatched signature chunk answers"));
  }
  out
}

/// One watched file's identity, resolved from the open sessions directory when the file lies below it.
fn file_signature(file: &WatchedFile, sessions_dir: Option<&File>) -> io::Result<FileSignature> {
  match (sessions_dir, &file.below_sessions) {
    (Some(dir), Some(relative)) => file_signature_at(dir, relative),
    _ => file_signature_unlogged(&file.path),
  }
}

/// One chunk of one signature batch: the shared paths, the sessions directory handle, the chunk's half-open range,
/// and its reply channel.
struct SignatureJob {
  paths: Arc<Vec<WatchedFile>>,
  sessions_dir: Option<Arc<File>>,
  start: usize,
  end: usize,
  reply: Sender<(usize, Vec<io::Result<FileSignature>>)>,
}

/// The threads that read signature batches, one job queue per worker and a round-robin cursor over them.
///
/// A worker reads the chunks it is handed in order, so a batch that sends one chunk per worker pays no more than one
/// message round trip per chunk.
struct SignaturePool {
  senders: Vec<Sender<SignatureJob>>,
  next: AtomicUsize,
}

impl SignaturePool {
  fn start() -> Self {
    let workers = std::thread::available_parallelism().map_or(1, |count| count.get()).min(SIGNATURE_BATCH_WORKERS);
    let mut senders = Vec::with_capacity(workers);
    for index in 0..workers {
      let (sender, receiver) = mpsc::channel();
      std::thread::Builder::new()
        .name(format!("search-signature-{index}"))
        .spawn(move || signature_worker(receiver))
        .expect("a signature worker thread starts");
      senders.push(sender);
    }
    Self { senders, next: AtomicUsize::new(0) }
  }

  fn dispatch(&self, job: SignatureJob) {
    let index = self.next.fetch_add(1, Ordering::Relaxed) % self.senders.len();
    self.senders[index].send(job).expect("a signature worker lives as long as the pool");
  }
}

fn signature_worker(jobs: Receiver<SignatureJob>) {
  for job in jobs {
    let sessions_dir = job.sessions_dir.as_deref();
    let signatures = job.paths[job.start..job.end].iter().map(|file| file_signature(file, sessions_dir)).collect();
    // A batch that stopped waiting has no reader for this chunk.
    let _ = job.reply.send((job.start, signatures));
  }
}

static SIGNATURE_POOL: LazyLock<SignaturePool> = LazyLock::new(SignaturePool::start);

/// The open sessions directory, one handle for the process lifetime, keyed by its path.
///
/// A signature read resolves a session's file below this handle, so it walks only the session's own components.
/// The store holds the same kind of handle for its identity reads; this one keeps search's reads off the home's
/// path prefix. A directory that is not there answers None, and the batch reads by path instead.
/// The sessions directory's path and its open handle, so a store built on another home opens its own.
type SessionsDirMemo = Mutex<Option<(PathBuf, Arc<File>)>>;

static SESSIONS_DIR: LazyLock<SessionsDirMemo> = LazyLock::new(|| Mutex::new(None));

fn sessions_dir_handle(dir: &Path) -> Option<Arc<File>> {
  let mut memo = SESSIONS_DIR.lock().expect("the sessions directory memo mutex is not poisoned");
  if let Some((path, handle)) = memo.as_ref()
    && path == dir
  {
    return Some(Arc::clone(handle));
  }
  let handle = Arc::new(File::open(dir).ok()?);
  *memo = Some((dir.to_path_buf(), Arc::clone(&handle)));
  Some(handle)
}

/// The watched file at `path`, with its path below `sessions_dir` when it has one.
fn watched_file(sessions_dir: &Path, path: PathBuf) -> WatchedFile {
  let below_sessions =
    path.strip_prefix(sessions_dir).ok().and_then(|relative| CString::new(relative.as_os_str().as_bytes()).ok());
  WatchedFile { path, below_sessions }
}

fn file_signature_unlogged(path: &Path) -> io::Result<FileSignature> {
  let metadata = fs::metadata(path)?;
  Ok(FileSignature {
    mtime_ns: timefmt::file_time_ns(metadata.mtime(), metadata.mtime_nsec()),
    size: metadata.size(),
    inode: metadata.ino(),
  })
}

/// The identity of the file at `relative`, resolved below the open directory `dir`.
///
/// It is the same `fstatat` the path form makes; anchoring it on a directory handle means each read walks only
/// the session's own path components instead of the home's whole path.
fn file_signature_at(dir: &File, relative: &CString) -> io::Result<FileSignature> {
  let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
  let status = unsafe { libc::fstatat(dir.as_raw_fd(), relative.as_ptr(), stat.as_mut_ptr(), 0) };
  if status != 0 {
    return Err(io::Error::last_os_error());
  }
  let stat = unsafe { stat.assume_init() };
  Ok(FileSignature {
    mtime_ns: timefmt::file_time_ns(stat.st_mtime, stat.st_mtime_nsec),
    size: stat.st_size as u64,
    inode: stat.st_ino,
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

/// The trigrams a lowercase ASCII needle holds; a window with any other byte contributes none.
///
/// Every window of the needle's own bytes is a window of any text that holds the needle, so an index that lacks one
/// of these trigrams proves the needle absent. A window the index cannot encode proves nothing and is dropped.
fn needle_trigrams(needle: &str) -> Vec<usize> {
  if !needle.is_ascii() {
    return Vec::new();
  }
  let codes = &*TRIGRAM_CODES;
  let bytes = needle.as_bytes();
  bytes
    .windows(3)
    .filter_map(|window| {
      trigram_number(codes[window[0] as usize], codes[window[1] as usize], codes[window[2] as usize])
    })
    .collect()
}

/// The bit position of three letter codes, or None when one code is not a lowercase letter.
///
/// Each code is checked: a [`TRIGRAM_NONE`] in the second or third position would still sum below
/// [`TRIGRAM_BITS`] and alias the bit of a letter sequence the text does not hold.
fn trigram_number(first: u8, second: u8, third: u8) -> Option<usize> {
  if first >= TRIGRAM_NONE || second >= TRIGRAM_NONE || third >= TRIGRAM_NONE {
    return None;
  }
  Some(first as usize * 676 + second as usize * 26 + third as usize)
}

/// One file's content index and whether the needle occurs in it, from one pass over its bytes.
///
/// The needle search matches the scan's rule: both read the text with ASCII case folding, so a needle the index
/// cleared the file for can still match here. `window` carries the last bytes of each chunk, which keeps a trigram
/// and a needle that span the boundary.
fn index_and_search_file(path: &Path, needle: &str) -> ContentIndexAnswer {
  let mut file = File::open(path)?;
  let mut bits = vec![0u8; TRIGRAM_BYTES];
  let finder = memchr::memmem::Finder::new(needle.as_bytes());
  let codes = &*TRIGRAM_CODES;
  let overlap = needle.len().saturating_sub(1).max(2);
  let mut carry: Vec<u8> = Vec::with_capacity(overlap);
  let mut chunk = vec![0u8; SEARCH_CHUNK_SIZE];
  let mut window: Vec<u8> = Vec::with_capacity(overlap + SEARCH_CHUNK_SIZE);
  let mut matched = false;
  loop {
    let count = file.read(&mut chunk)?;
    if count == 0 {
      return Ok((bits.into_boxed_slice(), matched));
    }
    window.clear();
    window.extend_from_slice(&carry);
    window.extend_from_slice(&chunk[..count]);
    window.make_ascii_lowercase();
    for position in 0..window.len().saturating_sub(2) {
      let index = trigram_number(
        codes[window[position] as usize],
        codes[window[position + 1] as usize],
        codes[window[position + 2] as usize],
      );
      if let Some(index) = index {
        bits[index >> 3] |= 1u8 << (index & 7);
      }
    }
    if !matched && finder.find(&window).is_some() {
      matched = true;
    }
    let keep = window.len().min(overlap);
    carry.clear();
    carry.extend_from_slice(&window[window.len() - keep..]);
  }
}

/// A content index and a needle search for each named candidate, each answer carrying its candidate's index.
///
/// The batch splits by file size and not by file count, because the candidates range over four orders of magnitude
/// and the route waits on its slowest worker. The split is why the answers come in no fixed order; the caller pairs
/// each answer with the index it carries.
fn index_and_search_files(
  paths: &[WatchedFile],
  jobs: &[usize],
  signatures: &[Option<FileSignature>],
  needle: &str,
) -> Vec<(usize, ContentIndexAnswer)> {
  let workers = std::thread::available_parallelism().map_or(1, |count| count.get()).min(SCAN_BATCH_WORKERS);
  if jobs.len() < SCAN_BATCH_MIN_JOBS || workers < 2 {
    return jobs.iter().map(|index| (*index, index_and_search_file(&paths[*index].path, needle))).collect();
  }
  let mut order = jobs.to_vec();
  order.sort_by_key(|index| Reverse(signatures[*index].map_or(0, |signature| signature.size)));
  let mut bins: Vec<Vec<usize>> = (0..workers).map(|_| Vec::new()).collect();
  let mut loads = vec![0u64; workers];
  for index in order {
    let lightest = (0..workers).min_by_key(|worker| loads[*worker]).expect("a worker exists");
    loads[lightest] += signatures[index].map_or(0, |signature| signature.size);
    bins[lightest].push(index);
  }
  let mut answers = Vec::with_capacity(jobs.len());
  std::thread::scope(|scope| {
    let handles: Vec<_> = bins
      .into_iter()
      .filter(|bin| !bin.is_empty())
      .map(|bin| {
        scope.spawn(move || {
          bin.into_iter().map(|index| (index, index_and_search_file(&paths[index].path, needle))).collect::<Vec<_>>()
        })
      })
      .collect();
    for handle in handles {
      answers.extend(handle.join().expect("an index worker finishes without panic"));
    }
  });
  answers
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
  // memchr's memmem is the two-way search behind the substring test,
  // built once per scan over the folded windows.
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

/// The scan answer of every job, in the jobs' order.
///
/// A scan reads its file from the start offset to the end. The jobs touch different files, so a batch long
/// enough to pay for them runs the jobs on their own threads.
fn scan_contents(paths: &[WatchedFile], scans: &[(usize, u64)], needle: &str) -> Vec<io::Result<bool>> {
  let workers = std::thread::available_parallelism().map_or(1, |count| count.get()).min(SCAN_BATCH_WORKERS);
  if scans.len() < SCAN_BATCH_MIN_JOBS || workers < 2 {
    return scans.iter().map(|(index, start)| scan_content(&paths[*index].path, needle, *start)).collect();
  }
  let chunk = scans.len().div_ceil(workers);
  let mut answers = Vec::with_capacity(scans.len());
  std::thread::scope(|scope| {
    let handles: Vec<_> = scans
      .chunks(chunk)
      .map(|chunk| {
        scope.spawn(move || {
          chunk.iter().map(|(index, start)| scan_content(&paths[*index].path, needle, *start)).collect::<Vec<_>>()
        })
      })
      .collect();
    for handle in handles {
      answers.extend(handle.join().expect("a scan worker finishes without panic"));
    }
  });
  answers
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
  ///
  /// The batch reads below the sessions directory, so its answers must match a direct path read: the
  /// two syscalls differ only in how they resolve the path.
  #[test]
  fn signature_batch_answers_in_argument_order() {
    let home = tempfile::tempdir().unwrap();
    let sessions_dir = home.path().join("sessions");
    let data = sessions_dir.join("session-a").join("data");
    fs::create_dir_all(&data).unwrap();
    let mut paths = Vec::new();
    for index in 0..super::SIGNATURE_BATCH_MIN_PATHS + 37 {
      let path = data.join(format!("chat-{index:05}.jsonl"));
      // Every 97th path stays absent, so a failed read sits between successful answers.
      if index % 97 != 0 {
        fs::write(&path, format!("event {index}\n")).unwrap();
      }
      paths.push(super::watched_file(&sessions_dir, path));
    }
    let paths = Arc::new(paths);
    let handle = Arc::new(fs::File::open(&sessions_dir).unwrap());
    let batch = super::file_signatures(&paths, Some(&handle));
    assert_eq!(batch.len(), paths.len());
    for (index, (answer, file)) in batch.iter().zip(paths.iter()).enumerate() {
      if index % 97 == 0 {
        assert!(answer.is_err(), "path {index} is absent and its answer reports the failed read");
        continue;
      }
      let signature = answer.as_ref().expect("a written path reads");
      assert_eq!(signature.size, (format!("event {index}\n").len() as u64), "path {index} answers for itself");
      assert_eq!(*signature, super::file_signature_unlogged(&file.path).unwrap(), "path {index} matches a direct read");
    }
  }

  /// A scan batch pairs every answer with its own file: a needle found in one candidate must not mark another.
  #[test]
  fn scan_batch_answers_in_argument_order() {
    let home = tempfile::tempdir().unwrap();
    let mut paths = Vec::new();
    for index in 0..super::SCAN_BATCH_MIN_JOBS + 13 {
      let path = home.path().join(format!("chat-{index:03}.jsonl"));
      let body =
        if index % 7 == 0 { format!("event {index} carries the needle\n") } else { format!("event {index}\n") };
      fs::write(&path, body).unwrap();
      paths.push(super::watched_file(home.path(), path));
    }
    let scans: Vec<(usize, u64)> = (0..paths.len()).map(|index| (index, 0)).collect();
    let answers = super::scan_contents(&paths, &scans, "needle");
    assert_eq!(answers.len(), paths.len());
    for (index, answer) in answers.iter().enumerate() {
      assert_eq!(answer.as_ref().unwrap(), &(index % 7 == 0), "candidate {index} answers for itself");
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

    let rows = search_sessions(&store, "  \t").unwrap().rows;
    assert_eq!(rows.iter().map(|meta| meta.id.as_str()).collect::<Vec<_>>(), ["new", "old"]);
  }

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

    let rows = search_sessions(&store, "needle").unwrap().rows;
    let ids: Vec<&str> = rows.iter().map(|meta| meta.id.as_str()).collect();
    assert_eq!(rows.len(), 200);
    assert!(ids.contains(&"above"));
    assert!(!ids.contains(&"below"));
    assert!(!ids.contains(&"archived-000"));
  }

  #[test]
  fn search_reads_new_chat_content_and_names_on_the_next_request() {
    let home = tempfile::tempdir().unwrap();
    let store = test_store(home.path());
    write_session(&store, "quiet", "quiet", SessionStatus::Active, "2026-08-01T12:00:00Z");
    write_session(&store, "carrier", "carrier", SessionStatus::Active, "2026-08-01T12:01:00Z");
    assert!(search_sessions(&store, "needle").unwrap().rows.is_empty());

    let events = store.chat_events_path("carrier");
    fs::create_dir_all(events.parent().unwrap()).unwrap();
    fs::write(&events, "{\"content\":\"needle in the events\"}\n").unwrap();
    let content_hits = search_sessions(&store, "needle").unwrap().rows;
    assert_eq!(content_hits.iter().map(|meta| meta.id.as_str()).collect::<Vec<_>>(), ["carrier"]);

    write_session(&store, "named", "needle name", SessionStatus::Archived, "2026-08-01T12:02:00Z");
    let all_hits = search_sessions(&store, "needle").unwrap().rows;
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

    assert!(super::search_sessions_with_memos(&store, "needle", &memos).unwrap().rows.is_empty());
    assert!(super::search_sessions_with_memos(&store, "needle", &memos).unwrap().rows.is_empty());
    assert_eq!(memos.lock().unwrap().scan_calls, 1, "the repeated query does no second file scan");

    fs::write(&path, "no match yet; needle appears").unwrap();
    let hit = super::search_sessions_with_memos(&store, "needle", &memos).unwrap();
    assert_eq!(hit.rows.iter().map(|meta| meta.id.as_str()).collect::<Vec<_>>(), ["content"]);
    let repeat = super::search_sessions_with_memos(&store, "needle", &memos).unwrap();
    assert!(Arc::ptr_eq(&hit.rows[0], &repeat.rows[0]), "the repeat query answers with the rows the memo holds");
    let source = repeat.source.expect("a repeat query names the memo it reused");
    assert!(Arc::ptr_eq(hit.source.as_ref().unwrap(), &source), "the repeat query reuses the same memo entry");
    source.remember_body(7, Arc::new(b"needle body".to_vec()));
    assert_eq!(source.body(7).map(|body| body.to_vec()), Some(b"needle body".to_vec()));
    assert!(source.body(8).is_none(), "a moved derived-state generation drops the body");
    assert_eq!(super::search_sessions_with_memos(&store, "eed", &memos).unwrap().rows.len(), 1);
    assert_eq!(memos.lock().unwrap().scan_calls, 2, "the changed file rescans once and its hit covers substrings");
  }

  /// A fresh query answers from the content index of a file whose identity it already holds: a trigram the index
  /// lacks proves the needle absent, while a needle the index cannot rule out still reaches the file.
  #[test]
  fn a_fresh_query_skips_a_file_whose_content_index_lacks_one_of_its_trigrams() {
    let home = tempfile::tempdir().unwrap();
    let store = test_store(home.path());
    write_session(&store, "content", "content session", SessionStatus::Active, "2026-08-01T12:00:00Z");
    let path = store.chat_events_path("content");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "the quick brown fox\n").unwrap();
    let memos = Mutex::new(super::SearchMemos::default());

    assert!(super::search_sessions_with_memos(&store, "walrus", &memos).unwrap().rows.is_empty());
    assert_eq!(memos.lock().unwrap().scan_calls, 1, "the first query reads the file and keeps its index");

    // "wal" is absent from the file, so this fresh query answers without reading it.
    assert!(super::search_sessions_with_memos(&store, "walq1rus", &memos).unwrap().rows.is_empty());
    assert_eq!(memos.lock().unwrap().scan_calls, 1, "the content index ruled the needle out");

    // Every trigram of this needle occurs in the file, and the query's case does not matter.
    let rows = super::search_sessions_with_memos(&store, "BROWN", &memos).unwrap().rows;
    assert_eq!(rows.iter().map(|meta| meta.id.as_str()).collect::<Vec<_>>(), ["content"]);
    assert_eq!(memos.lock().unwrap().scan_calls, 2, "the index lets through a needle the file holds");
  }

  /// A file that gains matching text must rebuild its index: an index read at the old signature would hide the match.
  #[test]
  fn a_changed_file_rebuilds_its_content_index() {
    let home = tempfile::tempdir().unwrap();
    let store = test_store(home.path());
    write_session(&store, "content", "content session", SessionStatus::Active, "2026-08-01T12:00:00Z");
    let path = store.chat_events_path("content");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "no match here\n").unwrap();
    let memos = Mutex::new(super::SearchMemos::default());

    assert!(super::search_sessions_with_memos(&store, "walrus", &memos).unwrap().rows.is_empty());
    fs::write(&path, "no match here; walrus appears\n").unwrap();
    let rows = super::search_sessions_with_memos(&store, "walrus", &memos).unwrap().rows;
    assert_eq!(rows.iter().map(|meta| meta.id.as_str()).collect::<Vec<_>>(), ["content"]);
  }

  /// A non-letter in any position must name no trigram: the combined sum alone would alias another sequence's bit.
  #[test]
  fn a_window_with_a_non_letter_names_no_trigram() {
    let codes = &*super::TRIGRAM_CODES;
    let code = |byte: u8| codes[byte as usize];
    assert_eq!(super::trigram_number(code(b'a'), code(b'{'), code(b'a')), None);
    assert_eq!(super::trigram_number(code(b'a'), code(b'b'), code(b'{')), None);
    assert_eq!(super::trigram_number(code(b'{'), code(b'b'), code(b'c')), None);
    assert_eq!(super::trigram_number(code(b'a'), code(b'b'), code(b'c')), Some(28));
  }

  #[test]
  fn the_content_index_holds_trigrams_across_chunk_boundaries() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("chat.jsonl");
    let mut body = vec![b'x'; super::SEARCH_CHUNK_SIZE - 1];
    body.extend_from_slice(b"BoundaryNeedle");
    fs::write(&path, &body).unwrap();

    let (bits, found) = super::index_and_search_file(&path, "boundaryneedle").unwrap();
    assert!(found, "the needle search reads the text with ASCII case folding");
    for trigram in super::needle_trigrams("boundaryneedle") {
      assert_ne!(bits[trigram >> 3] & (1u8 << (trigram & 7)), 0, "trigram {trigram} is indexed");
    }

    let (bits, found) = super::index_and_search_file(&path, "walq1rus").unwrap();
    assert!(!found);
    assert!(
      super::needle_trigrams("walq1rus").iter().any(|trigram| bits[trigram >> 3] & (1u8 << (trigram & 7)) == 0),
      "the index lacks a trigram of the absent needle"
    );
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

    let rows = search_sessions(&store, "walrus").unwrap().rows;
    assert_eq!(rows.len(), super::SEARCH_RESULT_LIMIT);
    assert!(rows.iter().all(|meta| meta.id.starts_with("hit-")));

    write_content("old", "{\"content\":\"walrus in the events\"}\n");
    let rows = search_sessions(&store, "walrus").unwrap().rows;
    assert_eq!(rows.len(), super::SEARCH_RESULT_LIMIT);
    assert!(!rows.iter().any(|meta| meta.id == "old"), "a match older than the last row stays out of the full result");

    write_content("newer", "{\"content\":\"walrus in the events\"}\n");
    let rows = search_sessions(&store, "walrus").unwrap().rows;
    assert_eq!(rows.len(), super::SEARCH_RESULT_LIMIT);
    assert_eq!(rows[0].id, "newer", "a new match in a watched candidate enters the result");
    assert!(!rows.iter().any(|meta| meta.id == "old"));
  }
}
