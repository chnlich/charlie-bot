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
use std::time::{Duration, Instant};

use charliebot_core::timefmt;
use charliebot_store::metadata::{SessionMetadata, SessionStatus, UtcDateTime};
use charliebot_store::{Store, StoreResult};
use lru::LruCache;

const SEARCH_RESULT_LIMIT: usize = 200;
const SEARCH_FILE_MEMO_LIMIT: usize = 4096;
const SEARCH_ROOTS_PER_FILE: usize = 8;
const SEARCH_MATCH_MEMO_LIMIT: usize = 8;
/// How long a repeat query answers from the match it just verified, and how long a fresh query trusts a content index
/// without reading its file's signature.
///
/// The repeat check is one `stat` per watched chat file: every active session's file while the answer is short of the
/// cap, which is 7,160 reads and about 12 ms of CPU on the mock home for an answer of a few bytes. The fresh query's
/// index check is the same read per candidate file, and it is the route's dominant cost. A chat file that an
/// out-of-band writer changes is seen within this interval either way, the same bound the store's listing memo puts on
/// its own out-of-band edits.
#[cfg(not(test))]
const SEARCH_SWEEP_INTERVAL: Duration = Duration::from_secs(1);
/// A test changes a file behind the memo's back and expects the next query to see it, so tests read a signature on
/// every query unless they name their own interval.
#[cfg(test)]
const SEARCH_SWEEP_INTERVAL: Duration = Duration::ZERO;
const SEARCH_CHUNK_SIZE: usize = 1 << 18;
/// The number of lowercase ASCII letters a trigram position may take.
const TRIGRAM_LETTERS: u8 = 26;
/// Every three-letter sequence's bit in one file's content index.
const TRIGRAM_BITS: usize = 26 * 26 * 26;
/// The bytes one file's trigram bitmap takes.
const TRIGRAM_BYTES: usize = TRIGRAM_BITS.div_ceil(8);
/// The bytes one file's bigram bitmap takes: one bit per pair of [`BIGRAM_SYMBOLS`].
const BIGRAM_BYTES: usize = BIGRAM_SYMBOLS.len() * BIGRAM_SYMBOLS.len() / 8;
/// The bytes one file's high-byte bigram bitmap takes: one bit per pair of bytes at or above 0x80.
///
/// A multibyte character's UTF-8 bytes all lie at or above 0x80, so every pair inside such a needle is a high-byte
/// pair this bitmap can test. Unknown or rare multibyte text carries few of the pairs a common needle holds, so the
/// test rules out the files a trigram cannot encode.
const HIGH_BIGRAM_BYTES: usize = 128 * 128 / 8;
/// The code of a byte that is neither a lowercase ASCII letter nor a [`BIGRAM_SYMBOLS`] entry.
const CONTENT_NONE: u8 = u8::MAX;
/// The symbols a content index's bigrams distinguish: the lowercase ASCII letters, the digits, and the punctuation
/// text carries.
///
/// The trigram bitmap keeps the 26 letters alone, because a bitmap over these 64 symbols would take 32 KB per file.
/// A bigram needs only 64 × 64 bits, so it covers the digits and symbols that a needle such as an identifier, a
/// version number or a path holds, which no letter trigram can prove absent.
const BIGRAM_SYMBOLS: &[u8; 64] = b"abcdefghijklmnopqrstuvwxyz0123456789 ._-/:,;()[]{}<>=\"'`|&#*@+%!";
/// The files whose content indexes the memo keeps.
///
/// One index's bitmaps are 4,757 bytes and the 7,160-session mock home takes about 34 MB. The cap exceeds that
/// count, so a fresh query does not evict the indexes its next query would reuse.
const SEARCH_CONTENT_INDEX_MEMO_LIMIT: usize = 16_384;
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
/// A sparse needle's first query scans nearly every active session's chat file: 5.5 GB on the 7,160-session mock home,
/// which one thread spends about 0.6 s on. The scans read different files and do not depend on each other.
const SCAN_BATCH_WORKERS: usize = 16;
/// The fewest scans a batch runs on its own threads: starting them costs more than a short batch saves.
const SCAN_BATCH_MIN_JOBS: usize = 32;
/// The candidates the first sweep window reads before the route checks whether the answer's slots are full.
///
/// The sweep runs newest-first, so the first window holds the newest candidates a full answer can carry. A window
/// larger than the slots need would read files the answer cannot hold; a smaller one needs more batches to fill it.
/// Tests shrink it, so a stopped sweep needs a corpus small enough to write inside the unit-test time bound.
#[cfg(not(test))]
const SWEEP_WINDOW_CANDIDATES: usize = 256;
#[cfg(test)]
const SWEEP_WINDOW_CANDIDATES: usize = 8;

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
  /// The path's [`path_key`], which the content index memo is keyed by.
  key: u64,
  below_sessions: Option<CString>,
}

/// The key a chat file's content index is stored under.
///
/// The sweep looks every candidate's index up, so the key is the path's FNV-1a hash: an eight-byte key costs one
/// hashbrown probe, where the path itself cost a hash of its bytes and a compare on every candidate. A hash is not
/// unique, so an entry keeps the path it was built for, and a mismatched path answers as a missing index.
fn path_key(path: &Path) -> u64 {
  let mut key: u64 = 0xcbf2_9ce4_8422_2325;
  for byte in path.as_os_str().as_bytes() {
    key ^= u64::from(*byte);
    key = key.wrapping_mul(0x0000_0100_0000_01b3);
  }
  key
}

/// The content index's code per byte: the byte's position in [`BIGRAM_SYMBOLS`], its letter's position for an
/// uppercase letter, and [`CONTENT_NONE`] for every byte the symbols do not hold.
static CONTENT_CODES: LazyLock<[u8; 256]> = LazyLock::new(|| {
  let mut codes = [CONTENT_NONE; 256];
  for (code, symbol) in BIGRAM_SYMBOLS.iter().enumerate() {
    let code = code as u8;
    codes[*symbol as usize] = code;
    if symbol.is_ascii_lowercase() {
      codes[symbol.to_ascii_uppercase() as usize] = code;
    }
  }
  codes
});

/// One file's content index: a bit per three-letter sequence, a bit per two-symbol sequence, and a bit per pair of
/// high bytes its text holds.
///
/// `signature` is the file's identity when the index was built. A file whose identity moved needs its index rebuilt,
/// because text that grew or changed can hold sequences the old index lacks.
struct ContentIndexEntry {
  /// The file the index was built for, so a [`path_key`] collision answers as a missing index.
  path: PathBuf,
  signature: FileSignature,
  /// When the sweep last read the file's signature and found it equal to [`Self::signature`], so the index still
  /// describes the file.
  verified_at: Instant,
  trigrams: Box<[u8]>,
  bigrams: Box<[u8]>,
  high_bigrams: Box<[u8]>,
}

/// One file's content index and whether the needle occurs in it, or the read that failed.
type ContentIndexAnswer = io::Result<(Box<[u8]>, Box<[u8]>, Box<[u8]>, bool)>;

/// What a file's content index says about a query.
enum ContentVerdict {
  /// No index holds this file at its current signature.
  Missing,
  /// The index cannot rule the file out, so the file is scanned.
  Possible,
  /// A trigram, a bigram or a high-byte bigram of the needle is absent from the file, so the needle is absent from
  /// the file too.
  Absent,
}

/// Whether an index holds every sequence of the needle: the needle can occur in the file only then.
fn index_holds(entry: &ContentIndexEntry, trigrams: &[usize], bigrams: &[usize], high_bigrams: &[usize]) -> bool {
  let holds = |bits: &[u8], bit: usize| bits[bit >> 3] & (1u8 << (bit & 7)) != 0;
  trigrams.iter().all(|bit| holds(&entry.trigrams, *bit))
    && bigrams.iter().all(|bit| holds(&entry.bigrams, *bit))
    && high_bigrams.iter().all(|bit| holds(&entry.high_bigrams, *bit))
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
  /// The corpus's chat files, shared with the corpus work: a short result watches every candidate, so a copy of the
  /// list would clone and later free 7,160 paths for each query the match memo keeps.
  candidates: Arc<Vec<WatchedFile>>,
  /// The candidates a repeat query re-checks: every content candidate while the result is short of the cap, and only
  /// the candidates at or above the last returned row's `updated_at` once the result reaches it.
  watched: Vec<usize>,
  /// The signature each watched candidate was read at, in `watched` order.
  signatures: Vec<Option<FileSignature>>,
  /// The matched rows, newest first, capped at [`SEARCH_RESULT_LIMIT`].
  rows: Vec<Arc<SessionMetadata>>,
  /// The response body rendered from `rows`, with the derived-state generation it holds for.
  body: Mutex<Option<SearchBody>>,
  /// When `signatures` last held. A repeat query inside [`SEARCH_SWEEP_INTERVAL`] answers from this match without
  /// reading the watched files again.
  verified_at: Mutex<Instant>,
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

  /// Whether this match's file check is recent enough for a repeat query to answer without reading the files again.
  fn verification_holds(&self, interval: Duration) -> bool {
    self.verified_at.lock().expect("search verification mutex is not poisoned").elapsed() < interval
  }

  /// Record that the watched files' signatures held just now.
  fn mark_verified(&self) {
    *self.verified_at.lock().expect("search verification mutex is not poisoned") = Instant::now();
  }
}

