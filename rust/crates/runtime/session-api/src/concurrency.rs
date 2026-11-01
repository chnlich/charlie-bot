//! `GET /api/concurrency/now` and `GET /api/concurrency/history`: one owner for how many agents run at once.
//!
//! An agent runs when its session's own sidebar row paints the spinner: the busy table holds an entry for it, or it
//! has its own started, unended run. Both inputs are readable per session, so the live count walks the same listing
//! and activity path the sidebar does, over the rows the sidebar's Workspace and Threads tabs list. The busy table
//! never reaches disk, so the history reads the run records instead: one run's `[started_at, ended_at or now)` is a
//! span its session ran in, and a point's value is the most sessions open at once inside its bucket.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::SystemTime;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use charliebot_compat::responses;
use charliebot_compat::routing::get_only;
use charliebot_registry::{FeatureContext, Registry};
use charliebot_store::metadata::{SessionStatus, TaskProfile, UtcDateTime};
use charliebot_store::run_records::{RunKind, RunRecord};
use charliebot_store::{SharedSubtreeRoots, Store, StoreError, StoreResult};
use serde_json::{Value, json};
use time::{Duration, OffsetDateTime, UtcOffset};

use crate::route_support::{Query, blocking_store, store_error};
use crate::sidebar;

const NOW_PATH: &str = "/api/concurrency/now";
const HISTORY_PATH: &str = "/api/concurrency/history";

/// The history range names and the seconds one point of each covers.
const BUCKETS: [(&str, i64); 3] = [("24h", 60), ("7d", 300), ("all", 3600)];

/// The range a request that names none reads.
const DEFAULT_RANGE: &str = "7d";

/// The seconds one lookback range covers: `None` reads every record.
fn lookback_seconds(range: &str) -> Option<i64> {
  match range {
    "24h" => Some(24 * 60 * 60),
    "7d" => Some(7 * 24 * 60 * 60),
    _ => None,
  }
}

fn bucket_seconds(range: &str) -> i64 {
  BUCKETS.iter().find(|(name, _)| *name == range).map(|(_, seconds)| *seconds).expect("the range is one of the three")
}

/// The paths the trailing-slash redirect claims.
pub(crate) const ROUTE_PATTERNS: &[&str] = &[NOW_PATH, HISTORY_PATH];

/// The two read-only concurrency routes, with the shared feature context as their state.
pub(crate) fn routes(features: &Arc<FeatureContext>) -> Router {
  Router::new().route(NOW_PATH, get_only(now)).route(HISTORY_PATH, get_only(history)).with_state(Arc::clone(features))
}

/// How many agents run right now, and the manager/worker split of that number.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct LiveCount {
  pub(crate) running: u32,
  pub(crate) manager: u32,
  pub(crate) worker: u32,
}

impl LiveCount {
  pub(crate) fn to_json(self, at: UtcDateTime) -> Value {
    json!({"at": at.to_str(), "running": self.running, "manager": self.manager, "worker": self.worker})
  }
}

/// The live count one request derived, with the inputs whose identities still name it.
///
/// The corpus generation names the active metadata rows the listing returns, the derived archive set and the two
/// subtree-root maps name the filters the listed rows pass, and the sidebar generation names every overlay and busy
/// mark the count reads. The store and registry are held weakly: a dropped store's entry never answers another
/// store's request.
struct LiveCountMemo {
  store: Weak<Store>,
  registry: Weak<Registry>,
  generation: u64,
  archived: Arc<Vec<String>>,
  sequence_roots: Arc<HashMap<String, String>>,
  view_roots: Arc<HashMap<String, HashMap<String, String>>>,
  sidebar_generation: u64,
  count: LiveCount,
}

/// The live count the last request derived, kept while the inputs it read still hold.
///
/// Deriving the count lists every active row, filters it through the subtree maps, resolves every row's overlay and
/// walks the busy table. The badge polls this route every five seconds from every open page, so a repeat poll reads
/// the identities that would move if any input changed and serves the count it already derived.
static LIVE_COUNT_MEMO: LazyLock<Mutex<Option<LiveCountMemo>>> = LazyLock::new(|| Mutex::new(None));

/// Count the sessions whose own row paints the spinner.
///
/// The rows are the ones the sidebar lists: the active sessions no sequence owns, less the ones a sidebar view owns
/// other than the one whose descendants fill the Threads tab. A row counts when the busy table holds it or when its
/// own run is running; the split reads the session's profile.
///
/// A repeat poll serves the count the last request derived while the corpus, the two subtree-root maps, the derived
/// archive set and the sidebar state still hold. A busy interval opening or closing, a probe storing a snapshot and a
/// changed corpus each move the sidebar generation, and a metadata write moves the corpus generation.
pub(crate) fn live_count(store: &Arc<Store>, registry: &Arc<Registry>) -> StoreResult<LiveCount> {
  let (_, generation) = store.load_session_metas_snapshot_versioned(Some(SessionStatus::Active))?;
  let archived = store.derived_archived_ids()?;
  let roots = store.subtree_roots_shared()?;
  let sidebar_generation = charliebot_store::sidebar_state::derived_generation();
  if let Some(count) = memoized_live_count(store, registry, generation, &archived, &roots, sidebar_generation) {
    return Ok(count);
  }
  let mut listed = store.list_session_rows(Some(SessionStatus::Active), None, Some(false))?;
  listed.retain(|row| listed_in_a_tab(&roots, &row.id));
  let derived = sidebar::resolve_sidebar_state(store, registry, &listed, true, false, false, false)?;
  let mut count = LiveCount::default();
  for row in &listed {
    let has_running_tasks =
      derived.get(&row.id).and_then(|entry| entry.get("has_running_tasks")).and_then(Value::as_bool).unwrap_or(false);
    if !has_running_tasks && store.busy_since(&row.id).is_none() {
      continue;
    }
    count.running += 1;
    match row.profile {
      TaskProfile::Manager => count.manager += 1,
      TaskProfile::Worker => count.worker += 1,
    }
  }
  // Resolving the overlays probes the sessions that moved, and one probe bumps the sidebar generation, so a
  // derivation that moved one stores nothing: its overlays were read under the older generation, and the next poll
  // rebuilds them under the newer one and stores them.
  if charliebot_store::sidebar_state::derived_generation() == sidebar_generation {
    *LIVE_COUNT_MEMO.lock().expect("the live count memo mutex is not poisoned") = Some(LiveCountMemo {
      store: Arc::downgrade(store),
      registry: Arc::downgrade(registry),
      generation,
      archived,
      sequence_roots: Arc::clone(&roots.sequence),
      view_roots: Arc::clone(&roots.views),
      sidebar_generation,
      count,
    });
  }
  Ok(count)
}

