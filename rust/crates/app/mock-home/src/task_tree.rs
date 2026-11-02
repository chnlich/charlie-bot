//! The task-tree shape: a synthetic home whose task tree has the scale and proportions of a production start.
//!
//! At startup the task-tree walk (`charliebot_tasks::recovery`) reads every session. It gives a full node pass to each
//! session that one keep reason holds, and skips the rest. The walk checks the reasons in this order: an open task, a
//! chat log or metadata file written after the boot marker, a run with no end, a run with a sequence ref, and a run
//! whose worktree exists. This shape gives every session exactly one of these reasons, or none. It writes the files that
//! the walk and the later trigger recovery read: task facts, run records, trigger files, worktrees and the marker.
//!
//! The counts come from one production start (11-01 12:04 PT, 8,003 nodes). It made 593 full passes: 296 open tasks,
//! 100 files after the marker, 19 runs with no end, 34 sequence refs and 144 existing worktrees. It skipped 7,410
//! sessions. The store held task metadata in 83.1 % of its sessions and run directories in 72.9 %. It held about 4,800
//! trigger files, about 25 of them pending. Every count scales with the session count. Full-pass sessions hold larger
//! event logs than the rest, so the full passes are drawn with that start's odds per size band (`FULL_PASS_ODDS`).
//!
//! No state in the home makes a lab server start an agent or signal a process:
//! - an open task belongs to a manager whose finished turns confirm every input, so no input waits for a launch;
//! - every launched run carries a pid at or above Linux's PID_MAX_LIMIT (2^22), which no process can hold;
//! - a run with no end sits in a closed task, and the walk ends it as interrupted;
//! - every node is a root, so no recovery step reports to a parent;
//! - a sequence run names the cron controller's kind and a cron task that no lab home configures;
//! - a pending trigger waits on an open task and fires on 2030-01-01.
//!
//! The boot marker and every file time are fixed instants around `marker_time()`. A server start rewrites the marker,
//! so `reset` restores the marker, the times and the worktrees from the plan that the seed gives.

use std::fs::{self, File};
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use charliebot_core::event_types;
use charliebot_core::text_repr::{self, DEFAULT_JSON};
use charliebot_store::control_events::{self, ACTOR_AGENT, ACTOR_SYSTEM, ACTOR_USER};
use charliebot_store::layout;
use charliebot_store::metadata::{TaskProfile, TaskSpec, TaskType, UtcDateTime};
use charliebot_store::run_records::{self, RunKind, RunRecord, SequenceRef};
use charliebot_store::trigger_files::{self, PendingTrigger, TriggerStatus};
use chrono::{NaiveDate, NaiveDateTime, TimeDelta};
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::Shape;
use crate::generate::{self, EventLog};
use crate::manifest::{MANIFEST_NAME, Manifest, ManifestSession, SessionMetadata};
use crate::rng::Rng;
use crate::text::{self, SyntheticText};

/// The file beside the sessions that holds the boot marker. `scripts/release/trial.sh --seed-home` copies it to the
/// trial home's `state/boot_recovery.json`, where the walk reads it.
pub const MARKER_SEED_NAME: &str = "boot_recovery.json";
/// The directory beside the sessions that holds the worktrees the run records name. Its dot keeps it out of the seed
/// copy, which takes only visible directories.
const WORKTREES_DIR: &str = ".worktrees";
/// The repository that work runs name. No lab step reads it, so it does not exist.
const LAB_REPO: &str = "/nonexistent/charliebot-lab-repo";
/// The cron task that sequence runs name. No lab home configures it, so the cron controller logs it as missing.
const LAB_CRON_TASK: &str = "lab-sequence";
/// The plan's stream is separate from the content stream, so `reset` can replay the plan without writing content.
const PLAN_STREAM: u64 = 0x7461_736b_7472_6565;