/// One search answer: its rows and the memoized result they belong to.
pub(crate) struct SearchAnswer {
  pub rows: Vec<Arc<SessionMetadata>>,
  /// The memoized result whose signature check held, or the one this pass just stored. `None` when a read failure kept
  /// this pass out of the memo, and for a blank query, which lists sessions instead of searching them.
  pub source: Option<Arc<SearchMatch>>,
}

/// The work every fresh query of one corpus shares: each session's lowercase name, and each active session's chat
/// file.
///
/// A fresh query tests the needle against every session name and checks the chat files of the active sessions that the
/// capped answer can still take, and both sets depend only on the corpus. The store keeps a corpus until a metadata
/// write or the sweep interval rebuilds it, so the entry serves the queries between two rebuilds. `corpus` is the key
/// and holds the rows it names alive, so a pointer match names the same sessions; `sessions_dir` is the directory the
/// paths resolve below.
struct CorpusWork {
  corpus: Arc<Vec<Arc<SessionMetadata>>>,
  sessions_dir: PathBuf,
  /// Each corpus row's name, lowercased once.
  lowered_names: Vec<String>,
  /// Every active session's chat file, in corpus order. The signature batch shares it across its workers.
  candidates: Arc<Vec<WatchedFile>>,
  /// The corpus row of each candidate, so a verdict maps back to its session.
  candidate_rows: Vec<usize>,
  /// The candidates, newest first, so a window of the sweep holds the newest candidates a full answer can carry.
  ///
  /// The order breaks ties on corpus order, as the answer does, so a scan's insertion order does not move.
  newest_first: Vec<usize>,
}

struct SearchMemos {
  corpus_work: Option<Arc<CorpusWork>>,
  files: LruCache<PathBuf, SearchFileRoots>,
  content: LruCache<u64, ContentIndexEntry>,
  matches: LruCache<String, Arc<SearchMatch>>,
  read_failures: HashSet<(String, String)>,
  /// How long a verified match answers a repeat query: [`SEARCH_SWEEP_INTERVAL`], or the interval a test names.
  sweep_interval: Duration,
  #[cfg(test)]
  scan_calls: usize,
  /// The repeat queries that re-read the watched files' signatures, so a test can tell a memo answer from a sweep.
  #[cfg(test)]
  repeat_sweeps: usize,
  /// The candidates the scan windows read, so a test can tell a stopped scan from a complete one.
  #[cfg(test)]
  scanned_candidates: usize,
  /// The candidates whose signatures the sweep read, so a test can tell a stopped sweep from a complete one.
  #[cfg(test)]
  swept_candidates: usize,
}

impl Default for SearchMemos {
  fn default() -> Self {
    let files = LruCache::new(NonZeroUsize::new(SEARCH_FILE_MEMO_LIMIT).expect("the file memo limit is nonzero"));
    let content = LruCache::new(
      NonZeroUsize::new(SEARCH_CONTENT_INDEX_MEMO_LIMIT).expect("the content index memo limit is nonzero"),
    );
    let matches = LruCache::new(NonZeroUsize::new(SEARCH_MATCH_MEMO_LIMIT).expect("the match memo limit is nonzero"));
    Self {
      corpus_work: None,
      files,
      content,
      matches,
      read_failures: HashSet::new(),
      sweep_interval: SEARCH_SWEEP_INTERVAL,
      #[cfg(test)]
      scan_calls: 0,
      #[cfg(test)]
      repeat_sweeps: 0,
      #[cfg(test)]
      scanned_candidates: 0,
      #[cfg(test)]
      swept_candidates: 0,
    }
  }
}