/// The count the last request derived while every input it read still holds.
fn memoized_live_count(
  store: &Arc<Store>,
  registry: &Arc<Registry>,
  generation: u64,
  archived: &Arc<Vec<String>>,
  roots: &SharedSubtreeRoots,
  sidebar_generation: u64,
) -> Option<LiveCount> {
  let memo = LIVE_COUNT_MEMO.lock().expect("the live count memo mutex is not poisoned");
  memo
    .as_ref()
    .filter(|entry| {
      Weak::ptr_eq(&entry.store, &Arc::downgrade(store))
        && Weak::ptr_eq(&entry.registry, &Arc::downgrade(registry))
        && entry.generation == generation
        // The same index answers the common request, so the shared list's pointer is the whole comparison. A rebuilt
        // index holds an equal list in a new allocation, and the value comparison keeps the count it answered for.
        && (Arc::ptr_eq(&entry.archived, archived) || entry.archived.as_slice() == archived.as_slice())
        && Arc::ptr_eq(&entry.sequence_roots, &roots.sequence)
        && Arc::ptr_eq(&entry.view_roots, &roots.views)
        && entry.sidebar_generation == sidebar_generation
    })
    .map(|entry| entry.count)
}

/// Whether one active row appears in a sidebar tab: the Workspace tab lists the rows outside every owned subtree, and
/// a view's tab (the Threads tab for chat threads) lists that view's rows and their descendants.
fn listed_in_a_tab(roots: &SharedSubtreeRoots, session_id: &str) -> bool {
  roots.views.values().any(|members| members.contains_key(session_id)) || !roots.sequence.contains_key(session_id)
}

/// One point of the series: the most sessions running at once inside `[at, at + bucket)`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Point {
  pub(crate) at: OffsetDateTime,
  pub(crate) running: u32,
  pub(crate) manager: u32,
  pub(crate) worker: u32,
}

/// The point that reaches the range's maximum, and the first instant that maximum held.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Peak {
  pub(crate) at: OffsetDateTime,
  pub(crate) running: u32,
  pub(crate) manager: u32,
  pub(crate) worker: u32,
}

/// One range's series.
pub(crate) struct History {
  pub(crate) range: &'static str,
  pub(crate) from: OffsetDateTime,
  pub(crate) to: OffsetDateTime,
  pub(crate) bucket_seconds: i64,
  pub(crate) first_record_at: Option<OffsetDateTime>,
  pub(crate) peak: Option<Peak>,
  pub(crate) points: Vec<Point>,
}

/// One session's run span, and whether a manager turn covered it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Span {
  session: usize,
  start: OffsetDateTime,
  end: OffsetDateTime,
  manager: bool,
}

/// The longest stamp text: `YYYY-MM-DDTHH:MM:SS.ffffffZ`.
const STAMP_CHARS: usize = 27;

/// One point's bytes at their longest: `{"t":"...","running":N,"manager":N,"worker":N},`.
const INSTANT_BYTES: usize = 104;

/// The body's fixed fields: the range, the two range ends, the bucket width, the first record and the peak.
const HISTORY_BODY_HEADROOM: usize = 192;

/// Write `at` as `UtcDateTime::to_str` writes it, from `length`: `YYYY-MM-DDTHH:MM:SS[.ffffff]Z`.
///
/// Every instant the route carries holds a four-digit year: the store parses a timestamp as RFC 3339, whose year is
/// four digits, and a point is the clock less a lookback.
fn write_stamp(text: &mut [u8], length: &mut usize, at: OffsetDateTime) {
  let at = if at.offset() == UtcOffset::UTC { at } else { at.to_offset(UtcOffset::UTC) };
  let year = at.year() as u32;
  write_pair(text, length, year / 100);
  write_pair(text, length, year % 100);
  text[*length] = b'-';
  *length += 1;
  write_pair(text, length, u8::from(at.month()) as u32);
  text[*length] = b'-';
  *length += 1;
  write_pair(text, length, at.day() as u32);
  text[*length] = b'T';
  *length += 1;
  write_pair(text, length, at.hour() as u32);
  text[*length] = b':';
  *length += 1;
  write_pair(text, length, at.minute() as u32);
  text[*length] = b':';
  *length += 1;
  write_pair(text, length, at.second() as u32);
  let microsecond = at.microsecond();
  if microsecond != 0 {
    text[*length] = b'.';
    *length += 1;
    write_pair(text, length, microsecond / 10_000);
    write_pair(text, length, (microsecond / 100) % 100);
    write_pair(text, length, microsecond % 100);
  }
  text[*length] = b'Z';
  *length += 1;
}