/// The reference start's node count, and its full-pass count for each keep reason.
const REFERENCE_NODES: f64 = 8003.0;
const REFERENCE_KEEPS: [(KeepReason, f64); 5] = [
  (KeepReason::StateOpen, 296.0),
  (KeepReason::FileAfterMarker, 100.0),
  (KeepReason::RunUnended, 19.0),
  (KeepReason::SequenceRef, 34.0),
  (KeepReason::WorktreeExists, 144.0),
];
/// The odds that a session took a full pass in the reference start, by event-log size band: (lower bound in bytes,
/// odds). 5 of the 11 logs of 50 MB or more took one, 40 of 145 between 10 and 50 MB, 57 of 716 between 1 and 10 MB,
/// and 491 of 7,210 under 1 MB. The whole-log reads of that start give the counts above 1 MB.
const FULL_PASS_ODDS: [(u64, f64); 4] =
  [(50_000_000, 5.0 / 11.0), (10_000_000, 40.0 / 145.0), (1_000_000, 57.0 / 716.0), (0, 491.0 / 7210.0)];
/// The store-wide shares of sessions with task metadata and with a run directory.
const TASK_SHARE: f64 = 0.831;
const RUNS_SHARE: f64 = 0.729;
/// The share of workers among the sessions with task metadata. The production split was not measured.
const WORKER_SHARE: f64 = 0.55;
/// The run count of a session with runs at each quantile, log-linear between knots. Production showed 1 to 243 runs per
/// session; the store-wide distribution was not measured.
const RUN_COUNT_KNOTS: [(f64, f64); 5] = [(0.0, 1.0), (0.5, 3.0), (0.9, 12.0), (0.99, 60.0), (1.0, 250.0)];
/// One run's span of the event log takes at most this many bytes, so a log of n such spans holds at least n runs.
const RUN_SPAN_BYTES: u64 = 2_000_000;
/// Trigger files and pending triggers per session: about 4,800 and 25 at about 8,100 sessions.
const TRIGGER_FILES_PER_SESSION: f64 = 4800.0 / 8100.0;
const PENDING_TRIGGERS_PER_SESSION: f64 = 25.0 / 8100.0;
/// The share of sessions that hold the fired trigger files.
const TRIGGER_HOLDER_SHARE: f64 = 0.2;
/// The lowest pid a launched run carries: Linux's PID_MAX_LIMIT on 64-bit hosts, which no process id reaches.
const STALE_PID_FLOOR: u32 = 1 << 22;

/// A session's activity lasts from one hour to this long after its start.
const ACTIVITY_SECONDS: u64 = 3 * 86_400;
/// The reference start came this long after its marker, so files after the marker fall in this window.
const AFTER_MARKER_SECONDS: u64 = 69_840;
/// Files before the marker end at least this long before it: the walk counts a file within one second of it as after.
const BEFORE_MARKER_MARGIN_SECONDS: i64 = 60;
/// An open task's files were last written within this window before the marker, or after the marker.
const OPEN_TASK_LOOKBACK_SECONDS: u64 = 7 * 86_400;

/// The keep reasons of the walk, in its check order (`boot_skip_reason`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum KeepReason {
  StateOpen,
  FileAfterMarker,
  RunUnended,
  SequenceRef,
  WorktreeExists,
}

/// How a run ended, as its record and the session's facts show.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RunEnd {
  /// A successful `run_finished` fact, and an `ended_at` on the record.
  Finished,
  /// Launched, with no end fact and no `ended_at`: its process was running when the server stopped.
  Unended,
}

/// Where a work run's worktree is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Worktree {
  /// The run names no worktree.
  Absent,
  /// The run names a worktree that delivery removed.
  Removed,
  /// The run names a worktree that exists.
  Present,
}

/// One run of a session.
struct RunPlan {
  id: String,
  kind: RunKind,
  end: RunEnd,
  started_at: NaiveDateTime,
  ended_at: NaiveDateTime,
  sequence: bool,
  worktree: Worktree,
  pid: u32,
  pid_start: u64,
}

/// One session: its keep reason (none for a skip), its node, its file time, its runs and its trigger files.
struct SessionPlan {
  reason: Option<KeepReason>,
  profile: TaskProfile,
  has_task: bool,
  started_at: NaiveDateTime,
  modified_at: NaiveDateTime,
  runs: Vec<RunPlan>,
  pending_triggers: usize,
  fired_triggers: usize,
}