impl SearchMemos {
  /// The corpus work for `all`: the cached entry while the corpus and its sessions directory still hold, and a fresh
  /// one otherwise.
  fn corpus_work(
    &mut self,
    all: &Arc<Vec<Arc<SessionMetadata>>>,
    sessions_dir: &Path,
    store: &Store,
  ) -> Arc<CorpusWork> {
    if let Some(work) = self.corpus_work.as_ref()
      && Arc::ptr_eq(&work.corpus, all)
      && work.sessions_dir == sessions_dir
    {
      return Arc::clone(work);
    }
    let mut lowered_names = Vec::with_capacity(all.len());
    let mut candidates = Vec::new();
    let mut candidate_rows = Vec::new();
    for (row, meta) in all.iter().enumerate() {
      lowered_names.push(meta.name.to_lowercase());
      if meta.status == SessionStatus::Active {
        candidates.push(watched_file(sessions_dir, store.chat_events_path(&meta.id)));
        candidate_rows.push(row);
      }
    }
    let mut newest_first: Vec<usize> = (0..candidates.len()).collect();
    newest_first.sort_by_key(|index| Reverse(all[candidate_rows[*index]].updated_at));
    let work = Arc::new(CorpusWork {
      corpus: Arc::clone(all),
      sessions_dir: sessions_dir.to_path_buf(),
      lowered_names,
      candidates: Arc::new(candidates),
      candidate_rows,
      newest_first,
    });
    self.corpus_work = Some(Arc::clone(&work));
    work
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

  /// What this file's content index says about a needle, given the needle's trigrams, bigrams and high-byte bigrams.
  fn content_verdict(
    &mut self,
    file: &WatchedFile,
    signature: FileSignature,
    trigrams: &[usize],
    bigrams: &[usize],
    high_bigrams: &[usize],
  ) -> ContentVerdict {
    let Some(entry) = self.content.get_mut(&file.key) else { return ContentVerdict::Missing };
    if entry.path != file.path || entry.signature != signature {
      return ContentVerdict::Missing;
    }
    entry.verified_at = Instant::now();
    if index_holds(entry, trigrams, bigrams, high_bigrams) { ContentVerdict::Possible } else { ContentVerdict::Absent }
  }

  /// The signature of a file whose index, read within `interval` of `now`, already rules the needle out.
  ///
  /// `None` means the caller reads the file's signature before it asks [`Self::content_verdict`]. A fresh query reads
  /// one signature per candidate, which is the route's dominant cost: 7,160 path walks on the mock home, 85% of its
  /// CPU. The interval that already bounds how long a repeat query takes to see an out-of-band writer now bounds the
  /// index's age too, so a query inside it answers from the indexes and a query after it re-reads the signatures.
  ///
  /// `now` is read once per sweep, because the clock read costs more than the index test it guards.
  fn recent_absence(
    &mut self,
    file: &WatchedFile,
    now: Instant,
    interval: Duration,
    trigrams: &[usize],
    bigrams: &[usize],
    high_bigrams: &[usize],
  ) -> Option<FileSignature> {
    let entry = self.content.get_mut(&file.key)?;
    if entry.path != file.path || now.duration_since(entry.verified_at) >= interval {
      return None;
    }
    if index_holds(entry, trigrams, bigrams, high_bigrams) { None } else { Some(entry.signature) }
  }

  fn remember_content_index(
    &mut self,
    file: &WatchedFile,
    signature: FileSignature,
    trigrams: Box<[u8]>,
    bigrams: Box<[u8]>,
    high_bigrams: Box<[u8]>,
  ) {
    self.content.put(
      file.key,
      ContentIndexEntry {
        path: file.path.clone(),
        signature,
        verified_at: Instant::now(),
        trigrams,
        bigrams,
        high_bigrams,
      },
    );
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
  search_sessions_with_limit(store, query, memos, SEARCH_RESULT_LIMIT)
}

/// A repeat answer: the rows its match holds and the body rendered for the request's generation.
pub(crate) type RepeatAnswer = (Vec<Arc<SessionMetadata>>, Arc<Vec<u8>>);

/// The rows and body a repeat of `query` over `corpus` already holds, when every check its memo makes holds without
/// reading a file, and `None` when the answer needs a signature read or a scan.
///
/// The body this returns is the body a rebuild would render: the match it comes from passed the same corpus-identity
/// and verification checks [`search_sessions`] makes before it answers a repeat from the memo. The rows come with it
/// because a route that answers from a memo still counts its sidebar read, which is what starts the periodic sweep for
/// an edit made outside the server. The route calls this on the request's own thread, because the blocking pool exists
/// for the file reads a fresh query and a lapsed check need, and this path makes none.
pub(crate) fn repeat_answer(
  query: &str,
  corpus: &Arc<Vec<Arc<SessionMetadata>>>,
  generation: u64,
) -> Option<RepeatAnswer> {
  repeat_answer_with_memos(query, corpus, generation, &SEARCH_MEMOS)
}

fn repeat_answer_with_memos(
  query: &str,
  corpus: &Arc<Vec<Arc<SessionMetadata>>>,
  generation: u64,
  memos: &Mutex<SearchMemos>,
) -> Option<RepeatAnswer> {
  if query.trim().is_empty() {
    return None;
  }
  let needle = query.to_lowercase();
  let (cached, sweep_interval) = {
    let mut guard = memos.lock().expect("session search memo mutex is not poisoned");
    let sweep_interval = guard.sweep_interval;
    (guard.cached_match(&needle)?, sweep_interval)
  };
  if !Arc::ptr_eq(&cached.corpus, corpus) || !cached.verification_holds(sweep_interval) {
    return None;
  }
  Some((cached.rows.clone(), cached.body(generation)?))
}

fn search_sessions_with_limit(
  store: &Store,
  query: &str,
  memos: &Mutex<SearchMemos>,
  result_limit: usize,
) -> StoreResult<SearchAnswer> {
  if query.trim().is_empty() {
    return Ok(SearchAnswer { rows: store.list_sessions(Some(SessionStatus::Active), None, None)?.rows, source: None });
  }

  let needle = query.to_lowercase();
  let sessions_dir = store.config().sessions_dir();
  let sessions_handle = sessions_dir_handle(&sessions_dir);
  let (all, _) = store.load_session_metas_snapshot_versioned(None)?;
  // The query result's listing identity is checked first, then each stored file signature, before name matching or
  // content classification. Keep that hot path ahead of candidate rebuilds.
  let cached_and_interval = {
    let mut guard = memos.lock().expect("session search memo mutex is not poisoned");
    let sweep_interval = guard.sweep_interval;
    guard.cached_match(&needle).map(|cached| (cached, sweep_interval))
  };
  if let Some((cached, sweep_interval)) = cached_and_interval
    && Arc::ptr_eq(&cached.corpus, &all)
  {
    if cached.verification_holds(sweep_interval) {
      return Ok(SearchAnswer { rows: cached.rows.clone(), source: Some(cached) });
    }
    #[cfg(test)]
    {
      memos.lock().expect("session search memo mutex is not poisoned").repeat_sweeps += 1;
    }
    let signatures: Vec<Option<FileSignature>> =
      file_signatures(&cached.candidates, &cached.watched, sessions_handle.as_ref())
        .into_iter()
        .map(Result::ok)
        .collect();
    if cached.signatures == signatures {
      cached.mark_verified();
      return Ok(SearchAnswer { rows: cached.rows.clone(), source: Some(cached) });
    }
  }

  let work = memos.lock().expect("session search memo mutex is not poisoned").corpus_work(&all, &sessions_dir, store);
  let mut names = Vec::new();
  let mut name_hits = vec![false; all.len()];
  for (row, meta) in all.iter().enumerate() {
    if work.lowered_names[row].contains(&needle) {
      name_hits[row] = true;
      names.push(meta);
    }
  }
  names.sort_by_key(|left| Reverse(left.updated_at));
  names.truncate(result_limit);

  let trigrams = needle_trigrams(&needle);
  let bigrams = needle_bigrams(&needle);
  let high_bigrams = needle_high_bigrams(&needle);
  let content_index_settles = !(trigrams.is_empty() && bigrams.is_empty() && high_bigrams.is_empty());
  // A candidate's place in `content_hits` follows the corpus order, because the result's order breaks ties on
  // insertion order.
  let mut content_hits = vec![false; work.candidates.len()];
  // The candidates a content index ruled the needle out of without a signature read, so the verdict pass leaves them
  // out instead of asking their index again.
  let mut content_absent = vec![false; work.candidates.len()];
  // The signature of every candidate the sweep has read. A candidate the sweep stops before is never read, so its
  // slot stays None.
  let mut signatures: Vec<Option<FileSignature>> = vec![None; work.candidates.len()];
  // Every match already known counts toward the cap: a name hit answers the query without reading a chat file.
  let mut matched: Vec<UtcDateTime> =
    name_hits.iter().enumerate().filter(|(_, hit)| **hit).map(|(row, _)| all[row].updated_at).collect();
  let mut scan_failures = 0;

  // The result keeps the newest `result_limit` matches, so the sweep reads candidates newest-first and stops
  // once every unread candidate is older than the result's slot at rank `result_limit`: such a candidate cannot
  // enter the capped result, however its chat file reads. A common word matches most of the corpus, so this is the
  // difference between reading the corpus and reading the rows the answer can hold.
  //
  // While the answer is short, the next window is sized to the candidates the matches so far say it still needs: a
  // window that matches nothing means the answer sits far below it, so the rest of the corpus goes in one batch and
  // pays no further signature round trip.
  let mut processed = 0;
  let mut window = SWEEP_WINDOW_CANDIDATES;
  loop {
    if processed == work.newest_first.len() {
      break;
    }
    let cutoff = result_cutoff(&mut matched, result_limit);
    let end = match cutoff {
      // Every candidate at or above the cutoff can still change the answer, and every candidate below it cannot. A
      // full answer still checks those candidates a window at a time, because each match raises the cutoff and leaves
      // the candidates below the new one unread.
      Some(cutoff) => {
        let above =
          work.newest_first[processed..].partition_point(|index| all[work.candidate_rows[*index]].updated_at >= cutoff);
        if above == 0 {
          break;
        }
        (processed + SWEEP_WINDOW_CANDIDATES).min(processed + above)
      }
      None => (processed + window).min(work.newest_first.len()),
    };
    let matched_before = matched.len();
    let positions = &work.newest_first[processed..end];
    // A session whose name matched already answers the query, so its chat file is not read.
    let selection: Vec<usize> =
      positions.iter().copied().filter(|index| !name_hits[work.candidate_rows[*index]]).collect();
    // A file whose content index is younger than the sweep interval rules the needle out without a signature read,
    // and a needle occurs in none of most files. The rest of the selection is read by the signature batch.
    let mut unread: Vec<usize> = Vec::with_capacity(selection.len());
    {
      let mut guard = memos.lock().expect("session search memo mutex is not poisoned");
      let interval = guard.sweep_interval;
      let now = Instant::now();
      for index in &selection {
        let recent = if content_index_settles {
          guard.recent_absence(&work.candidates[*index], now, interval, &trigrams, &bigrams, &high_bigrams)
        } else {
          None
        };
        match recent {
          Some(signature) => {
            signatures[*index] = Some(signature);
            content_absent[*index] = true;
          }
          None => unread.push(*index),
        }
      }
    }
    #[cfg(test)]
    {
      memos.lock().expect("session search memo mutex is not poisoned").swept_candidates += unread.len();
    }
    for (index, answer) in unread.iter().zip(file_signatures(&work.candidates, &unread, sessions_handle.as_ref())) {
      signatures[*index] = match answer {
        Ok(signature) => Some(signature),
        Err(error) => {
          log_search_read_failed_once(memos, &all[work.candidate_rows[*index]].id, &error);
          None
        }
      };
    }

    // The per-file verdicts are read here, on the request's thread, and the files that need a scan are collected so
    // one batch can read them across threads.
    let mut scans: Vec<(usize, u64)> = Vec::new();
    let mut unindexed: Vec<usize> = Vec::new();
    {
      let mut guard = memos.lock().expect("session search memo mutex is not poisoned");
      for index in positions.iter().copied() {
        if name_hits[work.candidate_rows[index]] || content_absent[index] {
          continue;
        }
        let Some(signature) = signatures[index] else { continue };
        let path = &work.candidates[index].path;
        // The content index settles most fresh queries before the per-file roots are read.
        let needs_index = if content_index_settles {
          match guard.content_verdict(&work.candidates[index], signature, &trigrams, &bigrams, &high_bigrams) {
            ContentVerdict::Absent => continue,
            ContentVerdict::Possible => false,
            ContentVerdict::Missing => true,
          }
        } else {
          false
        };
        let (covered, start) = match guard.files.get_mut(path) {
          Some(entry) => (
            hit_root_covers(entry.hits.iter(), signature, &needle),
            absence_rescan_start(&entry.misses, signature, &needle),
          ),
          // A file with no memo entry has no recorded root: none covers it, and its scan starts at the first byte.
          None => (false, Some(0)),
        };
        if covered {
          content_hits[index] = true;
          matched.push(all[work.candidate_rows[index]].updated_at);
          continue;
        }
        let Some(start) = start else { continue };
        if needs_index {
          unindexed.push(index);
        }
        #[cfg(test)]
        {
          guard.scan_calls += 1;
        }
        scans.push((index, start));
      }
    }
    // A file with no fresh content index gets one, and the same pass searches it, so the query reads it once. A file
    // whose index rules the needle out leaves the scan batch here, while a file the index cannot rule out is searched
    // by the pass itself and leaves it too.
    if !unindexed.is_empty() {
      let answers = index_and_search_files(&work.candidates, &unindexed, &signatures, &needle);
      let mut indexed = HashSet::new();
      for (index, answer) in answers {
        let signature = signatures[index].expect("an indexed candidate holds its signature");
        match answer {
          Ok((trigrams, bigrams, high_bigrams, found)) => {
            let path = &work.candidates[index].path;
            memos.lock().expect("session search memo mutex is not poisoned").remember_content_index(
              &work.candidates[index],
              signature,
              trigrams,
              bigrams,
              high_bigrams,
            );
            memos
              .lock()
              .expect("session search memo mutex is not poisoned")
              .remember_root(path, &needle, signature, found);
            content_hits[index] = found;
            if found {
              matched.push(all[work.candidate_rows[index]].updated_at);
            }
            indexed.insert(index);
          }
          Err(error) => log_search_read_failed_once(memos, &all[work.candidate_rows[index]].id, &error),
        }
      }
      if !indexed.is_empty() {
        scans.retain(|(index, _)| !indexed.contains(index));
      }
    }
    if !scans.is_empty() {
      #[cfg(test)]
      {
        memos.lock().expect("session search memo mutex is not poisoned").scanned_candidates += scans.len();
      }
      for ((index, _), answer) in scans.iter().zip(scan_contents(&work.candidates, &scans, &signatures, &needle)) {
        let path = &work.candidates[*index].path;
        let signature = signatures[*index].expect("a scanned candidate holds its signature");
        match answer {
          Ok(true) => {
            memos
              .lock()
              .expect("session search memo mutex is not poisoned")
              .remember_root(path, &needle, signature, true);
            content_hits[*index] = true;
            matched.push(all[work.candidate_rows[*index]].updated_at);
          }
          Ok(false) => memos
            .lock()
            .expect("session search memo mutex is not poisoned")
            .remember_root(path, &needle, signature, false),
          Err(error) => {
            scan_failures += 1;
            log_search_read_failed_once(memos, &all[work.candidate_rows[*index]].id, &error);
          }
        }
      }
    }
    processed = end;
    if cutoff.is_none() {
      let found = matched.len() - matched_before;
      let need = result_limit.saturating_sub(matched.len());
      // The candidates the window's match rate says the answer still needs. A window that matched nothing leaves no
      // rate, and the answer then sits far below the rest of the corpus.
      window = need
        .checked_mul(positions.len())
        .and_then(|scaled| scaled.checked_div(found))
        .map_or(work.newest_first.len(), |next| next.max(SWEEP_WINDOW_CANDIDATES));
    }
  }

  names.extend(work.candidate_rows.iter().zip(&content_hits).filter(|(_, hit)| **hit).map(|(row, _)| &all[*row]));
  names.sort_by_key(|left| Reverse(left.updated_at));
  names.truncate(result_limit);
  if scan_failures == 0 {
    // The result holds at most the newest `result_limit` rows, so a candidate older than the last row cannot
    // enter a full result however its chat file changes. A repeat query re-checks only the candidates that could
    // enter it; a short result can still take any candidate, so it keeps watching all of them.
    let cutoff = names.get(result_limit - 1).map(|meta| meta.updated_at);
    let mut watched = Vec::new();
    let mut watched_signatures = Vec::new();
    for (index, (row, signature)) in work.candidate_rows.iter().zip(signatures).enumerate() {
      if name_hits[*row] {
        continue;
      }
      if cutoff.is_none_or(|cutoff| all[*row].updated_at >= cutoff) {
        watched.push(index);
        watched_signatures.push(signature);
      }
    }
    let rows: Vec<Arc<SessionMetadata>> = names.into_iter().cloned().collect();
    let source = memos.lock().expect("session search memo mutex is not poisoned").remember_match(
      &needle,
      SearchMatch {
        corpus: Arc::clone(&all),
        candidates: Arc::clone(&work.candidates),
        watched,
        signatures: watched_signatures,
        rows: rows.clone(),
        body: Mutex::new(None),
        verified_at: Mutex::new(Instant::now()),
      },
    );
    return Ok(SearchAnswer { rows, source: Some(source) });
  }
  Ok(SearchAnswer { rows: names.into_iter().map(Arc::clone).collect(), source: None })
}

/// The signatures of the selected paths, in the selection's order.
///
/// Each read is one `stat` and the answers do not depend on each other, so a batch reads on the signature pool's
/// threads. The pool lives for the process: a repeat query re-checks one `stat` per watched file, 200 on the mock
/// home, and starting that many threads costs more than the reads they share.
///
/// When `sessions_dir` is open, a read resolves the file below it instead of walking the home's whole path to the
/// session's `data` directory, which is most of the route's kernel time on a repeat query.
fn file_signatures(
  paths: &Arc<Vec<WatchedFile>>,
  selection: &[usize],
  sessions_dir: Option<&Arc<File>>,
) -> Vec<io::Result<FileSignature>> {
  let pool = &*SIGNATURE_POOL;
  if selection.len() < SIGNATURE_BATCH_MIN_PATHS || pool.senders.len() < 2 {
    return selection.iter().map(|index| file_signature(&paths[*index], sessions_dir.map(Arc::as_ref))).collect();
  }
  let selection = Arc::new(selection.to_vec());
  let chunk = selection.len().div_ceil(pool.senders.len()).max(SIGNATURE_BATCH_CHUNK_FLOOR);
  let mut chunks: Vec<Option<Vec<io::Result<FileSignature>>>> = selection.chunks(chunk).map(|_| None).collect();
  let (reply, answers) = mpsc::channel();
  for (index, _) in chunks.iter().enumerate() {
    let start = index * chunk;
    let end = (start + chunk).min(selection.len());
    pool.dispatch(SignatureJob {
      paths: Arc::clone(paths),
      selection: Arc::clone(&selection),
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
  let mut out = Vec::with_capacity(selection.len());
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

/// One chunk of one signature batch: the shared paths, the candidate indices it reads, the sessions directory handle,
/// the chunk's half-open range into the indices, and its reply channel.
struct SignatureJob {
  paths: Arc<Vec<WatchedFile>>,
  selection: Arc<Vec<usize>>,
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
    let signatures =
      job.selection[job.start..job.end].iter().map(|index| file_signature(&job.paths[*index], sessions_dir)).collect();
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
  let key = path_key(&path);
  WatchedFile { path, key, below_sessions }
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

fn hit_root_covers<'a>(
  roots: impl Iterator<Item = (&'a String, &'a FileSignature)>,
  signature: FileSignature,
  needle: &str,
) -> bool {
  roots.into_iter().any(|(root, root_signature)| {
    root.contains(needle) && root_signature.inode == signature.inode && signature.size >= root_signature.size
  })
}

/// `None` means a stored absence already proves this query; otherwise return the byte offset to re-scan.
fn absence_rescan_start(roots: &RootMemo, signature: FileSignature, needle: &str) -> Option<u64> {
  for (root, root_signature) in roots.iter() {
    if needle.starts_with(root) && *root_signature == signature {
      return None;
    }
  }
  let best_size = roots
    .iter()
    .filter(|(root, root_signature)| {
      needle.starts_with(root.as_str())
        && root_signature.inode == signature.inode
        && signature.size > root_signature.size
    })
    .map(|(_, root_signature)| root_signature.size)
    .max();
  Some(best_size.map_or(0, |size| size.saturating_sub((4 * needle.chars().count() + 8) as u64)))
}

/// The `updated_at` of the oldest row a full result holds, or `None` while the matches are fewer than the cap.
///
/// A candidate older than this value cannot enter the answer however its chat file reads, because the cap already
/// holds `limit` matches at or above it.
fn result_cutoff(matched: &mut [UtcDateTime], limit: usize) -> Option<UtcDateTime> {
  if matched.len() < limit {
    return None;
  }
  let (_, cutoff, _) = matched.select_nth_unstable_by(limit - 1, |left, right| right.cmp(left));
  Some(*cutoff)
}

/// The trigrams a lowercase ASCII needle holds; a window with any other byte contributes none.
///
/// Every window of the needle's own bytes is a window of any text that holds the needle, so an index that lacks one
/// of these trigrams proves the needle absent. A window the index cannot encode proves nothing and is dropped.
fn needle_trigrams(needle: &str) -> Vec<usize> {
  if !needle.is_ascii() {
    return Vec::new();
  }
  let codes = &*CONTENT_CODES;
  let bytes = needle.as_bytes();
  bytes
    .windows(3)
    .filter_map(|window| {
      trigram_number(codes[window[0] as usize], codes[window[1] as usize], codes[window[2] as usize])
    })
    .collect()
}

/// The bigrams a lowercase ASCII needle holds; a pair of letters and a pair holding a byte outside [`BIGRAM_SYMBOLS`]
/// contribute none.
///
/// A bigram holds a digit or a symbol the trigram index cannot, so a needle such as an identifier or a version number
/// becomes provable absent. A pair of letters adds nothing the trigram index does not already prove.
fn needle_bigrams(needle: &str) -> Vec<usize> {
  if !needle.is_ascii() {
    return Vec::new();
  }
  let codes = &*CONTENT_CODES;
  let bytes = needle.as_bytes();
  bytes.windows(2).filter_map(|window| bigram_number(codes[window[0] as usize], codes[window[1] as usize])).collect()
}

/// The high-byte bigrams a needle holds: one for each adjacent pair of bytes at or above 0x80.
///
/// Every multibyte character's UTF-8 bytes lie at or above 0x80, so a needle that holds one carries at least one of
/// these pairs, and the trigram and bigram indexes cannot encode it.
///
/// Only a needle the raw-byte scan handles gets them. The index folds ASCII only, while any other needle is scanned
/// with Unicode lowercase folding, and that fold can produce bytes the index never saw: U+0130 lowers to `i` and
/// U+0307, whose bytes an index built from a file that holds U+0130 lacks.
fn needle_high_bigrams(needle: &str) -> Vec<usize> {
  if needle.is_ascii() || !uncased_alphanumeric(needle) {
    return Vec::new();
  }
  needle.as_bytes().windows(2).filter_map(|window| high_bigram_number(window[0], window[1])).collect()
}

/// The bit position of three letter codes, or None when one code is not a lowercase letter.
///
/// Each code is checked: a later code that is not a letter would still sum below [`TRIGRAM_BITS`] and alias the bit of
/// a letter sequence the text does not hold.
fn trigram_number(first: u8, second: u8, third: u8) -> Option<usize> {
  if first >= TRIGRAM_LETTERS || second >= TRIGRAM_LETTERS || third >= TRIGRAM_LETTERS {
    return None;
  }
  Some(first as usize * 676 + second as usize * 26 + third as usize)
}

/// The bit position of a pair of bytes at or above 0x80, or None when either byte lies below that.
fn high_bigram_number(first: u8, second: u8) -> Option<usize> {
  if first < 0x80 || second < 0x80 {
    return None;
  }
  Some((first as usize - 0x80) * 128 + (second as usize - 0x80))
}

/// The bit position of two symbol codes, or None when a code is not a [`BIGRAM_SYMBOLS`] entry or both codes are
/// letters.
///
/// A pair of letters needs no bit: the trigram index already holds every letter sequence, and giving most positions a
/// bigram would double the first query's indexing pass for evidence the next query does not need.
fn bigram_number(first: u8, second: u8) -> Option<usize> {
  let symbols = BIGRAM_SYMBOLS.len() as u8;
  if first >= symbols || second >= symbols || (first < TRIGRAM_LETTERS && second < TRIGRAM_LETTERS) {
    return None;
  }
  Some(first as usize * BIGRAM_SYMBOLS.len() + second as usize)
}

/// One file's content index and whether the needle occurs in it, from one pass over its bytes.
///
/// The needle search matches the scan's rule: both read the text with ASCII case folding, so a needle the index
/// cleared the file for can still match here. `window` carries the last bytes of each chunk, which keeps a trigram,
/// a bigram and a needle that span the boundary.
fn index_and_search_file(path: &Path, needle: &str) -> ContentIndexAnswer {
  let mut file = File::open(path)?;
  let mut trigrams = vec![0u8; TRIGRAM_BYTES];
  let mut bigrams = vec![0u8; BIGRAM_BYTES];
  let mut high_bigrams = vec![0u8; HIGH_BIGRAM_BYTES];
  let finder = memchr::memmem::Finder::new(needle.as_bytes());
  let codes = &*CONTENT_CODES;
  let overlap = needle.len().saturating_sub(1).max(2);
  let mut carry: Vec<u8> = Vec::with_capacity(overlap);
  let mut chunk = vec![0u8; SEARCH_CHUNK_SIZE];
  let mut window: Vec<u8> = Vec::with_capacity(overlap + SEARCH_CHUNK_SIZE);
  let mut matched = false;
  loop {
    let count = file.read(&mut chunk)?;
    if count == 0 {
      return Ok((trigrams.into_boxed_slice(), bigrams.into_boxed_slice(), high_bigrams.into_boxed_slice(), matched));
    }
    window.clear();
    window.extend_from_slice(&carry);
    window.extend_from_slice(&chunk[..count]);
    window.make_ascii_lowercase();
    // One pass sets all three bitmaps: a second pass over the window would stream the whole file through memory again.
    // The loop ends two bytes short of the window, so the last pair's bigram follows it. ASCII folding leaves the high
    // bytes unchanged, so the high-byte pairs are the text's own.
    let last_pair = window.len().saturating_sub(2);
    for position in 0..last_pair {
      let first = codes[window[position] as usize];
      let second = codes[window[position + 1] as usize];
      let third = codes[window[position + 2] as usize];
      if let Some(index) = bigram_number(first, second) {
        bigrams[index >> 3] |= 1u8 << (index & 7);
      }
      if let Some(index) = trigram_number(first, second, third) {
        trigrams[index >> 3] |= 1u8 << (index & 7);
      }
      if let Some(index) = high_bigram_number(window[position], window[position + 1]) {
        high_bigrams[index >> 3] |= 1u8 << (index & 7);
      }
    }
    if window.len() >= 2 {
      let first = codes[window[window.len() - 2] as usize];
      let second = codes[window[window.len() - 1] as usize];
      if let Some(index) = bigram_number(first, second) {
        bigrams[index >> 3] |= 1u8 << (index & 7);
      }
      if let Some(index) = high_bigram_number(window[window.len() - 2], window[window.len() - 1]) {
        high_bigrams[index >> 3] |= 1u8 << (index & 7);
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
/// The split is why the answers come in no fixed order; the caller pairs each answer with the index it carries.
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
  let bins =
    size_balanced_bins(jobs.len(), |slot| signatures[jobs[slot]].map_or(0, |signature| signature.size), workers);
  let mut answers = Vec::with_capacity(jobs.len());
  std::thread::scope(|scope| {
    let handles: Vec<_> = bins
      .into_iter()
      .filter(|bin| !bin.is_empty())
      .map(|bin| {
        scope.spawn(move || {
          bin
            .into_iter()
            .map(|slot| (jobs[slot], index_and_search_file(&paths[jobs[slot]].path, needle)))
            .collect::<Vec<_>>()
        })
      })
      .collect();
    for handle in handles {
      answers.extend(handle.join().expect("an index worker finishes without panic"));
    }
  });
  answers
}

fn scan_content(path: &Path, needle: &str, start: u64, buffer: &mut ScanBuffer) -> io::Result<bool> {
  let mut file = File::open(path)?;
  file.seek(SeekFrom::Start(start))?;
  if needle.is_ascii() {
    return scan_ascii(&mut file, needle.as_bytes(), buffer);
  }
  // Folding the text is the scan's expensive half. On the 7,160-session mock home, a Chinese needle spends four
  // fifths of its user CPU in Unicode lowercase conversion and UTF-8 validation, against under 1% in the needle
  // search. A needle folding cannot change reads the file's bytes directly instead.
  if uncased_alphanumeric(needle) {
    return scan_bytes(&mut file, needle.as_bytes(), false, buffer);
  }
  scan_unicode(&mut file, needle, start > 0)
}

/// Whether Unicode case folding leaves every occurrence of `needle` exactly where the raw bytes hold it.
///
/// Folding rewrites only cased characters, and the characters it produces are cased or combining marks: Unicode's
/// only unconditional lowercase expansion, U+0130, yields `i` and a combining dot. A needle of uncased
/// alphanumerics — Han characters, digits, uncased letters — is therefore present in the folded text exactly when
/// the file's own bytes carry it. The scan then searches the bytes and skips decoding and lowering every chunk.
fn uncased_alphanumeric(needle: &str) -> bool {
  needle.chars().all(|c| c.is_alphanumeric() && !c.is_lowercase() && !c.is_uppercase())
}

/// The read buffer one scan worker reuses across the files it scans.
///
/// A query scans thousands of files, so a fresh zeroed buffer per file would cost more than the reads it holds. The
/// buffer keeps each read's last bytes in front of the next read, so a needle that spans a chunk boundary is still
/// one contiguous window.
struct ScanBuffer {
  bytes: Vec<u8>,
}

impl ScanBuffer {
  fn new(overlap: usize) -> Self {
    Self { bytes: vec![0_u8; overlap + SEARCH_CHUNK_SIZE] }
  }
}

fn scan_ascii(file: &mut File, needle: &[u8], buffer: &mut ScanBuffer) -> io::Result<bool> {
  scan_bytes(file, needle, true, buffer)
}

/// The needle searched for in each window of the file's bytes, with each window ASCII-folded when `fold_ascii`.
fn scan_bytes(file: &mut File, needle: &[u8], fold_ascii: bool, buffer: &mut ScanBuffer) -> io::Result<bool> {
  let overlap = needle.len().saturating_sub(1);
  // memchr's memmem is the two-way search behind the substring test, built once per scan over the windows.
  let finder = memchr::memmem::Finder::new(needle);
  let mut carry = 0;
  loop {
    let count = file.read(&mut buffer.bytes[carry..])?;
    if count == 0 {
      return Ok(false);
    }
    let filled = carry + count;
    let window = &mut buffer.bytes[..filled];
    if fold_ascii {
      window.make_ascii_lowercase();
    }
    if finder.find(window).is_some() {
      return Ok(true);
    }
    carry = filled.min(overlap);
    buffer.bytes.copy_within(filled - carry..filled, 0);
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

/// Split `jobs` across `workers` bins so the heaviest bin holds the least bytes it can.
///
/// The batches' files range over four orders of magnitude, and a batch waits on its heaviest worker. A file count per
/// worker would hand one worker a 144 MB file beside its share of the small ones.
fn size_balanced_bins(jobs: usize, size: impl Fn(usize) -> u64, workers: usize) -> Vec<Vec<usize>> {
  let mut order: Vec<usize> = (0..jobs).collect();
  order.sort_by_key(|job| Reverse(size(*job)));
  let mut bins: Vec<Vec<usize>> = (0..workers).map(|_| Vec::new()).collect();
  let mut loads = vec![0u64; workers];
  for job in order {
    let lightest = (0..workers).min_by_key(|worker| loads[*worker]).expect("a worker exists");
    loads[lightest] += size(job);
    bins[lightest].push(job);
  }
  bins
}

/// The scan answer of every job, in the jobs' order.
///
/// A scan reads its file from the start offset to the end. The jobs touch different files, so a batch long
/// enough to pay for them runs the jobs on their own threads, each holding one read buffer for every file it scans.
fn scan_contents(
  paths: &[WatchedFile],
  scans: &[(usize, u64)],
  signatures: &[Option<FileSignature>],
  needle: &str,
) -> Vec<io::Result<bool>> {
  let workers = std::thread::available_parallelism().map_or(1, |count| count.get()).min(SCAN_BATCH_WORKERS);
  if scans.len() < SCAN_BATCH_MIN_JOBS || workers < 2 {
    let mut buffer = ScanBuffer::new(needle.len().saturating_sub(1));
    return scans.iter().map(|(index, start)| scan_content(&paths[*index].path, needle, *start, &mut buffer)).collect();
  }
  let bins =
    size_balanced_bins(scans.len(), |slot| signatures[scans[slot].0].map_or(0, |signature| signature.size), workers);
  let mut answers: Vec<Option<io::Result<bool>>> = scans.iter().map(|_| None).collect();
  std::thread::scope(|scope| {
    let handles: Vec<_> = bins
      .into_iter()
      .filter(|bin| !bin.is_empty())
      .map(|bin| {
        scope.spawn(move || {
          let mut buffer = ScanBuffer::new(needle.len().saturating_sub(1));
          bin
            .into_iter()
            .map(|slot| {
              let (index, start) = scans[slot];
              (slot, scan_content(&paths[index].path, needle, start, &mut buffer))
            })
            .collect::<Vec<_>>()
        })
      })
      .collect();
    for handle in handles {
      for (slot, answer) in handle.join().expect("a scan worker finishes without panic") {
        answers[slot] = Some(answer);
      }
    }
  });
  answers.into_iter().map(|answer| answer.expect("every scan job answers")).collect()
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

    assert!(super::scan_ascii(&mut file, b"boundary-needle", &mut super::ScanBuffer::new(14)).unwrap());
  }

  #[test]
  fn ascii_scan_reports_absence_and_a_match_at_the_first_byte() {
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(b"needle at the start, then synthetic filler").unwrap();
    file.seek(SeekFrom::Start(0)).unwrap();
    assert!(super::scan_ascii(&mut file, b"needle at", &mut super::ScanBuffer::new(8)).unwrap());
    file.seek(SeekFrom::Start(0)).unwrap();
    assert!(!super::scan_ascii(&mut file, b"absent-needle", &mut super::ScanBuffer::new(12)).unwrap());
  }

  /// The raw scan reads the needle's own bytes. A Han needle is six or more bytes for two characters, so a chunk
  /// boundary can fall inside it; the buffer's overlap must keep the whole needle.
  #[test]
  fn raw_scan_finds_a_multibyte_needle_across_a_chunk_boundary() {
    let mut file = tempfile::tempfile().unwrap();
    let mut body = vec![b'x'; super::SEARCH_CHUNK_SIZE - 2];
    body.extend_from_slice("缓存层".as_bytes());
    file.write_all(&body).unwrap();
    file.seek(SeekFrom::Start(0)).unwrap();
    assert!(super::scan_bytes(&mut file, "缓存".as_bytes(), false, &mut super::ScanBuffer::new(5)).unwrap());
    file.seek(SeekFrom::Start(0)).unwrap();
    assert!(!super::scan_bytes(&mut file, "调度".as_bytes(), false, &mut super::ScanBuffer::new(5)).unwrap());
  }

  /// The raw scan serves needles that folding cannot move: Han characters, digits and other uncased characters.
  /// A cased or combining character keeps the folded scan, because folding can rewrite or create it.
  #[test]
  fn only_an_uncased_alphanumeric_needle_takes_the_raw_scan() {
    for needle in ["缓存", "2026", "٣٤"] {
      assert!(super::uncased_alphanumeric(needle), "{needle}");
    }
    for needle in ["Cache", "café", "ä", "\u{307}", "two words", "a-b"] {
      assert!(!super::uncased_alphanumeric(needle), "{needle}");
    }
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
    let selection: Vec<usize> = (0..paths.len()).collect();
    let batch = super::file_signatures(&paths, &selection, Some(&handle));
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
      let mut body =
        if index % 7 == 0 { format!("event {index} carries the needle\n") } else { format!("event {index}\n") };
      // Files of different sizes take different shares of a size-balanced batch.
      body.push_str(&".".repeat(index * 37));
      fs::write(&path, body).unwrap();
      paths.push(super::watched_file(home.path(), path));
    }
    let scans: Vec<(usize, u64)> = (0..paths.len()).map(|index| (index, 0)).collect();
    let signatures: Vec<Option<super::FileSignature>> =
      paths.iter().map(|file| super::file_signature_unlogged(&file.path).ok()).collect();
    let answers = super::scan_contents(&paths, &scans, &signatures, "needle");
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

  fn write_archived_sessions(store: &Store, count: usize) {
    let mut template = SessionMetadata::new("archived-000", "needle-000", TaskProfile::Manager);
    template.status = SessionStatus::Archived;
    template.updated_at = UtcDateTime::parse("2026-08-01T12:00:00Z").unwrap();
    let template_timestamp = template.updated_at.to_str();
    let template_json = template.to_json_string(&[], true);
    let sessions = (0..count)
      .map(|index| {
        let id = format!("archived-{index:03}");
        let name = format!("needle-{index:03}");
        let timestamp = format!("2026-08-01T{:02}:{:02}:00Z", 12 + index / 60, index % 60);
        let contents = template_json
          .replace("archived-000", &id)
          .replace("needle-000", &name)
          .replace(&template_timestamp, &timestamp);
        (store.metadata_path(&id), contents)
      })
      .collect::<Vec<_>>();
    std::thread::scope(|scope| {
      for chunk in sessions.chunks(25) {
        scope.spawn(move || {
          for (path, contents) in chunk {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
          }
        });
      }
    });
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

  /// A full result stops the sweep once every unread session is older than the answer's last row, and it still holds
  /// exactly the newest matches.
  ///
  /// Eight name hits fill the cap on their own. Two newer content hits enter the answer and push the cutoff to the
  /// sixth name hit; eight older content hits are past the
  /// cutoff and stay unread. Rank 0 is the newest session.
  #[test]
  fn a_full_result_stops_the_scan_at_the_newest_matches() {
    let home = tempfile::tempdir().unwrap();
    let store = test_store(home.path());
    let limit = 8;
    let stamp = |rank: usize| {
      let minutes = 700 - rank;
      format!("2026-08-01T{:02}:{:02}:00Z", 12 + minutes / 60, minutes % 60)
    };
    let content = |id: &str, rank: usize| {
      write_session(&store, id, id, SessionStatus::Active, &stamp(rank));
      let path = store.chat_events_path(id);
      fs::create_dir_all(path.parent().unwrap()).unwrap();
      fs::write(&path, "needle in the events\n").unwrap();
    };
    for index in 0..limit {
      write_session(
        &store,
        &format!("name-{index:03}"),
        &format!("needle name {index:03}"),
        SessionStatus::Active,
        &stamp(100 + index),
      );
    }
    for index in 0..2 {
      content(&format!("new-{index}"), index);
    }
    for index in 0..8 {
      content(&format!("old-{index}"), 400 + index);
    }
    let memos = Mutex::new(super::SearchMemos::default());
    // The first query indexes every chat file; the second reaches the first sweep window through that index.
    assert!(super::search_sessions_with_limit(&store, "zzzq", &memos, limit).unwrap().rows.is_empty());

    let rows = super::search_sessions_with_limit(&store, "needle", &memos, limit).unwrap().rows;
    let ids: Vec<&str> = rows.iter().map(|meta| meta.id.as_str()).collect();
    assert_eq!(ids.len(), limit);
    assert_eq!(ids.first().copied(), Some("new-0"), "the newest content hit leads the answer");
    assert_eq!(ids[2], "name-000", "the name hits follow the content hits");
    assert_eq!(ids.last().copied(), Some("name-005"), "the answer stops at the newest matches");
    assert!(!ids.iter().any(|id| id.starts_with("old-")), "the rows past the cutoff stay out");
    assert_eq!(
      memos.lock().unwrap().scanned_candidates,
      2,
      "the sweep read only the newer candidates and left the older files unread"
    );
  }

  /// A query whose matching names fill the cap reads no chat file: the name hits already hold every slot of the
  /// answer, and the sweep stops before it reads a candidate older than the answer's last row.
  #[test]
  fn name_hits_that_fill_the_cap_read_no_chat_file() {
    let home = tempfile::tempdir().unwrap();
    let store = test_store(home.path());
    let limit = 8;
    for index in 0..limit {
      let id = format!("name-{index:03}");
      let minutes = 200 - index;
      write_session(
        &store,
        &id,
        &format!("needle name {index:03}"),
        SessionStatus::Active,
        &format!("2026-08-01T{:02}:{:02}:00Z", 12 + minutes / 60, minutes % 60),
      );
      // The chat file holds the needle too, so a sweep that ran would read it and report a scan.
      let path = store.chat_events_path(&id);
      fs::create_dir_all(path.parent().unwrap()).unwrap();
      fs::write(&path, "needle in the events\n").unwrap();
    }
    let memos = Mutex::new(super::SearchMemos::default());

    let rows = super::search_sessions_with_limit(&store, "needle", &memos, limit).unwrap().rows;
    let ids: Vec<&str> = rows.iter().map(|meta| meta.id.as_str()).collect();
    assert_eq!(ids.len(), limit);
    assert_eq!(ids.first().copied(), Some("name-000"), "the newest name hit leads the answer");
    let counters = memos.lock().unwrap();
    assert_eq!(counters.swept_candidates, 0, "the name hits filled the cap, so no chat file's signature was read");
    assert_eq!(counters.scan_calls, 0, "no chat file was opened");
  }

  /// Rows tied at the answer's last timestamp keep corpus order, so the cap drops the corpus-last of them: the cutoff
  /// selection reorders `matched`'s timestamps, and only their values reach the stop test.
  #[test]
  fn rows_tied_at_the_answers_last_row_keep_corpus_order() {
    let home = tempfile::tempdir().unwrap();
    let store = test_store(home.path());
    let limit = 8;
    // One more match than the cap, all written at the same instant and all holding the needle in their chat file.
    for index in 0..limit + 1 {
      let id = format!("tied-{index:03}");
      write_session(&store, &id, &id, SessionStatus::Active, "2026-08-01T12:00:00Z");
      let path = store.chat_events_path(&id);
      fs::create_dir_all(path.parent().unwrap()).unwrap();
      fs::write(&path, "needle in the events\n").unwrap();
    }
    let (all, _) = store.load_session_metas_snapshot_versioned(None).unwrap();
    let expected: Vec<&str> = all.iter().take(limit).map(|meta| meta.id.as_str()).collect();

    let memos = Mutex::new(super::SearchMemos::default());
    let rows = super::search_sessions_with_limit(&store, "needle", &memos, limit).unwrap().rows;
    let ids: Vec<&str> = rows.iter().map(|meta| meta.id.as_str()).collect();
    assert_eq!(ids, expected, "the tied rows answer in corpus order and the cap drops the corpus-last one");
  }

  #[test]
  fn content_hits_compete_with_name_hits_at_the_two_hundred_row_cap() {
    let home = tempfile::tempdir().unwrap();
    let store = test_store(home.path());
    write_archived_sessions(&store, 200);
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

  /// A Han needle holds no case, so its content scan reads the file's bytes. The scan must still find the needle
  /// in chat content and report an absent needle as absent.
  #[test]
  fn a_han_needle_matches_chat_content_and_reports_absence() {
    let home = tempfile::tempdir().unwrap();
    let store = test_store(home.path());
    write_session(&store, "carrier", "carrier", SessionStatus::Active, "2026-08-01T12:00:00Z");
    let events = store.chat_events_path("carrier");
    fs::create_dir_all(events.parent().unwrap()).unwrap();
    fs::write(&events, "{\"content\":\"这个缓存的失效规则目前只看时间\"}\n").unwrap();

    let rows = search_sessions(&store, "缓存").unwrap().rows;
    assert_eq!(rows.iter().map(|meta| meta.id.as_str()).collect::<Vec<_>>(), ["carrier"]);
    assert!(search_sessions(&store, "缓存好").unwrap().rows.is_empty(), "an absent Han needle matches nothing");
  }

  /// A cased non-ASCII needle keeps the folded scan: the file's upper-case text holds the needle's lower-case form.
  #[test]
  fn a_cased_non_ascii_needle_still_matches_folded_text() {
    let home = tempfile::tempdir().unwrap();
    let store = test_store(home.path());
    write_session(&store, "carrier", "carrier", SessionStatus::Active, "2026-08-01T12:00:00Z");
    let events = store.chat_events_path("carrier");
    fs::create_dir_all(events.parent().unwrap()).unwrap();
    fs::write(&events, "{\"content\":\"ÄPFEL und Birnen\"}\n").unwrap();

    let rows = search_sessions(&store, "äpfel").unwrap().rows;
    assert_eq!(rows.iter().map(|meta| meta.id.as_str()).collect::<Vec<_>>(), ["carrier"]);
  }

  /// The request-thread repeat answer serves a body only while the corpus the match was taken from, the file check
  /// behind it and the derived-state generation all still hold: any one that moved sends the request to the blocking
  /// path, which reads the files the answer depends on.
  #[test]
  fn a_repeat_answer_serves_only_the_corpus_check_and_generation_its_body_holds_for() {
    let home = tempfile::tempdir().unwrap();
    let store = test_store(home.path());
    write_session(&store, "content", "content session", SessionStatus::Active, "2026-08-01T12:00:00Z");
    let path = store.chat_events_path("content");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "a needle for the repeat answer").unwrap();
    let memos =
      Mutex::new(super::SearchMemos { sweep_interval: std::time::Duration::from_secs(3600), ..Default::default() });

    let first = super::search_sessions_with_memos(&store, "needle", &memos).unwrap();
    first.source.expect("a fresh query names the match it built").remember_body(7, Arc::new(b"needle body".to_vec()));
    let (corpus, _) = store.load_session_metas_snapshot_versioned(None).unwrap();

    let (rows, body) = super::repeat_answer_with_memos("needle", &corpus, 7, &memos)
      .expect("the repeat answer serves the body its match built for the generation the request read");
    assert_eq!(body.to_vec(), b"needle body".to_vec());
    assert_eq!(
      rows.iter().map(|meta| meta.id.as_str()).collect::<Vec<_>>(),
      ["content"],
      "the rows come with the body, so the route can count its sidebar read"
    );
    assert!(
      super::repeat_answer_with_memos("needle", &corpus, 8, &memos).is_none(),
      "a moved derived-state generation takes the blocking path"
    );
    assert!(
      super::repeat_answer_with_memos("other", &corpus, 7, &memos).is_none(),
      "a query the memo does not hold takes the blocking path"
    );
    assert!(
      super::repeat_answer_with_memos("needle", &Arc::new(Vec::new()), 7, &memos).is_none(),
      "a corpus the match was not taken from takes the blocking path"
    );
    assert!(
      super::repeat_answer_with_memos("  ", &corpus, 7, &memos).is_none(),
      "a blank query lists sessions and takes the blocking path"
    );
    memos.lock().unwrap().sweep_interval = std::time::Duration::ZERO;
    assert!(
      super::repeat_answer_with_memos("needle", &corpus, 7, &memos).is_none(),
      "a lapsed file check takes the blocking path, which reads the watched signatures"
    );
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
    assert_eq!(
      memos.lock().unwrap().repeat_sweeps,
      3,
      "with no interval the memo re-reads the watched signatures on every repeat"
    );
  }

  /// A repeat query inside [`super::SEARCH_SWEEP_INTERVAL`] answers from the match it shares, so it reads no chat
  /// file's signature: the sweep is one `stat` per watched file, and every active session's file is watched while the
  /// answer is short of the cap.
  #[test]
  fn a_repeat_query_inside_the_sweep_interval_reads_no_signature() {
    let home = tempfile::tempdir().unwrap();
    let store = test_store(home.path());
    for index in 0..4 {
      let id = format!("content-{index}");
      write_session(&store, &id, &format!("session {index}"), SessionStatus::Active, "2026-08-01T12:00:00Z");
      let path = store.chat_events_path(&id);
      fs::create_dir_all(path.parent().unwrap()).unwrap();
      fs::write(&path, "no match here\n").unwrap();
    }
    let memos =
      Mutex::new(super::SearchMemos { sweep_interval: std::time::Duration::from_secs(1), ..Default::default() });

    let first = super::search_sessions_with_memos(&store, "absentneedle", &memos).unwrap();
    assert!(first.rows.is_empty());
    let repeat = super::search_sessions_with_memos(&store, "absentneedle", &memos).unwrap();
    assert!(
      Arc::ptr_eq(first.source.as_ref().unwrap(), repeat.source.as_ref().unwrap()),
      "the repeat query answers from the memo the first query built"
    );
    assert_eq!(memos.lock().unwrap().repeat_sweeps, 0, "the repeat query trusts the signatures the first query read");
  }

  /// A fresh query inside the sweep interval answers from the content indexes alone, and one at an exhausted interval
  /// reads the signatures again: the index's age decides whether a file's signature is read.
  #[test]
  fn a_fresh_query_inside_the_sweep_interval_reads_no_signature() {
    let home = tempfile::tempdir().unwrap();
    let store = test_store(home.path());
    for index in 0..4 {
      let id = format!("content-{index}");
      write_session(&store, &id, &format!("session {index}"), SessionStatus::Active, "2026-08-01T12:00:00Z");
      let path = store.chat_events_path(&id);
      fs::create_dir_all(path.parent().unwrap()).unwrap();
      fs::write(&path, "no match here\n").unwrap();
    }
    let memos =
      Mutex::new(super::SearchMemos { sweep_interval: std::time::Duration::from_secs(1), ..Default::default() });

    assert!(super::search_sessions_with_memos(&store, "walrus", &memos).unwrap().rows.is_empty());
    assert_eq!(memos.lock().unwrap().swept_candidates, 4, "the first query reads every candidate's signature");
    assert!(super::search_sessions_with_memos(&store, "walruskin", &memos).unwrap().rows.is_empty());
    assert_eq!(memos.lock().unwrap().swept_candidates, 4, "the fresh query answers from the indexes alone");

    let memos = Mutex::new(super::SearchMemos::default());
    assert!(super::search_sessions_with_memos(&store, "walrus", &memos).unwrap().rows.is_empty());
    assert!(super::search_sessions_with_memos(&store, "walruskin", &memos).unwrap().rows.is_empty());
    assert_eq!(memos.lock().unwrap().swept_candidates, 8, "an interval of zero reads every candidate's signature");
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

  /// Two paths whose keys collide must not answer with each other's index: the entry names the file it was built for.
  #[test]
  fn a_content_index_key_collision_answers_as_a_missing_index() {
    let mut memos = super::SearchMemos::default();
    let file = |path: &str| super::WatchedFile { path: std::path::PathBuf::from(path), key: 7, below_sessions: None };
    let first = file("/home/sessions/first/data/chat_events.jsonl");
    let second = file("/home/sessions/second/data/chat_events.jsonl");
    let signature = super::FileSignature { mtime_ns: 1, size: 2, inode: 3 };
    memos.remember_content_index(
      &first,
      signature,
      vec![u8::MAX; super::TRIGRAM_BYTES].into_boxed_slice(),
      vec![u8::MAX; super::BIGRAM_BYTES].into_boxed_slice(),
      vec![u8::MAX; super::HIGH_BIGRAM_BYTES].into_boxed_slice(),
    );

    assert!(
      matches!(memos.content_verdict(&first, signature, &[0], &[0], &[0]), super::ContentVerdict::Possible),
      "the file the index was built for reads its own index"
    );
    assert!(
      matches!(memos.content_verdict(&second, signature, &[0], &[0], &[0]), super::ContentVerdict::Missing),
      "a colliding key answers as no index, so the file is read again"
    );
  }

  /// A non-letter in any position must name no trigram: the combined sum alone would alias another sequence's bit.
  #[test]
  fn a_window_with_a_non_letter_names_no_trigram() {
    let codes = &*super::CONTENT_CODES;
    let code = |byte: u8| codes[byte as usize];
    assert_eq!(super::trigram_number(code(b'a'), code(b'{'), code(b'a')), None);
    assert_eq!(super::trigram_number(code(b'a'), code(b'b'), code(b'{')), None);
    assert_eq!(super::trigram_number(code(b'{'), code(b'b'), code(b'c')), None);
    assert_eq!(super::trigram_number(code(b'a'), code(b'b'), code(b'c')), Some(28));
    // The bigram index keeps the symbols the trigram index drops.
    assert_eq!(super::bigram_number(code(b'a'), code(b'1')), Some(27));
    assert_eq!(super::bigram_number(code(b'a'), code(b'b')), None);
    assert_eq!(super::bigram_number(code(b'a'), code(b'\r')), None);
  }

  #[test]
  fn the_content_index_holds_trigrams_across_chunk_boundaries() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("chat.jsonl");
    let mut body = vec![b'x'; super::SEARCH_CHUNK_SIZE - 1];
    body.extend_from_slice(b"Boundary-Needle");
    fs::write(&path, &body).unwrap();

    let (trigrams, bigrams, _, found) = super::index_and_search_file(&path, "boundary-needle").unwrap();
    assert!(found, "the needle search reads the text with ASCII case folding");
    for trigram in super::needle_trigrams("boundary-needle") {
      assert_ne!(trigrams[trigram >> 3] & (1u8 << (trigram & 7)), 0, "trigram {trigram} is indexed");
    }
    assert!(bigrams.iter().any(|byte| *byte != 0), "a pair with a symbol is indexed too");
  }

  #[test]
  fn the_content_index_holds_symbol_bigrams_across_chunk_boundaries() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("chat.jsonl");
    // A chunk boundary between the '9' and the '-' leaves the "9-" bigram to the window's carry.
    let mut body = vec![b'x'; super::SEARCH_CHUNK_SIZE - 1];
    body.extend_from_slice(b"9-needle");
    fs::write(&path, &body).unwrap();
    let (_, bigrams, _, found) = super::index_and_search_file(&path, "9-needle").unwrap();
    assert!(found, "the needle search reads the text with ASCII case folding");
    for bigram in super::needle_bigrams("9-needle") {
      assert_ne!(bigrams[bigram >> 3] & (1u8 << (bigram & 7)), 0, "bigram {bigram} spans the boundary");
    }
  }

  #[test]
  fn the_content_index_holds_high_byte_bigrams_across_chunk_boundaries() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("chat.jsonl");
    // A chunk boundary inside "缓存" leaves its high-byte pairs to the window's carry.
    let mut body = vec![b'x'; super::SEARCH_CHUNK_SIZE - 2];
    body.extend_from_slice("缓存".as_bytes());
    fs::write(&path, &body).unwrap();
    let (_, _, high_bigrams, found) = super::index_and_search_file(&path, "缓存").unwrap();
    assert!(found, "a multibyte needle is found in the text's bytes");
    for bigram in super::needle_high_bigrams("缓存") {
      assert_ne!(high_bigrams[bigram >> 3] & (1u8 << (bigram & 7)), 0, "high-byte bigram {bigram} spans the boundary");
    }
  }

  #[test]
  fn the_content_index_lacks_pairs_for_an_absent_needle() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("chat.jsonl");
    let mut body = vec![b'x'; super::SEARCH_CHUNK_SIZE - 2];
    body.extend_from_slice("缓存".as_bytes());
    fs::write(&path, &body).unwrap();
    let (trigrams, bigrams, _, found) = super::index_and_search_file(&path, "walq1rus").unwrap();
    assert!(!found);
    assert!(
      super::needle_trigrams("walq1rus").iter().any(|trigram| trigrams[trigram >> 3] & (1u8 << (trigram & 7)) == 0),
      "the index lacks a trigram of the absent needle"
    );
    assert!(
      super::needle_bigrams("walq1rus").iter().any(|bigram| bigrams[bigram >> 3] & (1u8 << (bigram & 7)) == 0),
      "the index lacks a bigram of the absent needle"
    );
  }

  /// A multibyte needle never reaches a file whose high-byte pairs the index lacks: the trigram and bigram indexes
  /// cannot encode it, and the high-byte index answers it.
  #[test]
  fn a_fresh_query_skips_a_file_whose_content_index_lacks_one_of_its_high_byte_bigrams() {
    let home = tempfile::tempdir().unwrap();
    let store = test_store(home.path());
    write_session(&store, "content", "content session", SessionStatus::Active, "2026-08-01T12:00:00Z");
    let path = store.chat_events_path("content");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "the quick brown fox\n").unwrap();
    let memos = Mutex::new(super::SearchMemos::default());

    assert!(super::search_sessions_with_memos(&store, "缓存", &memos).unwrap().rows.is_empty());
    assert_eq!(memos.lock().unwrap().scan_calls, 1, "the first query reads the file and keeps its index");

    assert!(super::search_sessions_with_memos(&store, "量子", &memos).unwrap().rows.is_empty());
    assert_eq!(memos.lock().unwrap().scan_calls, 1, "the high-byte index ruled the needle out");

    fs::write(&path, "the quick brown fox 量子\n").unwrap();
    let rows = super::search_sessions_with_memos(&store, "量子", &memos).unwrap().rows;
    assert_eq!(rows.iter().map(|meta| meta.id.as_str()).collect::<Vec<_>>(), ["content"]);
  }

  /// A needle Unicode lowering composes out of a cased character keeps the scan: U+0130 lowers to "i" and a
  /// combining dot, bytes an index built from the file's own U+0130 never saw.
  #[test]
  fn a_needle_folded_out_of_a_cased_character_reaches_the_file() {
    let home = tempfile::tempdir().unwrap();
    let store = test_store(home.path());
    write_session(&store, "content", "content session", SessionStatus::Active, "2026-08-01T12:00:00Z");
    let path = store.chat_events_path("content");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "İstanbul\n").unwrap();
    let memos = Mutex::new(super::SearchMemos::default());

    assert!(super::search_sessions_with_memos(&store, "缓存", &memos).unwrap().rows.is_empty());
    let rows = super::search_sessions_with_memos(&store, "i\u{307}stanbul", &memos).unwrap().rows;
    assert_eq!(rows.iter().map(|meta| meta.id.as_str()).collect::<Vec<_>>(), ["content"]);
  }

  /// A needle whose only absent sequence is a bigram never reaches the file: the trigram index shares no evidence
  /// about a digit, and the bigram index answers it.
  #[test]
  fn a_fresh_query_skips_a_file_whose_content_index_lacks_one_of_its_bigrams() {
    let home = tempfile::tempdir().unwrap();
    let store = test_store(home.path());
    write_session(&store, "content", "content session", SessionStatus::Active, "2026-08-01T12:00:00Z");
    let path = store.chat_events_path("content");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "the quick brown fox\n").unwrap();
    let memos = Mutex::new(super::SearchMemos::default());

    assert!(super::search_sessions_with_memos(&store, "walrus", &memos).unwrap().rows.is_empty());
    assert_eq!(memos.lock().unwrap().scan_calls, 1, "the first query reads the file and keeps its index");

    // Every trigram of "qu1ck" occurs in the file; its "1c" bigram does not, and the query answers without reading it.
    assert!(super::search_sessions_with_memos(&store, "qu1ck", &memos).unwrap().rows.is_empty());
    assert_eq!(memos.lock().unwrap().scan_calls, 1, "a missing bigram ruled the file out");

    let rows = super::search_sessions_with_memos(&store, "brown f", &memos).unwrap().rows;
    assert_eq!(rows.iter().map(|meta| meta.id.as_str()).collect::<Vec<_>>(), ["content"]);
    assert_eq!(memos.lock().unwrap().scan_calls, 2, "the index lets through a needle the file holds");
  }

  /// A full result re-checks only the candidates at or above its last row: a new match in an older session cannot
  /// enter the capped result, while a new match in a newer candidate does.
  #[test]
  fn a_full_result_watches_only_candidates_at_or_above_its_last_row() {
    let home = tempfile::tempdir().unwrap();
    let store = test_store(home.path());
    let memos = Mutex::new(super::SearchMemos::default());
    let write_content = |id: &str, content: &str| {
      let path = store.chat_events_path(id);
      fs::create_dir_all(path.parent().unwrap()).unwrap();
      fs::write(&path, content).unwrap();
    };
    for index in 0..2 {
      let id = format!("hit-{index}");
      write_session(&store, &id, &format!("hit {index}"), SessionStatus::Active, "2026-08-02T12:00:00Z");
      write_content(&id, "{\"content\":\"walrus in the events\"}\n");
    }
    write_session(&store, "old", "old session", SessionStatus::Active, "2026-08-01T12:00:00Z");
    write_session(&store, "newer", "newer session", SessionStatus::Active, "2026-08-03T12:00:00Z");

    let rows = super::search_sessions_with_limit(&store, "walrus", &memos, 2).unwrap().rows;
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|meta| meta.id.starts_with("hit-")));

    write_content("old", "{\"content\":\"walrus in the events\"}\n");
    let rows = super::search_sessions_with_limit(&store, "walrus", &memos, 2).unwrap().rows;
    assert_eq!(rows.len(), 2);
    assert!(!rows.iter().any(|meta| meta.id == "old"), "a match older than the last row stays out of the full result");

    write_content("newer", "{\"content\":\"walrus in the events\"}\n");
    let rows = super::search_sessions_with_limit(&store, "walrus", &memos, 2).unwrap().rows;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].id, "newer", "a new match in a watched candidate enters the result");
    assert!(!rows.iter().any(|meta| meta.id == "old"));
  }
}
