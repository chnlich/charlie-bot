//! `GET /api/concurrency/now` and `GET /api/concurrency/history`: one owner for how many agents run at once.
//!
//! An agent runs when its session's own sidebar row paints the spinner: the busy table holds an entry for it, or it
//! has its own started, unended run. Both inputs are readable per session, so the live count walks the same listing
//! and activity path the sidebar does, over the rows the sidebar's Workspace and Threads tabs list. The busy table
//! never reaches disk, so the history reads the run records instead: one run's `[started_at, ended_at or now)` is a
//! span its session ran in, and a point's value is the most sessions open at once inside its bucket.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use charliebot_compat::responses;
use charliebot_compat::routing::get_only;
use charliebot_registry::{FeatureContext, Registry};
use charliebot_store::metadata::{SessionStatus, TaskProfile, UtcDateTime};
use charliebot_store::run_records::{RunKind, RunRecord};
use charliebot_store::{SharedSubtreeRoots, Store, StoreResult};
use serde_json::{Value, json};
use time::{Duration, OffsetDateTime};

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

/// Count the sessions whose own row paints the spinner.
///
/// The rows are the ones the sidebar lists: the active sessions no sequence owns, less the ones a sidebar view owns
/// other than the one whose descendants fill the Threads tab. A row counts when the busy table holds it or when its
/// own run is running; the split reads the session's profile.
pub(crate) fn live_count(store: &Arc<Store>, registry: &Arc<Registry>) -> StoreResult<LiveCount> {
  let rows = store.list_session_rows(Some(SessionStatus::Active), None, Some(false))?;
  let derived = sidebar::resolve_sidebar_state(store, registry, &rows, true, false, false, false)?;
  let roots = store.subtree_roots_shared()?;
  let mut count = LiveCount::default();
  for row in &rows {
    if !listed_in_a_tab(&roots, &row.id) {
      continue;
    }
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
  Ok(count)
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

fn instant_json(at: OffsetDateTime, running: u32, manager: u32, worker: u32) -> Value {
  json!({"t": UtcDateTime::from(at).to_str(), "running": running, "manager": manager, "worker": worker})
}

impl History {
  pub(crate) fn into_json(self) -> Value {
    json!({
      "range": self.range,
      "from": UtcDateTime::from(self.from).to_str(),
      "to": UtcDateTime::from(self.to).to_str(),
      "bucket_seconds": self.bucket_seconds,
      "first_record_at": self.first_record_at.map(|at| UtcDateTime::from(at).to_str()),
      "peak": self.peak.map(|peak| instant_json(peak.at, peak.running, peak.manager, peak.worker)),
      "points": self
        .points
        .iter()
        .map(|point| instant_json(point.at, point.running, point.manager, point.worker))
        .collect::<Vec<_>>(),
    })
  }
}

/// The series of one range, read from every run record.
pub(crate) fn history_series(store: &Store, range: &'static str, now: UtcDateTime) -> StoreResult<History> {
  let records = store.list_all_run_records_sync()?;
  let now = now.into_inner();
  let first_record_at = records.iter().filter_map(|(_, record)| record.started_at).min().map(UtcDateTime::into_inner);
  let from =
    lookback_seconds(range).map_or_else(|| first_record_at.unwrap_or(now), |seconds| now - Duration::seconds(seconds));
  let buckets = bucket_peaks(&merged_spans(&records, now), from, now, bucket_seconds(range));
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
  for index in 0..count {
    let start = from + Duration::seconds(bucket_seconds * index as i64);
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
      if best.is_none_or(|(peak, _, _)| running > peak) {
        best = Some((running, manager, at));
      }
    }
    let (peak_running, peak_manager, at_max) = best.expect("a bucket always carries a candidate");
    buckets.push(Bucket {
      point: Point { at: start, running: peak_running, manager: peak_manager, worker: peak_running - peak_manager },
      at_max,
    });
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
  match blocking_store(move || history_series(&store, range, UtcDateTime::now()).map(History::into_json)).await {
    Ok(body) => responses::fast_json(StatusCode::OK, &body),
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
}