/// `at`'s stamp text, as the serializer's reference form needs it.
#[cfg(test)]
fn stamp_text(text: &mut [u8; STAMP_CHARS], at: OffsetDateTime) -> &str {
  let mut length = 0;
  write_stamp(text, &mut length, at);
  std::str::from_utf8(&text[..length]).expect("the stamp's bytes are ASCII digits and separators")
}

/// The two digits of every value 0-99, so a zero-padded field is one lookup.
const TWO_DIGITS: [[u8; 2]; 100] = {
  let mut table = [[b'0'; 2]; 100];
  let mut value = 0;
  while value < 100 {
    table[value][0] = b'0' + (value / 10) as u8;
    table[value][1] = b'0' + (value % 10) as u8;
    value += 1;
  }
  table
};

/// Write the two digits of `value` (`0`-`99`) to `text` at `length`.
fn write_pair(text: &mut [u8], length: &mut usize, value: u32) {
  let pair = TWO_DIGITS[(value % 100) as usize];
  text[*length] = pair[0];
  text[*length + 1] = pair[1];
  *length += 2;
}

/// Write `source` into `text` at `length`.
fn write_bytes(text: &mut [u8], length: &mut usize, source: &[u8]) {
  let end = *length + source.len();
  text[*length..end].copy_from_slice(source);
  *length = end;
}

impl History {
  /// Write the response body in the field order and text clients parse.
  ///
  /// The generic serializer's per-entry dispatch and its per-field calls are the route's largest cost on a
  /// 2,016-point series, so the fixed field names and the digits go straight into one buffer.
  fn into_body(self) -> Vec<u8> {
    let mut body = Vec::with_capacity(HISTORY_BODY_HEADROOM + self.points.len() * INSTANT_BYTES);
    body.extend_from_slice(b"{\"range\":\"");
    body.extend_from_slice(self.range.as_bytes());
    body.extend_from_slice(b"\",\"from\":\"");
    push_stamp(&mut body, self.from);
    body.extend_from_slice(b"\",\"to\":\"");
    push_stamp(&mut body, self.to);
    body.extend_from_slice(b"\",\"bucket_seconds\":");
    push_decimal(&mut body, self.bucket_seconds);
    body.extend_from_slice(b",\"first_record_at\":");
    match self.first_record_at {
      Some(at) => {
        body.push(b'"');
        push_stamp(&mut body, at);
        body.push(b'"');
      }
      None => body.extend_from_slice(b"null"),
    }
    body.extend_from_slice(b",\"peak\":");
    match self.peak {
      Some(peak) => push_instant(&mut body, peak.at, peak.running, peak.manager, peak.worker),
      None => body.extend_from_slice(b"null"),
    }
    body.extend_from_slice(b",\"points\":[");
    for (index, point) in self.points.iter().enumerate() {
      if index > 0 {
        body.push(b',');
      }
      push_instant(&mut body, point.at, point.running, point.manager, point.worker);
    }
    body.extend_from_slice(b"]}");
    body
  }
}

/// Append one chart point or peak as `{"t":"<stamp>","running":N,"manager":N,"worker":N}`.
///
/// The four fields go into one stack buffer first: appending each constant to the body separately costs a capacity
/// check and a small copy per field, and a point carries four.
fn push_instant(body: &mut Vec<u8>, at: OffsetDateTime, running: u32, manager: u32, worker: u32) {
  let mut instant = [0u8; INSTANT_BYTES];
  let mut length = 0;
  write_bytes(&mut instant, &mut length, b"{\"t\":\"");
  write_stamp(&mut instant, &mut length, at);
  write_bytes(&mut instant, &mut length, b"\",\"running\":");
  write_decimal(&mut instant, &mut length, i64::from(running));
  write_bytes(&mut instant, &mut length, b",\"manager\":");
  write_decimal(&mut instant, &mut length, i64::from(manager));
  write_bytes(&mut instant, &mut length, b",\"worker\":");
  write_decimal(&mut instant, &mut length, i64::from(worker));
  write_bytes(&mut instant, &mut length, b"}");
  body.extend_from_slice(&instant[..length]);
}

/// Append `at`'s stamp text.
fn push_stamp(body: &mut Vec<u8>, at: OffsetDateTime) {
  let mut text = [0u8; STAMP_CHARS];
  let mut length = 0;
  write_stamp(&mut text, &mut length, at);
  body.extend_from_slice(&text[..length]);
}

/// Append `value` in decimal, as serde_json writes an integer.
fn push_decimal(body: &mut Vec<u8>, value: i64) {
  let mut text = [0u8; DECIMAL_CHARS];
  let mut length = 0;
  write_decimal(&mut text, &mut length, value);
  body.extend_from_slice(&text[..length]);
}

/// The decimal text's longest form: `-9223372036854775808`.
const DECIMAL_CHARS: usize = 20;

/// Write `value` in decimal at `text`'s `length`, with the sign a negative value needs.
fn write_decimal(text: &mut [u8], length: &mut usize, value: i64) {
  let mut digits = [0u8; DECIMAL_CHARS];
  let mut index = digits.len();
  let mut rest = value.unsigned_abs();
  loop {
    index -= 1;
    digits[index] = b'0' + (rest % 10) as u8;
    rest /= 10;
    if rest == 0 {
      break;
    }
  }
  if value < 0 {
    index -= 1;
    digits[index] = b'-';
  }
  write_bytes(text, length, &digits[index..]);
}