/// The plan of every session, in the order `generate` writes them. One seed and one size list give one plan.
struct HomePlan {
  sessions: Vec<SessionPlan>,
}

/// The boot marker: the start of the last clean walk before the planned server start.
fn marker_time() -> NaiveDateTime {
  generate::first_session_time() + TimeDelta::seconds(generate::SESSION_START_SPREAD_SECONDS as i64 + 86_400)
}

/// The fire time of every pending trigger, after any lab start.
fn pending_fire_time() -> NaiveDateTime {
  NaiveDate::from_ymd_opt(2030, 1, 1).and_then(|date| date.and_hms_opt(0, 0, 0)).expect("the fire time is a valid date")
}

fn seconds(count: u64) -> TimeDelta {
  TimeDelta::seconds(count as i64)
}

/// The odds of a full pass for an event log of `size` bytes.
fn full_pass_odds(size: u64) -> f64 {
  FULL_PASS_ODDS.iter().find(|(floor, _)| size >= *floor).map(|(_, odds)| *odds).expect("the last band starts at 0")
}

/// The run count at quantile `u`, for 0 <= u <= 1.
fn run_count(u: f64) -> usize {
  for pair in RUN_COUNT_KNOTS.windows(2) {
    let ((u_low, count_low), (u_high, count_high)) = (pair[0], pair[1]);
    if u <= u_high {
      let position = (u - u_low) / (u_high - u_low);
      return (count_low.ln() + (count_high.ln() - count_low.ln()) * position).exp().round() as usize;
    }
  }
  unreachable!("the quantile {u} lies in [0, 1], which the last knot covers")
}

/// A random version-4 id from the plan's stream.
fn plan_id(rng: &mut Rng) -> String {
  generate::session_id(rng)
}

impl HomePlan {
  /// The plan for sessions whose event logs take `sizes` bytes, in session order.
  fn new(sizes: &[u64], seed: u64) -> Self {
    let count = sizes.len();
    let mut rng = Rng::new(seed ^ PLAN_STREAM);
    let reasons = Self::keep_reasons(sizes, &mut rng);
    let forced_runs =
      reasons.iter().filter(|reason| reason.is_some_and(|reason| reason != KeepReason::FileAfterMarker));
    let forced_runs = forced_runs.count() as f64;
    let rest_runs_share = ((RUNS_SHARE * count as f64 - forced_runs) / (count as f64 - forced_runs)).clamp(0.0, 1.0);
    let mut sessions: Vec<SessionPlan> = reasons
      .iter()
      .zip(sizes)
      .map(|(reason, size)| Self::session(*reason, *size, rest_runs_share, &mut rng))
      .collect();

    let open: Vec<usize> = (0..count).filter(|index| reasons[*index] == Some(KeepReason::StateOpen)).collect();
    let pending = (PENDING_TRIGGERS_PER_SESSION * count as f64).round() as usize;
    for _ in 0..pending {
      sessions[open[rng.below(open.len() as u64) as usize]].pending_triggers += 1;
    }
    let mut holders: Vec<usize> = (0..count).collect();
    rng.shuffle(&mut holders);
    holders.truncate((TRIGGER_HOLDER_SHARE * count as f64).ceil() as usize);
    let fired = ((TRIGGER_FILES_PER_SESSION * count as f64).round() as usize).saturating_sub(pending);
    for _ in 0..fired {
      sessions[holders[rng.below(holders.len() as u64) as usize]].fired_triggers += 1;
    }
    Self { sessions }
  }

