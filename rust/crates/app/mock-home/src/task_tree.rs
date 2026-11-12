//! The task-tree shape: a synthetic home whose task tree has the scale and proportions of a production start.
//!
//! On startup, the task-tree walk (`charliebot_tasks::recovery`) gives a full node pass to each session that one keep
//! reason holds, and skips the rest. The walk checks five reasons in order: an open task, a chat log or metadata file
//! after the marker, an unended run, a sequence ref, and an existing worktree. Every session has one reason, or none.
//! The shape writes the task facts, run records, trigger files, worktrees and marker for the walk and trigger recovery.
//!
//! `REFERENCE_NODES` and `REFERENCE_KEEPS` hold the 09:06Z release start's counts: 8,930 nodes, 574 full passes and
//! 8,356 skipped sessions. Every keep count scales with the session count. `TASK_SHARE`, `RUNS_SHARE` and
//! `FULL_PASS_ODDS` hold the shares of an earlier 8,003-node start; full passes use its odds per size band.
//!
//! Some large full-pass logs take an archive: their first events move to `data/archives` and the metadata's
//! `archive_offset` counts them. A build whose fact scan refuses archived sessions reads such a session's live log
//! whole in its node passes, as production's archived sessions did, and the lab keeps that read pattern for it
//! (`HomePlan::plan_archives`).
//!
//! The nodes form production's tree, so the node passes read other nodes' logs as production's do (`HomePlan::plant_tree`). A
//! parent is a manager with one of the largest logs, and a child's parent is drawn in proportion to the parents' log
//! bytes. A closed child's `task_closed` reports to its parent, and the parent's log holds that report. A pass then
//! folds the parent's log to find the report (`recover_pending_reports`). A closed worker of a type other than implement
//! closed on its newest run's automatic request, so the replay of an earlier successful run tries that close again
//! and primes the child's and the parent's aggregators before it finds the task closed (`replay_followups`).
//!
//! No state in the home makes a lab server start an agent or signal a process. An open task's manager confirms
//! every input, a child's report among them, a repaired run sits in a closed task, and a live run sits in an open one
//! (`HomePlan::plant_live_runs`). Every run's pid is at or above
//! Linux's PID_MAX_LIMIT, a parent is a root, a sequence run names an unconfigured cron task, and a pending trigger
//! fires on 2030-01-01. No worker is a cold read, whose replay can start a run. The boot marker and every file time are
//! fixed around `marker_time()`, so `reset` restores them from the seed's plan.

use std::fs::{self, File};
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use charliebot_core::event_types;
use charliebot_core::home::BOOT_RECOVERY_FILE_NAME;
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
use crate::manifest::{MANIFEST_NAME, Manifest, ManifestSession, SessionMetadata};
use crate::rng::Rng;
use crate::session::{self, EventLog};
use crate::text::{self, SyntheticText};

/// The directory beside the sessions that holds the worktrees the run records name. Its dot keeps it out of the seed
/// copy, which takes only visible directories.
const WORKTREES_DIR: &str = ".worktrees";
/// The repository that work runs name. No lab step reads it, so it does not exist.
const LAB_REPO: &str = "/nonexistent/charliebot-lab-repo";
/// The cron task that sequence runs name. No lab home configures it, so the cron controller logs it as missing.
const LAB_CRON_TASK: &str = "lab-sequence";
/// The plan's stream is separate from the content stream, so `reset` can replay the plan without writing content.
const PLAN_STREAM: u64 = 0x7461_736b_7472_6565;
/// The session ids' stream. A parent's log names its children's ids, so the plan holds every id before a log is written.
const ID_STREAM: u64 = 0x6e6f_6465_2d69_6473;

/// The production tree of 2026-11-03 (8,722 sessions): 374 parents, all managers, and 7,148 children. The parents
/// are mostly among the largest logs, and each parent's child count grows about in proportion to its log bytes.
const PARENT_SHARE: f64 = 374.0 / 8722.0;
/// The share of the task sessions other than parents that have a parent.
const CHILD_SHARE: f64 = 0.977;
/// The workers' task types in that tree, by worker count. The 788 cold reads count as untyped workers, because a cold
/// read's replay can start a run.
const WORKER_TYPES: [(Option<TaskType>, f64); 5] = [
  (Some(TaskType::Implement), 1679.0),
  (None, 2243.0 + 788.0),
  (Some(TaskType::Verify), 1106.0),
  (Some(TaskType::QuickEdit), 539.0),
  (Some(TaskType::ScriptRun), 509.0),
];
/// The share of closed workers with a finished last run that closed on its automatic request and reported to their
/// parent; the rest were archived by the user, without a report. A sample of 296 closed workers held 241 such closes.
const AUTO_CLOSE_SHARE: f64 = 0.84;
/// The task types of the 195 workers whose worktree existed in that home, by count. A worker kept for its worktree
/// draws its type from these.
const WORKTREE_WORKER_TYPES: [(Option<TaskType>, f64); 4] = [
  (Some(TaskType::Implement), 93.0),
  (Some(TaskType::QuickEdit), 48.0),
  (None, 44.0),
  (Some(TaskType::ScriptRun), 10.0),
];