/// The run-record scan the last request made, with the inputs that key it.
///
/// A committed run-record write moves the store's records generation, and a session directory added or removed moves
/// the sessions directory's mtime. A request whose two inputs still hold takes the scan the earlier request made: the
/// walk over the home's sessions directories is the route's largest cost. The clock and the range stay outside the
/// key, because every request applies them to the records afresh.
struct RecordsMemo {
  sessions_dir: PathBuf,
  sessions_mtime: Option<SystemTime>,
  records_generation: u64,
  records: Arc<Vec<(String, RunRecord)>>,
}

static RECORDS_MEMO: LazyLock<Mutex<Option<RecordsMemo>>> = LazyLock::new(|| Mutex::new(None));

/// Every session's run records, from the scan this process last made when its key still holds.
fn all_run_records(store: &Store) -> StoreResult<Arc<Vec<(String, RunRecord)>>> {
  let mut memo = RECORDS_MEMO.lock().expect("the run-record memo mutex is not poisoned");
  scan_records(&mut memo, store)
}

fn scan_records(memo: &mut Option<RecordsMemo>, store: &Store) -> StoreResult<Arc<Vec<(String, RunRecord)>>> {
  let sessions_dir = store.config().sessions_dir();
  let sessions_mtime = sessions_directory_mtime(&sessions_dir)?;
  // The generation is read before the scan, so a write that lands during it leaves a stale key behind.
  let records_generation = store.records_generation();
  let holds = memo.as_ref().is_some_and(|entry| {
    entry.records_generation == records_generation
      && entry.sessions_mtime == sessions_mtime
      && entry.sessions_dir == sessions_dir
  });
  if holds {
    return Ok(Arc::clone(&memo.as_ref().expect("the memo holds a scan").records));
  }
  let records = Arc::new(store.list_all_run_records_sync()?);
  *memo = Some(RecordsMemo { sessions_dir, sessions_mtime, records_generation, records: Arc::clone(&records) });
  Ok(records)
}

/// The sessions directory's mtime, or `None` when it does not exist.
///
/// The run-record scan reads a missing sessions directory as an empty home, so the key records that state as `None`.
fn sessions_directory_mtime(sessions_dir: &Path) -> StoreResult<Option<SystemTime>> {
  match fs::metadata(sessions_dir) {
    Ok(metadata) => {
      let modified =
        metadata.modified().map_err(|error| StoreError::new(format!("{}: {error}", sessions_dir.display())))?;
      Ok(Some(modified))
    }
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
    Err(error) => Err(StoreError::new(format!("{}: {error}", sessions_dir.display()))),
  }
}

/// The series of one range, read from every run record.
pub(crate) fn history_series(store: &Store, range: &'static str, now: UtcDateTime) -> StoreResult<History> {
  let records = all_run_records(store)?;
  let now = now.into_inner();
  let first_record_at = records.iter().filter_map(|(_, record)| record.started_at).min().map(UtcDateTime::into_inner);
  let from =
    lookback_seconds(range).map_or_else(|| first_record_at.unwrap_or(now), |seconds| now - Duration::seconds(seconds));
  let buckets = bucket_peaks(&merged_spans(records.as_slice(), now), from, now, bucket_seconds(range));
  let mut peak: Option<Peak> = None;
  for bucket in &buckets {
    if peak.is_none_or(|best| bucket.point.running > best.running) {
      peak = Some(Peak {
        at: bucket.at_max,
        running: bucket.point.running,
        manager: bucket.point.manager,
        worker: bucket.point.worker,
      });
    }
  }
  Ok(History {
    range,
    from,
    to: now,
    bucket_seconds: bucket_seconds(range),
    first_record_at,
    peak: peak.filter(|peak| peak.running > 0),
    points: buckets.into_iter().map(|bucket| bucket.point).collect(),
  })
}

/// One merged span per session and channel: a session with several overlapping runs covers its span once.
///
/// A run without `started_at` never ran and contributes nothing; an unended run runs up to `now`.
fn merged_spans(records: &[(String, RunRecord)], now: OffsetDateTime) -> Vec<Span> {
  let mut indexes: HashMap<&str, usize> = HashMap::new();
  let mut spans = Vec::new();
  for (session, record) in records {
    let Some(start) = record.started_at else { continue };
    let end = record.ended_at.map_or(now, UtcDateTime::into_inner);
    let start = start.into_inner();
    if end <= start {
      continue;
    }
    let next_index = indexes.len();
    let index = *indexes.entry(session.as_str()).or_insert(next_index);
    spans.push(Span { session: index, start, end, manager: record.kind == RunKind::ManagerTurn });
  }
  spans.sort_by_key(|span| (span.session, span.manager, span.start));
  let mut merged: Vec<Span> = Vec::with_capacity(spans.len());
  for span in spans {
    match merged.last_mut() {
      Some(last) if last.session == span.session && last.manager == span.manager && span.start <= last.end => {
        last.end = last.end.max(span.end);
      }
      _ => merged.push(span),
    }
  }
  merged
}

/// One bucket's point and the first instant inside it that reaches the point's value.
struct Bucket {
  point: Point,
  at_max: OffsetDateTime,
}