  /// Each session's keep reason. The full-pass sessions are a weighted draw without replacement by size band; the
  /// reasons are then dealt to them in a seeded order.
  fn keep_reasons(sizes: &[u64], rng: &mut Rng) -> Vec<Option<KeepReason>> {
    let targets =
      REFERENCE_KEEPS.map(|(reason, count)| (reason, (count * sizes.len() as f64 / REFERENCE_NODES).round()));
    let full = targets.iter().map(|(_, count)| *count as usize).sum::<usize>().min(sizes.len());
    // An exponential race: the smallest keys win, and each session's key falls faster with higher odds.
    let mut keyed: Vec<(f64, usize)> =
      sizes.iter().enumerate().map(|(index, size)| (-(1.0 - rng.unit()).ln() / full_pass_odds(*size), index)).collect();
    keyed.sort_by(|left, right| left.0.total_cmp(&right.0));
    let mut chosen: Vec<usize> = keyed[..full].iter().map(|(_, index)| *index).collect();
    rng.shuffle(&mut chosen);
    let mut reasons = vec![None; sizes.len()];
    let mut dealt = chosen.into_iter();
    for (reason, count) in targets {
      for index in dealt.by_ref().take(count as usize) {
        reasons[index] = Some(reason);
      }
    }
    reasons
  }

  fn session(reason: Option<KeepReason>, size: u64, rest_runs_share: f64, rng: &mut Rng) -> SessionPlan {
    let (profile, has_task) = match reason {
      Some(KeepReason::StateOpen) => (TaskProfile::Manager, rng.unit() < TASK_SHARE),
      Some(KeepReason::SequenceRef | KeepReason::WorktreeExists) => (TaskProfile::Worker, true),
      Some(KeepReason::FileAfterMarker | KeepReason::RunUnended) | None => {
        let has_task = rng.unit() < TASK_SHARE;
        let worker = has_task && rng.unit() < WORKER_SHARE;
        (if worker { TaskProfile::Worker } else { TaskProfile::Manager }, has_task)
      }
    };
    let has_runs = match reason {
      Some(KeepReason::StateOpen | KeepReason::RunUnended | KeepReason::SequenceRef | KeepReason::WorktreeExists) => {
        true
      }
      Some(KeepReason::FileAfterMarker) | None => rng.unit() < rest_runs_share,
    };
    let marker = marker_time();
    let started_at = generate::first_session_time() + seconds(rng.range(0, generate::SESSION_START_SPREAD_SECONDS));
    let activity = seconds(rng.range(3_600, ACTIVITY_SECONDS));
    let modified_at = match reason {
      Some(KeepReason::FileAfterMarker) => marker + seconds(rng.range(1, AFTER_MARKER_SECONDS)),
      Some(KeepReason::StateOpen) => {
        let offset = rng.range(0, OPEN_TASK_LOOKBACK_SECONDS + AFTER_MARKER_SECONDS);
        (marker - seconds(OPEN_TASK_LOOKBACK_SECONDS) + seconds(offset)).max(started_at + activity)
      }
      Some(KeepReason::RunUnended | KeepReason::SequenceRef | KeepReason::WorktreeExists) | None => {
        (started_at + activity).min(marker - TimeDelta::seconds(BEFORE_MARKER_MARGIN_SECONDS))
      }
    };
    let runs = if has_runs { Self::runs(reason, profile, size, started_at, modified_at, rng) } else { Vec::new() };
    SessionPlan { reason, profile, has_task, started_at, modified_at, runs, pending_triggers: 0, fired_triggers: 0 }
  }

  /// The runs of one session, in start order. A sequence-ref session holds cron steps; an unended session's last run has
  /// no end; a worktree session's last work run keeps its worktree.
  fn runs(
    reason: Option<KeepReason>,
    profile: TaskProfile,
    size: u64,
    started_at: NaiveDateTime,
    modified_at: NaiveDateTime,
    rng: &mut Rng,
  ) -> Vec<RunPlan> {
    let count = run_count(rng.unit()).max(size.div_ceil(RUN_SPAN_BYTES) as usize);
    let span = (modified_at - started_at).num_seconds().max(2 * count as i64);
    let step = span / count as i64;
    (0..count)
      .map(|index| {
        let kind = match (reason, profile) {
          (Some(KeepReason::SequenceRef), _) => RunKind::ScheduledStep,
          (_, TaskProfile::Manager) => RunKind::ManagerTurn,
          (_, TaskProfile::Worker) => RunKind::Work,
        };
        let last = index + 1 == count;
        let end = if reason == Some(KeepReason::RunUnended) && last { RunEnd::Unended } else { RunEnd::Finished };
        let worktree = match kind {
          RunKind::Work if reason == Some(KeepReason::WorktreeExists) && last => Worktree::Present,
          RunKind::Work => Worktree::Removed,
          _ => Worktree::Absent,
        };
        let run_started = started_at + TimeDelta::seconds(step * index as i64);
        RunPlan {
          id: plan_id(rng),
          kind,
          end,
          started_at: run_started,
          ended_at: run_started + TimeDelta::seconds(step / 2),
          sequence: reason == Some(KeepReason::SequenceRef),
          worktree,
          pid: STALE_PID_FLOOR + rng.below(1 << 20) as u32,
          pid_start: rng.range(1_000_000, 99_999_999),
        }
      })
      .collect()
  }
}