/// The 09:06Z release start's node count, and its full-pass count for each keep reason.
const REFERENCE_NODES: f64 = 8930.0;
const REFERENCE_KEEPS: [(KeepReason, f64); 5] = [
  (KeepReason::StateOpen, 318.0),
  (KeepReason::FileAfterMarker, 4.0),
  (KeepReason::RunUnended, 20.0),
  (KeepReason::SequenceRef, 38.0),
  (KeepReason::WorktreeExists, 194.0),
];
/// The odds that a session took a full pass in the reference start, by event-log size band: (lower bound in bytes,
/// odds). 5 of the 11 logs of 50 MB or more took one, and 40 of 145 between 10 and 50 MB. The bands between 1 and
/// 10 MB and under 1 MB took 57 of 716 and 491 of 7,210. The whole-log reads of that start give the counts above 1 MB.
const FULL_PASS_ODDS: [(u64, f64); 4] =
  [(50_000_000, 5.0 / 11.0), (10_000_000, 40.0 / 145.0), (1_000_000, 57.0 / 716.0), (0, 491.0 / 7210.0)];
/// The store-wide shares of sessions with task metadata and with a run directory.
const TASK_SHARE: f64 = 0.831;
const RUNS_SHARE: f64 = 0.729;
/// The share of workers among the sessions with task metadata. The production split was not measured.
const WORKER_SHARE: f64 = 0.55;
/// The run count of a session with runs at each quantile, log-linear between knots, for managers and for workers.
/// Production showed 1 to 243 runs per session. The reference start replayed 6,907 run follow-ups on its 593 full
/// passes, 327 of them for workers' work runs (`worker_delivery_awaits_review`). These knots give a full-pass plan
/// those totals, scaled to the session count.
const MANAGER_RUN_COUNT_KNOTS: [(f64, f64); 5] = [(0.0, 1.0), (0.5, 7.0), (0.9, 44.0), (0.99, 140.0), (1.0, 243.0)];
const WORKER_RUN_COUNT_KNOTS: [(f64, f64); 5] = [(0.0, 1.0), (0.4, 1.0), (0.8, 2.0), (0.97, 5.0), (1.0, 10.0)];
/// One manager turn's span of the event log takes at most this many bytes, so a log of n such spans holds at least n
/// turns.
const TURN_SPAN_BYTES: u64 = 1_000_000;
/// Trigger files and pending triggers per session: about 4,800 and 25 at about 8,100 sessions.
const TRIGGER_FILES_PER_SESSION: f64 = 4800.0 / 8100.0;
const PENDING_TRIGGERS_PER_SESSION: f64 = 25.0 / 8100.0;
/// The share of sessions that hold the fired trigger files.
const TRIGGER_HOLDER_SHARE: f64 = 0.2;
/// A parent whose log is between these lengths takes an archive. The reference start's node passes read the live logs
/// of ten archived sessions whole, from 1 MB to 84.7 MB each, most of them parents read by their children's passes. A
/// longer parent would read more than that reference's largest log, so it keeps its log whole.
const ARCHIVE_MIN_BYTES: u64 = 20_000_000;
const ARCHIVE_MAX_BYTES: u64 = 100_000_000;
/// The share of an archived log's bytes that its archive file holds. The two reference sessions that the start read
/// whole held 12 % and 30 % in their archives.
const ARCHIVE_FRACTION: f64 = 0.2;
/// The archive file an archived session's first events go to. The store reads every `chat_events.*.jsonl` in
/// `data/archives`, so the name only has to match that pattern.
const ARCHIVE_FILE_NAME: &str = "chat_events.2026-W44.jsonl";
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

/// The open tasks whose last run has a live process at a release start. Production's release start resumed 10 runs.
const LIVE_RUN_SESSIONS: usize = 10;

/// The file that lists each live run as `<session> <run>`, which a release start gives a live process.
const LIVE_RUNS_NAME: &str = "live-runs.txt";

/// The keep reasons of the walk, in its check order (`boot_skip_reason`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum KeepReason {
  StateOpen,
  FileAfterMarker,
  RunUnended,
  SequenceRef,
  WorktreeExists,
}

impl KeepReason {
  /// Whether a session kept for `reason` needs task metadata: a worker's task names its repository.
  fn needs_task(reason: Option<KeepReason>) -> bool {
    matches!(reason, Some(KeepReason::SequenceRef | KeepReason::WorktreeExists))
  }

  /// Whether a session kept for `reason` needs runs. Three reasons name a run, and an open task's finished turns confirm
  /// its inputs; a recent file needs none.
  fn needs_runs(reason: Option<KeepReason>) -> bool {
    matches!(
      reason,
      Some(KeepReason::StateOpen | KeepReason::RunUnended | KeepReason::SequenceRef | KeepReason::WorktreeExists)
    )
  }
}

/// How a run ended, as its record and the session's facts show.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RunEnd {
  /// A successful `run_finished` fact, and an `ended_at` on the record.
  Finished,
  /// A successful `run_finished` fact with no `ended_at` on the record: the end metadata write did not land, and the
  /// boot repairs it, as it does for the unended runs of production's closed tasks.
  Repaired,
  /// Launched, with no end fact and no `ended_at`: its process was running when the server stopped. A release start
  /// reattaches a live process to each one, which `live-runs.txt` lists.
  Live,
}