/// Sweep the spans once and record each bucket's maximum and the instant it first held.
///
/// Events at one instant apply ends before starts, as Python's `concurrency.py` orders them. A session counts once
/// however many of its runs are open, so the sweep counts sessions with any open run and sessions with an open
/// manager turn, and a point's workers are the difference.
fn bucket_peaks(spans: &[Span], from: OffsetDateTime, to: OffsetDateTime, bucket_seconds: i64) -> Vec<Bucket> {
  let step = Duration::seconds(bucket_seconds);
  let span_nanos = (to - from).whole_nanoseconds();
  if span_nanos <= 0 {
    return Vec::new();
  }
  let count = ((span_nanos + step.whole_nanoseconds() - 1) / step.whole_nanoseconds()) as usize;
  let mut events: Vec<(OffsetDateTime, bool, usize, bool)> = Vec::new();
  for span in spans {
    let start = span.start.max(from);
    let end = span.end.min(to);
    if end <= start {
      continue;
    }
    events.push((start, true, span.session, span.manager));
    events.push((end, false, span.session, span.manager));
  }
  events.sort_by_key(|event| (event.0, event.1));
  let mut open_any: HashMap<usize, i32> = HashMap::new();
  let mut open_manager: HashMap<usize, i32> = HashMap::new();
  let mut running = 0u32;
  let mut manager = 0u32;
  let mut next = 0usize;
  let mut buckets = Vec::with_capacity(count);
  let mut start = from;
  for _ in 0..count {
    let end = (start + step).min(to);
    // The carried state is a candidate at the bucket's start only when no event sits exactly there.
    let mut best = (events.get(next).map(|event| event.0) != Some(start)).then_some((running, manager, start));
    while let Some(&(at, opens, session, is_manager)) = events.get(next) {
      if at >= end {
        break;
      }
      next += 1;
      let delta = if opens { 1 } else { -1 };
      let any = open_any.entry(session).or_insert(0);
      let was_open = *any > 0;
      *any += delta;
      if !was_open && *any > 0 {
        running += 1;
      } else if was_open && *any == 0 {
        running -= 1;
      }
      if is_manager {
        let turns = open_manager.entry(session).or_insert(0);
        let was_open = *turns > 0;
        *turns += delta;
        if !was_open && *turns > 0 {
          manager += 1;
        } else if was_open && *turns == 0 {
          manager -= 1;
        }
      }
      // Events at one instant apply together, so only the state after the last of them held during the bucket.
      let instant_done = events.get(next).is_none_or(|event| event.0 != at);
      if instant_done && best.is_none_or(|(peak, _, _)| running > peak) {
        best = Some((running, manager, at));
      }
    }
    let (peak_running, peak_manager, at_max) = best.expect("a bucket always carries a candidate");
    buckets.push(Bucket {
      point: Point { at: start, running: peak_running, manager: peak_manager, worker: peak_running - peak_manager },
      at_max,
    });
    // Every bucket but the last carries its full step, so the next start is this bucket's end.
    start = end;
  }
  buckets
}

async fn now(State(features): State<Arc<FeatureContext>>) -> Response {
  let store = Arc::clone(&features.store);
  let registry = Arc::clone(&features.registry);
  match blocking_store(move || Ok(live_count(&store, &registry)?.to_json(UtcDateTime::now()))).await {
    Ok(body) => responses::fast_json(StatusCode::OK, &body),
    Err(error) => store_error(error),
  }
}

/// The requested range, or the 400 whose body lists the valid values.
fn requested_range(request: &Request<Body>) -> Result<&'static str, Box<Response>> {
  let query = Query::new(request);
  let name = query.get("range").unwrap_or(DEFAULT_RANGE);
  BUCKETS.iter().find(|(candidate, _)| *candidate == name).map(|(name, _)| *name).ok_or_else(|| {
    let valid = BUCKETS.iter().map(|(name, _)| *name).collect::<Vec<_>>().join(", ");
    Box::new(responses::json(StatusCode::BAD_REQUEST, &json!({"detail": format!("range must be one of: {valid}")})))
  })
}

async fn history(State(features): State<Arc<FeatureContext>>, request: Request<Body>) -> Response {
  let range = match requested_range(&request) {
    Ok(range) => range,
    Err(response) => return *response,
  };
  let store = Arc::clone(&features.store);
  match blocking_store(move || history_series(&store, range, UtcDateTime::now()).map(History::into_body)).await {
    Ok(body) => Response::builder()
      .status(StatusCode::OK)
      .header(header::CONTENT_TYPE, "application/json")
      .header(header::CONTENT_LENGTH, body.len())
      .body(Body::from(body))
      .expect("the history response builds"),
    Err(error) => store_error(error),
  }
}

#[cfg(test)]
mod tests {
  use std::fs;

  use axum::http::Request;
  use charliebot_core::proc_identity;
  use charliebot_store::metadata::SessionMetadata;

  use super::*;
  use crate::test_support;