/// A chrono instant as the store's UTC time.
fn utc(moment: NaiveDateTime) -> UtcDateTime {
  UtcDateTime::parse(&generate::stamp(moment)).expect("a generated stamp parses")
}

fn system_time(moment: NaiveDateTime) -> SystemTime {
  let micros = moment.and_utc().timestamp_micros();
  SystemTime::UNIX_EPOCH + Duration::from_micros(u64::try_from(micros).expect("every planned time is after 1970"))
}

/// Gives one plain user message an `id` ahead of its other fields, as the server's input events carry one, and returns
/// the id.
fn label_input(rng: &mut Rng, event: &mut Value) -> String {
  let id = plan_id(rng);
  let Value::Object(fields) = std::mem::take(event) else { unreachable!("a user message is a JSON object") };
  let mut labelled = Map::with_capacity(fields.len() + 1);
  labelled.insert("id".to_string(), Value::String(id.clone()));
  labelled.extend(fields);
  *event = Value::Object(labelled);
  id
}

/// The event log of one task node, and the input ids each run's finish confirms.
struct TaskLog {
  bytes: u64,
  run_inputs: Vec<Vec<String>>,
}

/// Writes one node's event log of about `target` bytes.
///
/// The log opens with `task_created`. Each run then takes an equal span of turns, the last span ends with the final
/// user message, and a finished run's `run_finished` fact closes its span and confirms the span's inputs. A closed task
/// ends with `task_closed`.
fn write_task_log(
  path: &Path,
  session_id: &str,
  session: &SessionPlan,
  target: u64,
  rng: &mut Rng,
  text: &SyntheticText,
  final_text: &str,
) -> Result<TaskLog> {
  let mut log = EventLog::create(path, session.started_at)?;
  let actor = match session.profile {
    TaskProfile::Manager => ACTOR_USER,
    TaskProfile::Worker => ACTOR_AGENT,
  };
  let request_id = plan_id(rng);
  let event_id = plan_id(rng);
  let created_at = utc(session.started_at);
  log.push(&control_events::build_task_created_event(
    actor,
    session_id,
    &request_id,
    None,
    None,
    Some(&event_id),
    Some(created_at),
  ))?;
  let spans = session.runs.len().max(1);
  let mut run_inputs = Vec::with_capacity(session.runs.len());
  for span in 0..spans {
    let limit = target as i64 * (span as i64 + 1) / spans as i64;
    let mut inputs = Vec::new();
    log.write_turns(limit, rng, text, &mut |rng: &mut Rng, event: &mut Value| inputs.push(label_input(rng, event)))?;
    if span + 1 == spans {
      let mut last = generate::final_event(final_text, &log.next_stamp());
      inputs.push(label_input(rng, &mut last));
      log.push(&last)?;
    }
    if let Some(run) = session.runs.get(span) {
      if run.end == RunEnd::Finished {
        let event_id = plan_id(rng);
        let finished_at = UtcDateTime::parse(&log.next_stamp()).expect("an event stamp parses");
        log.push(&control_events::build_control_event(
          event_types::RUN_FINISHED,
          ACTOR_SYSTEM,
          session_id,
          Some(&event_id),
          Some(finished_at),
          [
            ("run_id".to_string(), Value::String(run.id.clone())),
            (event_types::INPUT_EVENT_IDS.to_string(), json!(inputs)),
            ("outcome".to_string(), json!("success")),
          ],
        ))?;
      }
      run_inputs.push(inputs);
    }
  }
  if session.reason != Some(KeepReason::StateOpen) {
    let request_id = plan_id(rng);
    let closed_at = UtcDateTime::parse(&log.next_stamp()).expect("an event stamp parses");
    log.push(&control_events::build_control_event(
      event_types::TASK_CLOSED,
      ACTOR_AGENT,
      session_id,
      Some(&control_events::stable_close_event_id(session_id, &request_id)),
      Some(closed_at),
      [
        ("request_id".to_string(), json!(request_id)),
        ("outcome".to_string(), json!("completed")),
        ("summary".to_string(), json!(text::phrase(rng))),
        ("result_refs".to_string(), json!([])),
        ("run_ids".to_string(), json!(session.runs.iter().map(|run| run.id.as_str()).collect::<Vec<_>>())),
        ("report_to".to_string(), Value::Null),
      ],
    ))?;
  }
  let (bytes, _) = log.finish()?;
  Ok(TaskLog { bytes, run_inputs })
}