impl RunEnd {
  /// Whether the run's log holds its `run_finished` fact.
  fn has_finish_fact(self) -> bool {
    self != RunEnd::Live
  }
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

/// One session: its id, its keep reason (none for a skip), its node and place in the tree, its file time, its runs,
/// its close and its trigger files.
struct SessionPlan {
  id: String,
  reason: Option<KeepReason>,
  profile: TaskProfile,
  has_task: bool,
  /// A worker's task type; `None` for a manager and for an untyped worker.
  task_type: Option<TaskType>,
  /// The index of the parent's plan.
  parent: Option<usize>,
  started_at: NaiveDateTime,
  modified_at: NaiveDateTime,
  runs: Vec<RunPlan>,
  /// The `task_closed` fact of a session that is not open.
  close: Option<ClosePlan>,
  pending_triggers: usize,
  fired_triggers: usize,
  /// Whether the log's first `ARCHIVE_FRACTION` of bytes sits in an archive file, and the metadata names its events.
  archived: bool,
}

/// How a closed session closed: the request, who closed it and how, and the plan index of the parent it reported to.
struct ClosePlan {
  request_id: String,
  actor: &'static str,
  outcome: &'static str,
  summary: &'static str,
  report_to: Option<usize>,
}

/// One child report in a parent's log: the stable report id, the child and the child's close event.
struct ReportPlan {
  id: String,
  child_id: String,
  close_event_id: String,
  outcome: &'static str,
  summary: &'static str,
}

/// The plan of every session, in the order `generate` writes them. One seed and one size list give one plan.
struct HomePlan {
  sessions: Vec<SessionPlan>,
  /// The child reports each parent's log holds, by plan index, in child order.
  reports: Vec<Vec<ReportPlan>>,
}

/// The boot marker: the start of the last clean walk before the planned server start.
fn marker_time() -> NaiveDateTime {
  session::first_session_time() + TimeDelta::seconds(session::SESSION_START_SPREAD_SECONDS as i64 + 86_400)
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

/// The run count at quantile `u` of `knots`, for 0 <= u <= 1.
fn run_count(knots: &[(f64, f64)], u: f64) -> usize {
  for pair in knots.windows(2) {
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
  session::session_id(rng)
}

impl HomePlan {
  /// The plan for sessions whose event logs take `sizes` bytes, in session order.
  fn new(sizes: &[u64], seed: u64) -> Self {
    let count = sizes.len();
    let mut rng = Rng::new(seed ^ PLAN_STREAM);
    let mut id_rng = Rng::new(seed ^ ID_STREAM);
    let reasons = Self::keep_reasons(sizes, &mut rng);
    let tasks = Self::exact_flags(&reasons, TASK_SHARE, KeepReason::needs_task, &mut rng);
    let with_runs = Self::exact_flags(&reasons, RUNS_SHARE, KeepReason::needs_runs, &mut rng);
    let mut sessions: Vec<SessionPlan> = (0..count)
      .map(|index| {
        let id = session::session_id(&mut id_rng);
        Self::session(id, reasons[index], sizes[index], tasks[index], with_runs[index], &mut rng)
      })
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
    // The tree draws come after every draw above, so the keep reasons, runs and triggers stay those of a plan without
    // the tree.
    Self::plant_tree(&mut sessions, sizes, &mut rng);
    Self::type_workers(&mut sessions, &mut rng);
    Self::plan_closes(&mut sessions, &mut rng);
    Self::plan_archives(&mut sessions, sizes);
    Self::plant_live_runs(&mut sessions);
    let reports = Self::reports(&sessions);
    Self { sessions, reports }
  }

  /// Whether each session is the parent of another session.
  fn has_child(sessions: &[SessionPlan]) -> Vec<bool> {
    let mut has_child = vec![false; sessions.len()];
    for session in sessions {
      if let Some(parent) = session.parent {
        has_child[parent] = true;
      }
    }
    has_child
  }

  /// Archives every parent whose planned log is within `ARCHIVE_MIN_BYTES` and `ARCHIVE_MAX_BYTES`. A child's pass
  /// reads its archived parent's live log whole on a build whose fact scan refuses archived sessions, as the reference
  /// start's node passes did, and a parent that is itself a full pass reads its own live log in its pass.
  fn plan_archives(sessions: &mut [SessionPlan], sizes: &[u64]) {
    let has_child = Self::has_child(sessions);
    for (index, (session, size)) in sessions.iter_mut().zip(sizes).enumerate() {
      session.archived = has_child[index] && (ARCHIVE_MIN_BYTES..=ARCHIVE_MAX_BYTES).contains(size);
    }
  }

  /// Makes the last run of each of the first `LIVE_RUN_SESSIONS` open tasks with runs a live run: a release start finds
  /// its process alive and reattaches it, as production's release start resumed its runs of open tasks. A parent is
  /// skipped, so each of its reports keeps a span whose run holds a finish fact.
  fn plant_live_runs(sessions: &mut [SessionPlan]) {
    let has_child = Self::has_child(sessions);
    let live = sessions
      .iter_mut()
      .enumerate()
      .filter(|(index, session)| {
        session.reason == Some(KeepReason::StateOpen) && !session.runs.is_empty() && !has_child[*index]
      })
      .take(LIVE_RUN_SESSIONS);
    for (_, session) in live {
      session.runs.last_mut().expect("a session with runs has a last run").end = RunEnd::Live;
    }
  }

  /// Gives the tree its parents and children. The parents are the managers with a task and a finished run whose logs are
  /// the largest, `PARENT_SHARE` of the sessions. Each other task session gets a parent with odds `CHILD_SHARE`, drawn in
  /// proportion to the parents' log bytes. A parent stays a root.
  fn plant_tree(sessions: &mut [SessionPlan], sizes: &[u64], rng: &mut Rng) {
    let mut managers: Vec<usize> = (0..sessions.len())
      .filter(|index| {
        let session = &sessions[*index];
        session.profile == TaskProfile::Manager
          && session.has_task
          && session.runs.iter().any(|run| run.end == RunEnd::Finished)
      })
      .collect();
    managers.sort_by_key(|index| (std::cmp::Reverse(sizes[*index]), *index));
    managers.truncate((PARENT_SHARE * sessions.len() as f64).round() as usize);
    let parents = managers;
    let mut cumulative = Vec::with_capacity(parents.len());
    let mut total = 0.0;
    for parent in &parents {
      total += sizes[*parent] as f64;
      cumulative.push(total);
    }
    for (index, session) in sessions.iter_mut().enumerate() {
      if !session.has_task || parents.contains(&index) || rng.unit() >= CHILD_SHARE {
        continue;
      }
      let point = rng.unit() * total;
      let slot = cumulative.partition_point(|bound| *bound <= point).min(parents.len() - 1);
      session.parent = Some(parents[slot]);
    }
  }

  /// Draws each worker's task type: from `WORKTREE_WORKER_TYPES` for a worker kept for its worktree, else from
  /// `WORKER_TYPES`. A worker kept for its sequence ref stays an implement task.
  fn type_workers(sessions: &mut [SessionPlan], rng: &mut Rng) {
    for session in sessions.iter_mut().filter(|session| session.profile == TaskProfile::Worker) {
      let types: &[(Option<TaskType>, f64)] = match session.reason {
        Some(KeepReason::SequenceRef) => continue,
        Some(KeepReason::WorktreeExists) => &WORKTREE_WORKER_TYPES,
        _ => &WORKER_TYPES,
      };
      let total: f64 = types.iter().map(|(_, weight)| weight).sum();
      let point = rng.unit() * total;
      let mut bound = 0.0;
      for (task_type, weight) in types.iter().copied() {
        bound += weight;
        if point < bound {
          session.task_type = task_type;
          break;
        }
      }
    }
  }

  /// Plans the `task_closed` fact of every session that is not open. A worker whose last run finished closes on that
  /// run's automatic request and reports to its parent with odds `AUTO_CLOSE_SHARE`; the user archived the other
  /// workers, with no report. The user archived a manager, and an archived manager reports to its parent.
  fn plan_closes(sessions: &mut [SessionPlan], rng: &mut Rng) {
    for session in sessions.iter_mut().filter(|session| session.reason != Some(KeepReason::StateOpen)) {
      let auto_odds = rng.unit();
      let summary = text::phrase(rng);
      let archived_id = plan_id(rng);
      let last_finished = session.runs.last().filter(|run| run.end == RunEnd::Finished && run.kind == RunKind::Work);
      session.close = Some(match (session.profile, last_finished) {
        (TaskProfile::Worker, Some(run)) if auto_odds < AUTO_CLOSE_SHARE => ClosePlan {
          request_id: format!("auto:{}", run.id),
          actor: ACTOR_SYSTEM,
          outcome: "completed",
          summary,
          report_to: session.parent,
        },
        (TaskProfile::Worker, _) => {
          ClosePlan { request_id: archived_id, actor: ACTOR_USER, outcome: "archived", summary, report_to: None }
        }
        (TaskProfile::Manager, _) => ClosePlan {
          request_id: archived_id,
          actor: ACTOR_USER,
          outcome: "archived",
          summary,
          report_to: session.parent,
        },
      });
    }
  }

  /// The child reports each parent's log holds, by parent index, in child order.
  fn reports(sessions: &[SessionPlan]) -> Vec<Vec<ReportPlan>> {
    let mut reports: Vec<Vec<ReportPlan>> = sessions.iter().map(|_| Vec::new()).collect();
    for child in sessions {
      let Some(close) = &child.close else { continue };
      let Some(parent) = close.report_to else { continue };
      let close_event_id = control_events::stable_close_event_id(&child.id, &close.request_id);
      reports[parent].push(ReportPlan {
        id: control_events::stable_child_report_id(&child.id, &close_event_id, &sessions[parent].id),
        child_id: child.id.clone(),
        close_event_id,
        outcome: close.outcome,
        summary: close.summary,
      });
    }
    reports
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

  /// One flag per session. It is set for each session whose reason `forced` holds, and for a seeded draw of the
  /// rest. The draw brings the set count to `share` of the sessions.
  fn exact_flags(
    reasons: &[Option<KeepReason>],
    share: f64,
    forced: fn(Option<KeepReason>) -> bool,
    rng: &mut Rng,
  ) -> Vec<bool> {
    let mut flags: Vec<bool> = reasons.iter().map(|reason| forced(*reason)).collect();
    let mut rest: Vec<usize> = (0..flags.len()).filter(|index| !flags[*index]).collect();
    let wanted = ((share * flags.len() as f64).round() as usize).saturating_sub(flags.len() - rest.len());
    rng.shuffle(&mut rest);
    for index in rest.into_iter().take(wanted) {
      flags[index] = true;
    }
    flags
  }

  fn session(
    id: String,
    reason: Option<KeepReason>,
    size: u64,
    has_task: bool,
    has_runs: bool,
    rng: &mut Rng,
  ) -> SessionPlan {
    let profile = match reason {
      Some(KeepReason::StateOpen) => TaskProfile::Manager,
      Some(KeepReason::SequenceRef | KeepReason::WorktreeExists) => TaskProfile::Worker,
      Some(KeepReason::FileAfterMarker | KeepReason::RunUnended) | None if has_task && rng.unit() < WORKER_SHARE => {
        TaskProfile::Worker
      }
      Some(KeepReason::FileAfterMarker | KeepReason::RunUnended) | None => TaskProfile::Manager,
    };
    let marker = marker_time();
    let started_at = session::first_session_time() + seconds(rng.range(0, session::SESSION_START_SPREAD_SECONDS));
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
    let task_type = (profile == TaskProfile::Worker).then_some(TaskType::Implement);
    SessionPlan {
      id,
      reason,
      profile,
      has_task,
      task_type,
      parent: None,
      started_at,
      modified_at,
      runs,
      close: None,
      pending_triggers: 0,
      fired_triggers: 0,
      archived: false,
    }
  }

  /// The runs of one session, in start order. A sequence-ref session holds cron steps; an unended session's last run is
  /// repaired, with its finish fact and no `ended_at`; a worktree session's last work run keeps its worktree.
  fn runs(
    reason: Option<KeepReason>,
    profile: TaskProfile,
    size: u64,
    started_at: NaiveDateTime,
    modified_at: NaiveDateTime,
    rng: &mut Rng,
  ) -> Vec<RunPlan> {
    let count = match profile {
      TaskProfile::Manager => {
        run_count(&MANAGER_RUN_COUNT_KNOTS, rng.unit()).max(size.div_ceil(TURN_SPAN_BYTES) as usize)
      }
      TaskProfile::Worker => run_count(&WORKER_RUN_COUNT_KNOTS, rng.unit()),
    };
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
        let end = if reason == Some(KeepReason::RunUnended) && last { RunEnd::Repaired } else { RunEnd::Finished };
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
  UtcDateTime::parse(&session::stamp(moment)).expect("a generated stamp parses")
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
/// The log opens with `task_created`. Each run then takes an equal span of turns, and the last span ends with the
/// final user message. A parent's child reports open the spans of its finished runs. A finished run's `run_finished` fact
/// closes its span and confirms the span's inputs, its reports among them. A closed task ends with its planned
/// `task_closed`.
fn write_task_log(
  path: &Path,
  plan: &HomePlan,
  index: usize,
  target: u64,
  rng: &mut Rng,
  text: &SyntheticText,
  final_text: &str,
) -> Result<TaskLog> {
  let session = &plan.sessions[index];
  let reports = &plan.reports[index];
  let session_id = session.id.as_str();
  let parent_id = session.parent.map(|parent| plan.sessions[parent].id.as_str());
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
    parent_id,
    None,
    Some(&event_id),
    Some(created_at),
  ))?;
  let spans = session.runs.len().max(1);
  // Each child report lands in a span whose run finished, so that run's finish confirms it as an input.
  let finish_spans: Vec<usize> =
    (0..session.runs.len()).filter(|span| session.runs[*span].end.has_finish_fact()).collect();
  let report_span = |report: usize| finish_spans[report * finish_spans.len() / reports.len()];
  let mut run_inputs = Vec::with_capacity(session.runs.len());
  for span in 0..spans {
    let limit = target as i64 * (span as i64 + 1) / spans as i64;
    let mut inputs = Vec::new();
    for (_, report) in reports.iter().enumerate().filter(|(index, _)| report_span(*index) == span) {
      let reported_at = UtcDateTime::parse(&log.next_stamp()).expect("an event stamp parses");
      log.push(&control_events::build_control_event(
        event_types::CHILD_REPORT,
        ACTOR_SYSTEM,
        &report.child_id,
        Some(&report.id),
        Some(reported_at),
        [
          ("child_session_id".to_string(), Value::String(report.child_id.clone())),
          ("child_event_id".to_string(), Value::String(report.close_event_id.clone())),
          ("outcome".to_string(), json!(report.outcome)),
          ("summary".to_string(), json!(report.summary)),
          ("result_refs".to_string(), json!([])),
        ],
      ))?;
      inputs.push(report.id.clone());
    }
    log.write_turns(limit, rng, text, &mut |rng: &mut Rng, event: &mut Value| inputs.push(label_input(rng, event)))?;
    if span + 1 == spans {
      let mut last = session::final_event(final_text, &log.next_stamp());
      inputs.push(label_input(rng, &mut last));
      log.push(&last)?;
    }
    if let Some(run) = session.runs.get(span) {
      if run.end.has_finish_fact() {
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
  if let Some(close) = &session.close {
    let closed_at = UtcDateTime::parse(&log.next_stamp()).expect("an event stamp parses");
    log.push(&control_events::build_control_event(
      event_types::TASK_CLOSED,
      close.actor,
      session_id,
      Some(&control_events::stable_close_event_id(session_id, &close.request_id)),
      Some(closed_at),
      [
        ("request_id".to_string(), json!(close.request_id)),
        ("outcome".to_string(), json!(close.outcome)),
        ("summary".to_string(), json!(close.summary)),
        ("result_refs".to_string(), json!([])),
        ("run_ids".to_string(), json!(session.runs.iter().map(|run| run.id.as_str()).collect::<Vec<_>>())),
        (
          "report_to".to_string(),
          close.report_to.map_or(Value::Null, |parent| Value::String(plan.sessions[parent].id.clone())),
        ),
      ],
    ))?;
  }
  let (bytes, _) = log.finish()?;
  Ok(TaskLog { bytes, run_inputs })
}

/// Moves the whole lines that open a log's first `ARCHIVE_FRACTION` of `bytes` into the session's archive file, and
/// returns the archived events and the bytes the live log keeps. The archive holds the log's first events, as a store
/// archive does, and the live log keeps the rest.
fn archive_prefix(session_dir: &Path, bytes: u64) -> Result<(usize, u64)> {
  let log_path = layout::chat_events_path(session_dir);
  let text = fs::read(&log_path).with_context(|| format!("cannot read {}", log_path.display()))?;
  let target = (bytes as f64 * ARCHIVE_FRACTION) as usize;
  let cut = text[target..].iter().position(|byte| *byte == b'\n').map_or(text.len(), |offset| target + offset + 1);
  let archive_dir = layout::archives_dir(session_dir);
  fs::create_dir_all(&archive_dir).with_context(|| format!("cannot create {}", archive_dir.display()))?;
  let archive_path = archive_dir.join(ARCHIVE_FILE_NAME);
  fs::write(&archive_path, &text[..cut]).with_context(|| format!("cannot write {}", archive_path.display()))?;
  fs::write(&log_path, &text[cut..]).with_context(|| format!("cannot write {}", log_path.display()))?;
  let archived = text[..cut].iter().filter(|byte| **byte == b'\n').count();
  Ok((archived, (text.len() - cut) as u64))
}

/// One node's `metadata.json`: the default session fields, then the task-tree fields.
#[derive(Serialize)]
struct NodeMetadata {
  #[serde(flatten)]
  session: SessionMetadata,
  task_parent_id: Option<String>,
  task: Option<TaskSpec>,
  /// The count of events the archive files hold, as the store's `archive_offset`; zero for a session with no archive.
  archive_offset: usize,
}

fn task_spec(profile: TaskProfile, task_type: Option<TaskType>, rng: &mut Rng) -> TaskSpec {
  match profile {
    TaskProfile::Manager => TaskSpec { goal: text::phrase(rng).to_string(), ..TaskSpec::default() },
    TaskProfile::Worker => TaskSpec {
      goal: text::phrase(rng).to_string(),
      repo_path: Some(LAB_REPO.to_string()),
      base_branch: Some("main".to_string()),
      task_type,
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
///
/// The marker names the plan's open sessions, as a server's pass writes the sessions it left open. A build without that
/// key reads each state itself. `scripts/release/trial.sh --seed-home` copies the file into a trial home's
/// `state/boot_recovery.json`, where the walk reads it.
fn write_marker_seed(home: &Path, plan: &HomePlan) -> Result<()> {
  let open: Vec<&str> =
    plan.sessions.iter().filter(|session| session.close.is_none()).map(|session| session.id.as_str()).collect();
  let mut payload = json!({"pass_started_at": utc(marker_time()).to_isoformat_text()});
  if !open.is_empty() {
    payload["open_sessions"] = json!(open);
  }
  let path = home.join(BOOT_RECOVERY_FILE_NAME);
  fs::write(&path, text_repr::json_dumps(&payload, &DEFAULT_JSON))
    .with_context(|| format!("cannot write {}", path.display()))
}

/// Writes a task-tree home of `sessions` sessions for `seed` into `staging`, whose final path is `home`. Returns the
/// total event-log bytes.
pub(crate) fn write_home(staging: &Path, home: &Path, sessions: usize, seed: u64) -> Result<u64> {
  let (mut rng, text, sizes) = session::planned_home(sessions, seed);
  let plan = HomePlan::new(&sizes, seed);
  let worktrees = home.join(WORKTREES_DIR);
  fs::create_dir(staging.join(WORKTREES_DIR)).context("cannot create the worktree directory")?;
  let mut manifest_sessions = Vec::with_capacity(sessions);
  let mut total = 0;
  for (index, (session, size)) in plan.sessions.iter().zip(&sizes).enumerate() {
    let id = session.id.clone();
    let session_dir = staging.join(&id);
    fs::create_dir_all(layout::data_dir(&session_dir))
      .with_context(|| format!("cannot create {}", session_dir.display()))?;
    let final_text = format!("Checkpoint {id}: {}", text::phrase(&mut rng));
    let log_path = layout::chat_events_path(&session_dir);
    let log = write_task_log(&log_path, &plan, index, *size, &mut rng, &text, &final_text)?;
    let (archive_offset, live_bytes) =
      if session.archived { archive_prefix(&session_dir, log.bytes)? } else { (0, log.bytes) };
    let metadata = NodeMetadata {
      session: SessionMetadata {
        id: id.clone(),
        name: text::title(&mut rng),
        status: "active",
        profile: session.profile.as_str(),
        backend: "mock-backend",
        created_at: session::stamp(session.started_at),
        updated_at: session::stamp(session.modified_at),
        schema_version: 2,
        has_unread: false,
        starred: false,
        task: None,
      },
      task_parent_id: session.parent.map(|parent| plan.sessions[parent].id.clone()),
      task: session.has_task.then(|| task_spec(session.profile, session.task_type, &mut rng)),
      archive_offset,
    };
    session::write_json(&layout::metadata_path(&session_dir), &metadata, b"  ")?;
    for (run, inputs) in session.runs.iter().zip(log.run_inputs) {
      let run_dir = layout::runs_dir(&session_dir).join(&run.id);
      fs::create_dir_all(&run_dir).with_context(|| format!("cannot create {}", run_dir.display()))?;
      let record = run_records::run_record_text(&run_record(&id, run, inputs, &worktrees));
      let path = run_dir.join(run_records::RUN_METADATA_NAME);
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
    manifest_sessions.push(ManifestSession { id, event_log_bytes: live_bytes, last_message: final_text });
    total += log.bytes;
  }
  write_imported_chat(staging)?;
  write_marker_seed(staging, &plan)?;
  write_live_runs(staging, &plan)?;
  let manifest = Manifest {
    seed,
    session_count: sessions,
    event_log_bytes: total,
    sessions: manifest_sessions,
    shape: Shape::TaskTree,
  };
  session::write_json(&staging.join(MANIFEST_NAME), &manifest, b" ")?;
  Ok(total)
}

/// The id of the imported chat that `write_home` adds after the planned sessions. It has no task, no run directory and
/// no task boundary in its log, so its node pass fails at every boot (`has_no_task_node`), as an imported chat does in
/// production. The manifest does not list it, so `reset` leaves it as generated.
const IMPORTED_CHAT_ID: &str = "6f0c3b8e-2d9a-4e71-8b5c-1a7e9d4f0b26";

fn write_imported_chat(staging: &Path) -> Result<()> {
  let session_dir = staging.join(IMPORTED_CHAT_ID);
  fs::create_dir_all(layout::data_dir(&session_dir))
    .with_context(|| format!("cannot create {}", session_dir.display()))?;
  let stamp = session::stamp(marker_time());
  let metadata = NodeMetadata {
    session: SessionMetadata {
      id: IMPORTED_CHAT_ID.to_string(),
      name: "Imported chat".to_string(),
      status: "active",
      profile: "manager",
      backend: "mock-backend",
      created_at: stamp.clone(),
      updated_at: stamp,
      schema_version: 2,
      has_unread: false,
      starred: false,
      task: None,
    },
    task_parent_id: None,
    task: None,
    archive_offset: 0,
  };
  session::write_json(&layout::metadata_path(&session_dir), &metadata, b"  ")?;
  let path = layout::chat_events_path(&session_dir);
  let line = json!({"id": "imported-1", "type": "user", "content": "hello from an imported chat"});
  fs::write(&path, format!("{line}\n")).with_context(|| format!("cannot write {}", path.display()))
}

/// Writes `live-runs.txt` into `staging`: one `<session> <run>` line for each live run of `plan`.
fn write_live_runs(staging: &Path, plan: &HomePlan) -> Result<()> {
  let text: String = plan
    .sessions
    .iter()
    .flat_map(|session| {
      session.runs.iter().filter(|run| run.end == RunEnd::Live).map(move |run| format!("{} {}\n", session.id, run.id))
    })
    .collect();
  let path = staging.join(LIVE_RUNS_NAME);
  fs::write(&path, text).with_context(|| format!("cannot write {}", path.display()))
}

/// Returns the task-tree home `home` to its generated state: the marker, every session's file times, and the worktrees
/// the run records name.
///
/// A server start rewrites the marker, and a lab start can touch file times or remove a worktree, so each timed start
/// begins with a reset. An event log whose size differs from the manifest fails the reset: a server wrote into this
/// home, and only a new `generate` restores it. `allow_growth` accepts a log longer than its generated size, which is
/// the append `append_growth` makes to set up a steady state; a log shorter than its generated size fails either way.
pub fn reset(home: &Path, allow_growth: bool) -> Result<()> {
  let manifest = Manifest::read(home)?;
  if manifest.shape != Shape::TaskTree {
    bail!("{} holds a {:?} home; only a task-tree home has a generated state to reset", home.display(), manifest.shape);
  }
  let (_, _, sizes) = session::planned_home(manifest.session_count, manifest.seed);
  let plan = HomePlan::new(&sizes, manifest.seed);
  let worktrees = fs::canonicalize(home)?.join(WORKTREES_DIR);
  let mut restored_worktrees = 0;
  for (session, listed) in plan.sessions.iter().zip(&manifest.sessions) {
    if session.id != listed.id {
      bail!(
        "{} lists session {} where this plan has {}: generate the home again with this mock-home",
        home.display(),
        listed.id,
        session.id
      );
    }
    let session_dir = home.join(&listed.id);
    let log_path = layout::chat_events_path(&session_dir);
    let bytes = fs::metadata(&log_path).with_context(|| format!("cannot stat {}", log_path.display()))?.len();
    if bytes < listed.event_log_bytes || (bytes > listed.event_log_bytes && !allow_growth) {
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
  write_marker_seed(home, &plan)?;
  println!(
    "reset {} sessions in {}: marker {}, {restored_worktrees} worktrees restored",
    manifest.session_count,
    home.display(),
    utc(marker_time()).to_isoformat_text()
  );
  Ok(())
}

/// Append `bytes` of chat-content events to the task-tree home's full-pass logs of 1 MB or more, in proportion to
/// each log's size, and print the sessions and bytes appended.
///
/// This is the append a steady-state measurement needs: production's 122 scanned logs of 1 MB or more grew by
/// 17,653,088 bytes between two boots, and a boot after this append folds that tail onto the fact checkpoints the
/// previous start wrote. The line is a chat-content event, so a scan keeps only its id and type, as it keeps a
/// production chat line's.
pub fn append_growth(home: &Path, bytes: u64) -> Result<()> {
  let manifest = Manifest::read(home)?;
  if manifest.shape != Shape::TaskTree {
    bail!("{} holds a {:?} home; only a task-tree home has a generated state", home.display(), manifest.shape);
  }
  let (_, _, sizes) = session::planned_home(manifest.session_count, manifest.seed);
  let plan = HomePlan::new(&sizes, manifest.seed);
  let kept: Vec<(usize, u64)> = plan
    .sessions
    .iter()
    .enumerate()
    .filter(|(index, session)| session.reason.is_some() && sizes[*index] >= 1_000_000)
    .map(|(index, _)| (index, sizes[index]))
    .collect();
  if kept.is_empty() {
    bail!("{} has no full-pass log of 1 MB or more", home.display());
  }
  let total: u64 = kept.iter().map(|(_, size)| *size).sum();
  let mut written = 0u64;
  let mut sessions = 0usize;
  for (index, size) in kept {
    let share = (u128::from(bytes) * u128::from(size) / u128::from(total)) as u64;
    if share < 1 {
      continue;
    }
    let id = format!("growth-{}", plan.sessions[index].id);
    let prefix = format!("{{\"id\": \"{id}\", \"type\": \"assistant\", \"content\": \"");
    let suffix = "\"}\n";
    let overhead = (prefix.len() + suffix.len()) as u64;
    if share <= overhead {
      continue;
    }
    let mut line = prefix;
    line.push_str(&"x".repeat((share - overhead) as usize));
    line.push_str(suffix);
    let path = layout::chat_events_path(&home.join(&plan.sessions[index].id));
    let mut file = File::options()
      .append(true)
      .open(&path)
      .with_context(|| format!("cannot open {} to append its growth", path.display()))?;
    std::io::Write::write_all(&mut file, line.as_bytes())
      .with_context(|| format!("cannot append to {}", path.display()))?;
    written += line.len() as u64;
    sessions += 1;
  }
  println!("appended {written} bytes to {sessions} full-pass logs of {}", home.display());
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The reference start's follow-up totals, scaled to `sessions`.
  fn scaled(reference: f64, sessions: usize) -> f64 {
    reference * sessions as f64 / REFERENCE_NODES
  }

  /// The plan deals every keep reason at its scaled reference count, so a lab start's full-pass distribution lands
  /// within the A1 range of the reference start. Its full passes replay about the reference start's run follow-ups:
  /// one per finished manager turn and work run, the workers' work runs among them.
  #[test]
  fn a_scaled_plan_deals_each_keep_reason_its_scaled_count() {
    let sessions = 1_200;
    let (_, _, sizes) = session::planned_home(sessions, 1);
    let plan = HomePlan::new(&sizes, 1);
    for (reason, reference) in REFERENCE_KEEPS {
      let dealt = plan.sessions.iter().filter(|session| session.reason == Some(reason)).count();
      assert_eq!(dealt, scaled(reference, sessions).round() as usize, "{reason:?}");
    }
    let with_runs = plan.sessions.iter().filter(|session| !session.runs.is_empty()).count();
    assert_eq!(with_runs, (RUNS_SHARE * sessions as f64).round() as usize);
    let with_task = plan.sessions.iter().filter(|session| session.has_task).count();
    assert_eq!(with_task, (TASK_SHARE * sessions as f64).round() as usize);

    let followups = plan
      .sessions
      .iter()
      .filter(|session| session.reason.is_some_and(|reason| reason != KeepReason::SequenceRef))
      .flat_map(|session| &session.runs)
      .filter(|run| run.end == RunEnd::Finished)
      .count() as f64;
    // The 09:06Z start's walk counted 7,938 follow-ups over 8,930 nodes. The run knots spread the finished runs over
    // this home's kept nodes, and the sum lands within a quarter of that reference.
    assert!((followups / scaled(7938.0, sessions) - 1.0).abs() < 0.25, "{followups} follow-ups");

    // A report that no finished parent run confirms is a pending input, and a lab start would dispatch it.
    let parents: Vec<usize> = plan.sessions.iter().filter_map(|session| session.parent).collect();
    assert!(parents.iter().all(|parent| plan.sessions[*parent].parent.is_none()), "every parent is a root");
    for (parent, _) in plan.reports.iter().enumerate().filter(|(_, reports)| !reports.is_empty()) {
      assert!(parents.contains(&parent), "only a parent holds reports");
      assert!(plan.sessions[parent].runs.iter().any(|run| run.end == RunEnd::Finished), "a report's run finished");
    }
    assert!(plan.sessions.iter().all(|session| session.task_type != Some(TaskType::ColdRead)), "no cold read");

    // An archive takes a large parent's log: each archived session is a parent in the band. A plan whose logs
    // all sit in the band archives every parent a child lands under, so a small plan deals several archives.
    let archived: Vec<usize> = (0..sessions).filter(|index| plan.sessions[*index].archived).collect();
    for index in archived {
      let band = ARCHIVE_MIN_BYTES..=ARCHIVE_MAX_BYTES;
      assert!(parents.contains(&index) && band.contains(&sizes[index]), "session {index}");
    }
    let band_sizes = vec![30_000_000_u64; 500];
    let band_plan = HomePlan::new(&band_sizes, 1);
    let band_archived = band_plan.sessions.iter().filter(|session| session.archived).count();
    assert!(band_archived >= 5, "{band_archived} archived sessions");
  }
}