  fn at(text: &str) -> OffsetDateTime {
    OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339).expect("a test instant")
  }

  /// The serializer's rendering, the byte contract the direct writer meets.
  #[derive(serde::Serialize)]
  struct ReferenceInstant {
    t: ReferenceStamp,
    running: u32,
    manager: u32,
    worker: u32,
  }

  impl ReferenceInstant {
    fn of(at: OffsetDateTime, running: u32, manager: u32, worker: u32) -> Self {
      Self { t: ReferenceStamp(at), running, manager, worker }
    }
  }

  #[derive(Clone, Copy)]
  struct ReferenceStamp(OffsetDateTime);

  impl serde::Serialize for ReferenceStamp {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
      let mut text = [0u8; STAMP_CHARS];
      serializer.serialize_str(stamp_text(&mut text, self.0))
    }
  }

  #[derive(serde::Serialize)]
  struct ReferenceBody {
    range: &'static str,
    from: ReferenceStamp,
    to: ReferenceStamp,
    bucket_seconds: i64,
    first_record_at: Option<ReferenceStamp>,
    peak: Option<ReferenceInstant>,
    points: Vec<ReferenceInstant>,
  }

  fn reference_body(history: &History) -> Vec<u8> {
    let body = ReferenceBody {
      range: history.range,
      from: ReferenceStamp(history.from),
      to: ReferenceStamp(history.to),
      bucket_seconds: history.bucket_seconds,
      first_record_at: history.first_record_at.map(ReferenceStamp),
      peak: history.peak.map(|peak| ReferenceInstant::of(peak.at, peak.running, peak.manager, peak.worker)),
      points: history
        .points
        .iter()
        .map(|point| ReferenceInstant::of(point.at, point.running, point.manager, point.worker))
        .collect(),
    };
    serde_json::to_vec(&body).expect("a reference history body serializes")
  }

  /// `write_decimal` renders the whole i64 domain, including the magnitude a positive i64 cannot hold.
  #[test]
  fn write_decimal_renders_the_signed_domain() {
    for value in [0, 7, -7, 300, i64::MAX, i64::MIN] {
      let mut text = [0u8; DECIMAL_CHARS];
      let mut length = 0;
      write_decimal(&mut text, &mut length, value);
      assert_eq!(std::str::from_utf8(&text[..length]).expect("ASCII digits"), value.to_string());
    }
  }

  /// The direct writer keeps the serializer's bytes, with and without a peak and a first record.
  #[test]
  fn the_history_body_keeps_the_serializers_bytes() {
    let point = |minutes: i64, running: u32, manager: u32| Point {
      at: at("2026-11-01T12:00:00Z") + Duration::minutes(minutes),
      running,
      manager,
      worker: running - manager,
    };
    let populated = History {
      range: "7d",
      from: at("2026-10-25T12:00:00.25Z"),
      to: at("2026-11-01T12:00:00Z"),
      bucket_seconds: 300,
      first_record_at: Some(at("2026-10-26T04:07:31.5Z")),
      peak: Some(Peak { at: at("2026-10-30T08:15:00Z"), running: 3, manager: 1, worker: 2 }),
      points: vec![point(0, 0, 0), point(5, 1, 1), point(10, 3, 1), point(15, u32::MAX, 0)],
    };
    let empty = History {
      range: "24h",
      from: at("2026-10-31T12:00:00Z"),
      to: at("2026-11-01T12:00:00Z"),
      bucket_seconds: 60,
      first_record_at: None,
      peak: None,
      points: vec![point(0, 0, 0)],
    };
    for history in [populated, empty] {
      let expected = reference_body(&history);
      assert_eq!(history.into_body(), expected, "the direct writer keeps the serializer's bytes");
    }
  }

  fn record(session: &str, kind: &str, start: Option<&str>, end: Option<&str>) -> (String, RunRecord) {
    let value = json!({
      "id": format!("{session}-{kind}-{start:?}"),
      "session_id": session,
      "kind": kind,
      "started_at": start,
      "ended_at": end,
    });
    (session.to_string(), serde_json::from_value(value).expect("a test run record"))
  }

  /// Write one session's metadata as the store reads it.
  fn write_session(store: &Store, meta: &SessionMetadata) {
    let path = store.metadata_path(&meta.id);
    fs::create_dir_all(path.parent().expect("a metadata file has a directory")).expect("a session directory");
    fs::write(&path, meta.to_json_string(&[], true)).expect("a metadata file");
  }

  /// Write one running run record: the test process stands in for the launched run, so the liveness check passes.
  fn write_running_run(store: &Store, session_id: &str, kind: &str) {
    let pid = std::process::id();
    let pid_start = proc_identity::read_pid_stat(pid).expect("the test process has a proc stat").pid_start;
    let path = store.run_metadata_path(session_id, "synthetic-run");
    fs::create_dir_all(path.parent().expect("a run record has a directory")).expect("a run directory");
    let record = json!({
      "id": "synthetic-run",
      "session_id": session_id,
      "kind": kind,
      "pid": pid,
      "pid_start": pid_start,
      "started_at": UtcDateTime::now().to_str(),
    });
    fs::write(&path, record.to_string()).expect("a run record");
  }

  /// Overlapping runs of one session count once, and an unstarted run counts nowhere.
  #[test]
  fn overlapping_runs_of_one_session_count_once() {
    let now = at("2026-10-31T00:00:00Z");
    let records = vec![
      record("a", "work", Some("2026-10-30T23:00:00Z"), Some("2026-10-30T23:30:00Z")),
      record("a", "work", Some("2026-10-30T23:10:00Z"), Some("2026-10-30T23:40:00Z")),
      record("b", "review", Some("2026-10-30T23:20:00Z"), Some("2026-10-30T23:25:00Z")),
      record("c", "work", None, None),
    ];
    let spans = merged_spans(&records, now);
    assert_eq!(spans.len(), 2, "the two overlapping runs of session a merge into one span");
    let buckets = bucket_peaks(&spans, at("2026-10-30T23:00:00Z"), now, 3600);
    assert_eq!(buckets[0].point.running, 2, "a and b run at once; a's second run adds nothing");
    assert_eq!(buckets[0].point.worker, 2);
    assert_eq!(buckets[0].at_max, at("2026-10-30T23:20:00Z"), "the peak first holds when b starts");
    assert!(buckets.iter().all(|bucket| bucket.point.running <= 2), "an unstarted run never opens");
  }

  /// An unended run runs up to now.
  #[test]
  fn an_unended_run_counts_up_to_now() {
    let now = at("2026-10-31T00:00:00Z");
    let records = vec![record("a", "work", Some("2026-10-30T22:00:00Z"), None)];
    let buckets = bucket_peaks(&merged_spans(&records, now), at("2026-10-30T22:00:00Z"), now, 3600);
    assert_eq!(buckets.len(), 2);
    assert!(buckets.iter().all(|bucket| bucket.point.running == 1));
  }

  /// The split reads the run's kind: a manager turn is a manager, every other kind a worker.
  #[test]
  fn the_manager_worker_split_reads_the_run_kind() {
    let now = at("2026-10-31T00:00:00Z");
    let records = vec![
      record("m", "manager_turn", Some("2026-10-30T23:00:00Z"), Some("2026-10-30T23:30:00Z")),
      record("w1", "work", Some("2026-10-30T23:05:00Z"), Some("2026-10-30T23:10:00Z")),
      record("w2", "review", Some("2026-10-30T23:05:00Z"), Some("2026-10-30T23:10:00Z")),
    ];
    let buckets = bucket_peaks(&merged_spans(&records, now), at("2026-10-30T23:00:00Z"), now, 3600);
    assert_eq!((buckets[0].point.running, buckets[0].point.manager, buckets[0].point.worker), (3, 1, 2));
    assert_eq!(buckets[0].at_max, at("2026-10-30T23:05:00Z"));
    assert_eq!(buckets[0].point.manager + buckets[0].point.worker, buckets[0].point.running);
  }

  /// A bucket's value is the maximum inside it, and its split is taken at the instant that maximum first holds.
  #[test]
  fn a_bucket_reports_its_maximum_and_the_split_at_that_instant() {
    let now = at("2026-10-31T01:00:00Z");
    let records = vec![
      record("m1", "manager_turn", Some("2026-10-31T00:00:10Z"), Some("2026-10-31T00:50:00Z")),
      record("m2", "manager_turn", Some("2026-10-31T00:01:00Z"), Some("2026-10-31T00:05:00Z")),
      record("w1", "work", Some("2026-10-31T00:04:00Z"), Some("2026-10-31T00:04:30Z")),
    ];
    let buckets = bucket_peaks(&merged_spans(&records, now), at("2026-10-31T00:00:00Z"), now, 3600);
    assert_eq!((buckets[0].point.running, buckets[0].point.manager, buckets[0].point.worker), (3, 2, 1));
    assert_eq!(buckets[0].point.at, at("2026-10-31T00:00:00Z"), "a point stands at its bucket's start");
    assert_eq!(buckets[0].at_max, at("2026-10-31T00:04:00Z"), "the maximum first holds when w1 starts");
  }

  /// Runs that end at one instant leave together: a bucket starting then never reports the state between their ends.
  #[test]
  fn runs_ending_together_on_a_bucket_start_leave_together() {
    let now = at("2026-10-31T00:00:00Z");
    let records = vec![
      record("a", "work", Some("2026-10-30T22:00:00Z"), Some("2026-10-30T23:00:00Z")),
      record("b", "work", Some("2026-10-30T22:30:00Z"), Some("2026-10-30T23:00:00Z")),
    ];
    let buckets = bucket_peaks(&merged_spans(&records, now), at("2026-10-30T22:00:00Z"), now, 3600);
    assert_eq!(buckets[0].point.running, 2);
    assert_eq!(buckets[1].point.running, 0, "both runs ended at 23:00, so the bucket starting then holds none");
  }

  /// A worker session with its own running run and no busy entry counts, and so does a Threads-tab session; a
  /// sequence-owned session counts nowhere.
  #[tokio::test]
  async fn the_live_count_reads_both_inputs_over_the_listed_rows() {
    let (_home, features) = test_support::feature_context();
    let busy = test_support::create_node(&features.store, "live-busy-manager", None, TaskProfile::Manager).await;
    let worker = test_support::create_node(&features.store, "live-running-worker", None, TaskProfile::Worker).await;
    // The Slack thread session is a sidebar view's row, which the Threads tab lists and the Workspace tab hides.
    let mut thread = SessionMetadata::new("live-slack-thread", "Slack thread", TaskProfile::Worker);
    thread.extra.insert("slack_origin".to_string(), json!({"channel": "C0SYNTHETIC"}));
    write_session(&features.store, &thread);
    let mut scheduled = SessionMetadata::new("sequence-owned-scheduled", "Scheduled", TaskProfile::Manager);
    scheduled.extra.insert("scheduled_task".to_string(), json!("code-health"));
    write_session(&features.store, &scheduled);

    features.store.mark_busy(&busy.id, UtcDateTime::now()).unwrap();
    write_running_run(&features.store, &worker.id, "work");
    write_running_run(&features.store, &thread.id, "review");
    write_running_run(&features.store, &scheduled.id, "manager_turn");

    let count = live_count(&features.store, &features.registry).unwrap();
    assert_eq!(count, LiveCount { running: 3, manager: 1, worker: 2 }, "the sequence-owned session is not listed");
  }

  /// A repeat poll serves the count of the map it read, and a busy change moves the count the next poll derives.
  #[tokio::test]
  async fn a_repeat_live_count_follows_a_busy_change() {
    let (_home, features) = test_support::feature_context();
    let worker = test_support::create_node(&features.store, "memo-worker", None, TaskProfile::Worker).await;
    assert_eq!(live_count(&features.store, &features.registry).unwrap(), LiveCount::default());
    features.store.mark_busy(&worker.id, UtcDateTime::now()).unwrap();
    assert_eq!(
      live_count(&features.store, &features.registry).unwrap(),
      LiveCount { running: 1, manager: 0, worker: 1 }
    );
    features.store.clear_busy(&worker.id);
    assert_eq!(live_count(&features.store, &features.registry).unwrap(), LiveCount::default());
  }

  /// An idle sidebar counts nothing.
  #[tokio::test]
  async fn an_idle_workspace_counts_no_running_agent() {
    let (_home, features) = test_support::feature_context();
    test_support::create_node(&features.store, "idle-manager", None, TaskProfile::Manager).await;
    test_support::create_node(&features.store, "idle-worker", None, TaskProfile::Worker).await;
    assert_eq!(live_count(&features.store, &features.registry).unwrap(), LiveCount::default());
  }

  /// `/now` answers the four fields, and its counts stay split correctly.
  #[tokio::test]
  async fn the_now_route_answers_the_live_count_shape() {
    let (_home, features) = test_support::feature_context();
    let worker = test_support::create_node(&features.store, "now-worker", None, TaskProfile::Worker).await;
    write_running_run(&features.store, &worker.id, "work");

    let response = now(State(Arc::clone(&features))).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = crate::test_support::answer(response).await.1;
    assert_eq!(body["running"], 1);
    assert_eq!(body["manager"], 0);
    assert_eq!(body["worker"], 1);
    assert!(UtcDateTime::parse(body["at"].as_str().expect("at names an instant")).is_ok());
  }

  /// `/history` answers the designed shape for each range, and an unknown range is a 400 naming the valid values.
  #[tokio::test]
  async fn the_history_route_answers_each_range_and_refuses_another() {
    let (_home, features) = test_support::feature_context();
    for (range, bucket) in BUCKETS {
      let request =
        Request::builder().uri(format!("/api/concurrency/history?range={range}")).body(Body::empty()).unwrap();
      let response = history(State(Arc::clone(&features)), request).await;
      assert_eq!(response.status(), StatusCode::OK);
      let body = crate::test_support::answer(response).await.1;
      assert_eq!(body["range"], range);
      assert_eq!(body["bucket_seconds"], bucket);
      assert_eq!(body["first_record_at"], Value::Null, "an empty home holds no run record");
      assert_eq!(body["peak"], Value::Null);
      assert!(body["points"].as_array().expect("points is a list").iter().all(|point| point["running"] == 0));
    }

    let default = Request::builder().uri(HISTORY_PATH).body(Body::empty()).unwrap();
    let body = crate::test_support::answer(history(State(Arc::clone(&features)), default).await).await.1;
    assert_eq!(body["range"], "7d", "a request that names no range reads the last seven days");

    let invalid = Request::builder().uri("/api/concurrency/history?range=1h").body(Body::empty()).unwrap();
    let response = history(State(Arc::clone(&features)), invalid).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = crate::test_support::answer(response).await.1;
    let detail = body["detail"].as_str().expect("the refusal names the valid values");
    for range in ["24h", "7d", "all"] {
      assert!(detail.contains(range), "{range} is one of the listed values: {detail}");
    }
  }

  /// The route's own stamp text is the store's, fraction and all: the chart's time axis reads these bytes.
  #[test]
  fn a_stamp_writes_the_text_the_store_writes_for_an_instant() {
    for text in [
      "2026-11-01T07:00:00Z",
      "2026-11-01T07:00:00.5Z",
      "2026-02-28T23:59:59.000001Z",
      "2029-12-31T23:59:59.999999Z",
      "2030-01-01T00:00:00Z",
    ] {
      let instant = at(text);
      let mut stamp = [0u8; STAMP_CHARS];
      assert_eq!(stamp_text(&mut stamp, instant), UtcDateTime::from(instant).to_str());
    }
  }

  /// A run record written to the home reaches the series, at the instant it started.
  #[tokio::test]
  async fn the_history_reads_the_run_records_of_the_home() {
    let (_home, features) = test_support::feature_context();
    let worker = test_support::create_node(&features.store, "history-worker", None, TaskProfile::Worker).await;
    write_running_run(&features.store, &worker.id, "work");

    let series = history_series(&features.store, "24h", UtcDateTime::now()).unwrap();
    let peak = series.peak.expect("the running worker reaches a peak");
    assert_eq!((peak.running, peak.manager, peak.worker), (1, 0, 1));
    assert_eq!(series.first_record_at, Some(peak.at));
  }

  /// A repeat scan takes the records it already read until a session directory moves the key.
  #[tokio::test]
  async fn a_records_memo_reuses_its_scan_until_a_session_directory_moves() {
    let (_home, features) = test_support::feature_context();
    let mut memo = None;
    let first = scan_records(&mut memo, &features.store).unwrap();
    let repeat = scan_records(&mut memo, &features.store).unwrap();
    assert!(Arc::ptr_eq(&first, &repeat), "an unchanged home takes the scan it already made");

    test_support::create_node(&features.store, "memo-new-session", None, TaskProfile::Manager).await;
    let after = scan_records(&mut memo, &features.store).unwrap();
    assert!(!Arc::ptr_eq(&repeat, &after), "a session directory added moves the sessions directory's mtime");
  }
}