/// One node's `metadata.json`: the default session fields, then the task-tree fields.
#[derive(Serialize)]
struct NodeMetadata {
  #[serde(flatten)]
  session: SessionMetadata,
  task_parent_id: Option<String>,
  task: Option<TaskSpec>,
}

fn task_spec(profile: TaskProfile, rng: &mut Rng) -> TaskSpec {
  match profile {
    TaskProfile::Manager => TaskSpec { goal: text::phrase(rng).to_string(), ..TaskSpec::default() },
    TaskProfile::Worker => TaskSpec {
      goal: text::phrase(rng).to_string(),
      acceptance: vec![text::phrase(rng).to_string()],
      repo_path: Some(LAB_REPO.to_string()),
      base_branch: Some("main".to_string()),
      task_type: Some(TaskType::Implement),
      ..TaskSpec::default()
    },
  }
}

fn run_record(session_id: &str, run: &RunPlan, inputs: Vec<String>, worktrees: &Path) -> RunRecord {
  let work = run.kind == RunKind::Work;
  let finished = run.end == RunEnd::Finished;
  RunRecord {
    id: run.id.clone(),
    session_id: session_id.to_string(),
    kind: run.kind,
    input_event_ids: inputs,
    task_spec_hash: None,
    task_spec_ref: None,
    prompt_snapshot_ref: None,
    retry_of_run_id: None,
    review_of_run_id: None,
    backend: Some("mock-backend".to_string()),
    model: Some("mock-model".to_string()),
    native_session_id: None,
    pid: Some(run.pid),
    pid_start: Some(run.pid_start.to_string()),
    started_at: Some(utc(run.started_at)),
    ended_at: finished.then(|| utc(run.ended_at)),
    exit_code: finished.then_some(0),
    repo_path: work.then(|| LAB_REPO.to_string()),
    base_branch: work.then(|| "main".to_string()),
    branch_name: work.then(|| format!("charliebot/task-{}", &run.id[..8])),
    worktree_path: match run.worktree {
      Worktree::Absent => None,
      Worktree::Removed | Worktree::Present => Some(worktrees.join(&run.id).display().to_string()),
    },
    sequence_ref: run.sequence.then(|| SequenceRef {
      kind: "cron_steps".to_string(),
      owner_ref: format!("cron:{LAB_CRON_TASK}:{}", stamp_firing(run.started_at)),
      position: 0,
    }),
    raw_log_ref: None,
    events_ref: None,
    result_ref: None,
  }
}

/// A cron firing name, as the scheduler names one by its scheduled minute.
fn stamp_firing(moment: NaiveDateTime) -> String {
  moment.format("%Y%m%dT%H%M").to_string()
}

fn trigger(session_id: &str, session: &SessionPlan, status: TriggerStatus, rng: &mut Rng) -> PendingTrigger {
  let created = session.started_at;
  let (fire_at, fired_at, fire_reason) = match status {
    TriggerStatus::Pending => (pending_fire_time(), None, None),
    TriggerStatus::Fired => {
      let window = (session.modified_at - created).num_seconds().max(1) as u64;
      let fired = created + seconds(rng.range(0, window));
      (fired, Some(utc(fired)), Some("timeout".to_string()))
    }
    TriggerStatus::Cancelled => unreachable!("the shape writes pending and fired triggers"),
  };
  PendingTrigger {
    id: plan_id(rng),
    session_id: session_id.to_string(),
    fire_at: utc(fire_at),
    message: text::phrase(rng).to_string(),
    created_at: utc(created),
    status,
    fired_at,
    watch_targets: Vec::new(),
    fire_reason,
  }
}

/// Sets the file times the walk compares with the marker: the event log's and the metadata's.
fn set_session_times(session_dir: &Path, modified_at: NaiveDateTime) -> Result<()> {
  let time = system_time(modified_at);
  for path in [layout::chat_events_path(session_dir), layout::metadata_path(session_dir)] {
    File::options()
      .write(true)
      .open(&path)
      .and_then(|file| file.set_modified(time))
      .with_context(|| format!("cannot set the time of {}", path.display()))?;
  }
  Ok(())
}

/// Writes the marker the walk reads, in the bytes the server writes it (`write_boot_recovery_marker`).
fn write_marker_seed(home: &Path) -> Result<()> {
  let payload = json!({"pass_started_at": utc(marker_time()).to_isoformat_text()});
  let path = home.join(MARKER_SEED_NAME);
  fs::write(&path, text_repr::json_dumps(&payload, &DEFAULT_JSON))
    .with_context(|| format!("cannot write {}", path.display()))
}

/// Writes a task-tree home of `sessions` sessions for `seed` into `staging`, whose final path is `home`. Returns the
/// total event-log bytes.
pub(crate) fn write_home(staging: &Path, home: &Path, sessions: usize, seed: u64) -> Result<u64> {
  let (mut rng, text, sizes) = generate::planned_home(sessions, seed);
  let plan = HomePlan::new(&sizes, seed);
  let worktrees = home.join(WORKTREES_DIR);
  fs::create_dir(staging.join(WORKTREES_DIR)).context("cannot create the worktree directory")?;
  let mut manifest_sessions = Vec::with_capacity(sessions);
  let mut total = 0;
  for (session, size) in plan.sessions.iter().zip(&sizes) {
    let id = generate::session_id(&mut rng);
    let session_dir = staging.join(&id);
    fs::create_dir_all(layout::data_dir(&session_dir))
      .with_context(|| format!("cannot create {}", session_dir.display()))?;
    let final_text = format!("Checkpoint {id}: {}", text::phrase(&mut rng));
    let log =
      write_task_log(&layout::chat_events_path(&session_dir), &id, session, *size, &mut rng, &text, &final_text)?;
    let metadata = NodeMetadata {
      session: SessionMetadata {
        id: id.clone(),
        name: text::title(&mut rng),
        status: "active",
        profile: match session.profile {
          TaskProfile::Manager => "manager",
          TaskProfile::Worker => "worker",
        },
        backend: "mock-backend",
        created_at: generate::stamp(session.started_at),
        updated_at: generate::stamp(session.modified_at),
        schema_version: 2,
        has_unread: false,
        starred: false,
      },
      task_parent_id: None,
      task: session.has_task.then(|| task_spec(session.profile, &mut rng)),
    };
    generate::write_json(&layout::metadata_path(&session_dir), &metadata, b"  ")?;
    for (run, inputs) in session.runs.iter().zip(log.run_inputs) {
      let run_dir = layout::runs_dir(&session_dir).join(&run.id);
      fs::create_dir_all(&run_dir).with_context(|| format!("cannot create {}", run_dir.display()))?;
      let record = run_records::run_record_text(&run_record(&id, run, inputs, &worktrees));
      let path = run_dir.join(run_records::METADATA_NAME);
      fs::write(&path, record).with_context(|| format!("cannot write {}", path.display()))?;
      if run.worktree == Worktree::Present {
        let path = staging.join(WORKTREES_DIR).join(&run.id);
        fs::create_dir(&path).with_context(|| format!("cannot create {}", path.display()))?;
      }
    }
    let statuses = std::iter::repeat_n(TriggerStatus::Pending, session.pending_triggers)
      .chain(std::iter::repeat_n(TriggerStatus::Fired, session.fired_triggers));
    for status in statuses {
      trigger_files::write_trigger(&session_dir, &trigger(&id, session, status, &mut rng))?;
    }
    set_session_times(&session_dir, session.modified_at)?;
    manifest_sessions.push(ManifestSession { id, event_log_bytes: log.bytes, last_message: final_text });
    total += log.bytes;
  }
  write_marker_seed(staging)?;
  let manifest = Manifest {
    seed,
    session_count: sessions,
    event_log_bytes: total,
    sessions: manifest_sessions,
    shape: Shape::TaskTree,
  };
  generate::write_json(&staging.join(MANIFEST_NAME), &manifest, b" ")?;
  Ok(total)
}

/// Returns the task-tree home `home` to its generated state: the marker, every session's file times, and the worktrees
/// the run records name.
///
/// A server start rewrites the marker, and a lab start can touch file times or remove a worktree, so each timed start
/// begins with a reset. An event log whose size differs from the manifest fails the reset: a server wrote into this
/// home, and only a new `generate` restores it.
pub fn reset(home: &Path) -> Result<()> {
  let manifest = Manifest::read(home)?;
  if manifest.shape != Shape::TaskTree {
    bail!("{} holds a {:?} home; only a task-tree home has a generated state to reset", home.display(), manifest.shape);
  }
  let (_, _, sizes) = generate::planned_home(manifest.session_count, manifest.seed);
  let plan = HomePlan::new(&sizes, manifest.seed);
  let worktrees = fs::canonicalize(home)?.join(WORKTREES_DIR);
  let mut restored_worktrees = 0;
  for (session, listed) in plan.sessions.iter().zip(&manifest.sessions) {
    let session_dir = home.join(&listed.id);
    let log_path = layout::chat_events_path(&session_dir);
    let bytes = fs::metadata(&log_path).with_context(|| format!("cannot stat {}", log_path.display()))?.len();
    if bytes != listed.event_log_bytes {
      bail!(
        "{} holds {bytes} bytes, not the generated {}: generate the home again",
        log_path.display(),
        listed.event_log_bytes
      );
    }
    set_session_times(&session_dir, session.modified_at)?;
    for run in session.runs.iter().filter(|run| run.worktree == Worktree::Present) {
      let path = worktrees.join(&run.id);
      if !path.is_dir() {
        fs::create_dir(&path).with_context(|| format!("cannot create {}", path.display()))?;
        restored_worktrees += 1;
      }
    }
  }
  write_marker_seed(home)?;
  println!(
    "reset {} sessions in {}: marker {}, {restored_worktrees} worktrees restored",
    manifest.session_count,
    home.display(),
    utc(marker_time()).to_isoformat_text()
  );
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The plan deals every keep reason at its scaled reference count, so a lab start's full-pass distribution lands
  /// within the A1 range of the reference start.
  #[test]
  fn a_production_size_plan_deals_each_keep_reason_its_scaled_count() {
    let sessions = 8_100;
    let (_, _, sizes) = generate::planned_home(sessions, 1);
    let plan = HomePlan::new(&sizes, 1);
    for (reason, reference) in REFERENCE_KEEPS {
      let dealt = plan.sessions.iter().filter(|session| session.reason == Some(reason)).count();
      assert_eq!(dealt, (reference * sessions as f64 / REFERENCE_NODES).round() as usize, "{reason:?}");
    }
    let with_runs = plan.sessions.iter().filter(|session| !session.runs.is_empty()).count() as f64;
    assert!((with_runs / sessions as f64 - RUNS_SHARE).abs() < 0.02, "{with_runs} sessions hold runs");
  }
}
