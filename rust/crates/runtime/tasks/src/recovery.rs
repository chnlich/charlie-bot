//! The startup reconcile of task-tree nodes, the end-landing retry, and the let-go step at shutdown.
//!
//! A node pass runs its steps in this order: an interrupted prompt edit lands its fact; stop requests win; a
//! launched run re-attaches (live) or drains (dead); the follow-ups of terminal runs replay once; undelivered reports
//! are recovered; pending inputs dispatch. A replay step whose owning task has not landed fails the pass with an error
//! that names the owner; the call sites are listed in `README.md`.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(feature = "test-hooks")]
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail};
use charliebot_agents::runs::run_alive_probe;
use charliebot_agents::transport::{identity, recovery as transport_recovery};
use charliebot_core::config::BackendOption;
use charliebot_core::home::boot_recovery_marker_path;
use charliebot_core::parallel_walk;
use charliebot_registry::context::{FeatureContext, StartupStop};
use charliebot_registry::sequence::{
  SequenceCompletion, SequenceController, SequenceDispatch, SequenceExecutor, SequenceSessionEvents,
  SequenceTreeRuntime, StoreSequenceTree,
};
use charliebot_store::ChatEvent;
use charliebot_store::metadata::{SessionMetadata, TaskProfile, TaskType, UtcDateTime};
use charliebot_store::run_records::{
  RAW_LOG_NAME, RunKind, RunOutcome, RunOutcomeKind, RunRecord, run_not_found_in_task_text,
};
use charliebot_store::{HistoryWork, PromptScope, Store, StoreError, StoreResult};
use futures_util::future::BoxFuture;
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::oneshot;

use crate::completion::TaskCompletion;
use crate::dispatch::{self, Dispatcher};
use crate::execution::{TaskExecutionAdapter, is_out_of_space_error};
use crate::master_queue::{MasterQueue, SessionCallbacks};
use crate::master_run;
use crate::master_state::{MasterResumeRecord, MasterResumeRequest, ResumeLiveness, TaskRunBinding, TaskSpawnCallback};
use crate::skip_proof::{SkipProof, SkipProofRow, SkipProofSource};
use crate::spawner_prompt;

pub use crate::execution::{RUN_END_LANDING_RETRY_INTERVAL, closed_task_stop_request_id};
const DRIVEN_RUN_RELEASE_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// A durable file whose mtime is no earlier than the marker minus this slack keeps its node.
/// The kernel stamps `st_mtime` from its coarse clock,
/// which can lag the fine-clock pass start by about two ticks (~2 ms at 500 Hz), so the slack must exceed that lag.
const MARKER_MTIME_SLACK_SECONDS: f64 = 1.0;

/// The boot marker: the start of the last pass that wrote it and the nodes whose pass failed in that pass. Every node
/// outside `failed_sessions` was clean at or after `pass_started_at`, so the next boot may serve its skip verdict from
/// the startup-only skip proof; a node the proof cannot serve keeps the marker's file-age rule.
struct BootMarker {
  pass_started_at: UtcDateTime,
  failed_sessions: BTreeSet<String>,
}

/// The last clean pass's marker, or None when the marker is absent or unreadable. None makes the pass check every node
/// in full. Other read errors, such as a permission error, propagate.
fn read_boot_recovery_marker(home: &std::path::Path) -> anyhow::Result<Option<BootMarker>> {
  let path = boot_recovery_marker_path(home);
  let raw = match std::fs::read(&path) {
    Ok(raw) => raw,
    Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
    Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
  };
  match parse_boot_recovery_marker(&raw) {
    Ok(marker) => Ok(Some(marker)),
    Err(error_type) => {
      tracing::error!(target: "charliebot_tasks", size = raw.len(), error_type, "boot_recovery_marker_unreadable");
      Ok(None)
    }
  }
}

/// The marker that a file's bytes hold. On failure, the exception name that the boot logs as `error_type`. The boot logs
/// only that name, so the parser's own message is not kept. A marker with no `failed_sessions` key is a clean pass's
/// marker, so the key's absence reads as no failed node.
fn parse_boot_recovery_marker(raw: &[u8]) -> Result<BootMarker, &'static str> {
  let text = std::str::from_utf8(raw).map_err(|_| "UnicodeDecodeError")?;
  let payload: Value = serde_json::from_str(text).map_err(|_| "JSONDecodeError")?;
  let Value::Object(fields) = payload else {
    return Err("TypeError");
  };
  let pass_started_at = match fields.get("pass_started_at") {
    None => return Err("KeyError"),
    Some(Value::String(pass_started_at)) => UtcDateTime::parse(pass_started_at).map_err(|_| "ValueError")?,
    Some(_) => return Err("TypeError"),
  };
  let failed_sessions = match fields.get("failed_sessions") {
    None => BTreeSet::new(),
    Some(Value::Array(items)) => items
      .iter()
      .map(|item| match item {
        Value::String(session_id) => Ok(session_id.clone()),
        _ => Err("TypeError"),
      })
      .collect::<Result<BTreeSet<String>, &'static str>>()?,
    Some(_) => return Err("TypeError"),
  };
  Ok(BootMarker { pass_started_at, failed_sessions })
}

/// Writes the marker. `failed_sessions` is written only when non-empty, so a pass that leaves no failed node writes the
/// marker it always has.
fn write_boot_recovery_marker(
  home: &std::path::Path,
  pass_started_at: UtcDateTime,
  failed_sessions: &BTreeSet<String>,
) -> anyhow::Result<()> {
  let path = boot_recovery_marker_path(home);
  let parent = path.parent().context("boot-recovery marker has no parent directory")?;
  std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
  let mut payload = serde_json::Map::new();
  payload.insert("pass_started_at".to_string(), Value::String(pass_started_at.to_isoformat_text()));
  if !failed_sessions.is_empty() {
    payload.insert("failed_sessions".to_string(), serde_json::json!(failed_sessions));
  }
  let text = charliebot_core::text_repr::json_dumps(&Value::Object(payload), &charliebot_core::text_repr::DEFAULT_JSON);
  charliebot_core::fsutil::atomic_write_text_synced(&path, &text)
    .with_context(|| format!("writing {}", path.display()))?;
  Ok(())
}

/// The boot marker's verdict on one node.
enum SkipCheck {
  /// The node takes its full pass for this keep reason.
  Keep(&'static str),
  /// The node skips its pass. The newest run's backend warms the display-backend map.
  Skip { newest_backend: Option<String> },
}

fn boot_skip_check(
  features: &FeatureContext,
  session_id: &str,
  meta: &SessionMetadata,
  marker: &BootMarker,
  source: Option<&SkipProofSource>,
) -> anyhow::Result<SkipCheck> {
  let marker_instant = marker.pass_started_at.into_inner();
  let marker_ns =
    charliebot_core::timefmt::file_time_ns(marker_instant.unix_timestamp(), i64::from(marker_instant.nanosecond()));
  let slack_ns = (MARKER_MTIME_SLACK_SECONDS as i128) * 1_000_000_000;
  // A node without a log or metadata file cannot be skipped: the check treats it as changed.
  let file_after_marker = match source {
    None => true,
    Some(source) => source.log_changed_since(marker_ns, slack_ns) || source.meta_changed_since(marker_ns, slack_ns),
  };

  // The state decides a keep before any file's age does.
  if features.store.task_state_fast(session_id)? == "open" {
    return Ok(SkipCheck::Keep("state_open"));
  }
  if file_after_marker {
    return Ok(SkipCheck::Keep("file_after_marker"));
  }

  let runs = features.store.list_run_records_sync(session_id)?;
  for run in &runs {
    if run.ended_at.is_none() {
      return Ok(SkipCheck::Keep("run_unended"));
    }
    if run.sequence_ref.is_some() {
      return Ok(SkipCheck::Keep("sequence_ref"));
    }
    if let Some(worktree_path) = run.worktree_path.as_deref().filter(|path| !path.is_empty()) {
      // A path that cannot be stat'ed counts as absent, so the pass continues past it. An error other than
      // not-found logs a warning, which keeps that error visible.
      match std::fs::symlink_metadata(worktree_path) {
        Ok(_) => return Ok(SkipCheck::Keep("worktree_exists")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(
          target: "charliebot_tasks",
          session_id,
          path = worktree_path,
          error = %error,
          "task_recovery_worktree_stat_failed"
        ),
      }
    }
  }

  if meta.task.as_ref().is_some_and(|task| task.task_type == Some(TaskType::ColdRead))
    && let Some(last_work_run) = runs.iter().rfind(|run| run.kind == RunKind::Work)
    && features.store.node_state(session_id)?.runs.terminal_outcome(&last_work_run.id)? == Some(RunOutcomeKind::Failed)
  {
    return Ok(SkipCheck::Keep("cold_read_failed"));
  }
  // The listing is in `(started_at, id)` order, so its last record is the session's newest run.
  Ok(SkipCheck::Skip { newest_backend: runs.last().and_then(|run| run.backend.clone()) })
}

/// The counters of one boot pass, logged with `task_tree_recovery_done`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PassCounters {
  pub nodes: u64,
  pub resumed: u64,
  pub drained: u64,
  pub followups: u64,
  /// The follow-up replays this pass served from the last completed pass instead of re-running them.
  pub replay_served: u64,
  pub full: u64,
  pub skipped: u64,
  pub marker: Option<String>,
  pub keep_reasons: BTreeMap<String, u64>,
}

/// Test hooks over the recovery state (feature `test-hooks`): a shorter end-landing retry interval, and read access to
/// the nodes with a live retry task and to the resume follows this process holds.
#[cfg(feature = "test-hooks")]
impl TaskExecutionAdapter {
  /// Count direct node-pass calls, including calls made by an end-landing retry.
  pub fn reconcile_node_calls(&self) -> usize {
    self.recovery.reconcile_node_calls.load(Ordering::SeqCst)
  }

  /// Start an end-landing retry for a node from an integration test.
  pub fn start_landing_retry_for_test(&self, session_id: &str) {
    self.start_run_end_landing_retry(session_id, None);
  }

  /// Replace the end-landing retry interval for every retry this adapter starts from now on.
  pub fn set_landing_retry_interval(&self, interval: Duration) {
    *self.recovery.landing_retry_interval.lock().expect("landing retry interval is not poisoned") = Some(interval);
  }

  /// The nodes that have a live end-landing retry task.
  pub fn landing_retry_nodes(&self) -> HashSet<String> {
    self.recovery.landing_retries.lock().expect("landing retry set is not poisoned").clone()
  }

  /// The (session, run) pairs whose resume follow this process holds.
  pub fn resume_follows(&self) -> HashSet<(String, String)> {
    self.recovery.resume_follows.lock().expect("resume follow set is not poisoned").clone()
  }
}

/// The adapter operations the recovery owns, as instance accessors: the launch and after-run paths reach them through
/// the adapter they hold, without naming this module.
impl TaskExecutionAdapter {
  /// Re-attach one launched run, or drain it when its process is dead. A manager turn re-enters the per-session queue;
  /// the follow is tracked and this call returns before it ends.
  pub async fn resume_run(
    &self,
    session_id: &str,
    run_id: &str,
    is_alive: Option<ResumeLiveness>,
  ) -> anyhow::Result<()> {
    let key = (session_id.to_string(), run_id.to_string());
    self.recovery.resume_follows.lock().expect("resume follow set is not poisoned").insert(key.clone());
    let result = resume_run_checked(self, session_id, run_id, is_alive).await;
    // Ownership passes to the manager follow's tracker only on success; every other exit releases the pair here.
    if !matches!(result, Ok(true)) {
      self.recovery.resume_follows.lock().expect("resume follow set is not poisoned").remove(&key);
    }
    result.map(|_| ())
  }

  /// Start the node's end-landing retry task; a second request for the node is a no-op.
  pub(crate) fn start_run_end_landing_retry(&self, session_id: &str, run_id: Option<&str>) {
    start_end_landing_retry(self.clone(), session_id, run_id)
  }

  /// The registered controller that owns a sequence kind, or `None` when no registered controller claims it (its
  /// package was deleted). A kind in `UNLANDED_SEQUENCE_KINDS` fails by name instead: its controller has not landed.
  pub(crate) fn owning_controller(
    &self,
    kind: &str,
    step: &str,
  ) -> anyhow::Result<Option<Arc<dyn SequenceController>>> {
    owning_controller(&self.features, kind, step)
  }

  /// The task-tree handle the sequence controllers use: the dispatcher, the completion owner, the store's session
  /// events, and this adapter as the launch executor.
  pub(crate) fn sequence_tree(&self) -> anyhow::Result<StoreSequenceTree> {
    sequence_tree(&self.features, self)
  }
}

/// The startup recovery step: a pass error is logged and the server starts anyway. The adapter is a wiring fact, so its
/// absence fails startup.
pub fn recovery_step(features: Arc<FeatureContext>) -> BoxFuture<'static, anyhow::Result<()>> {
  Box::pin(async move {
    let adapter = features.startup.require::<TaskExecutionAdapter>()?;
    #[cfg(debug_assertions)]
    wait_test_recovery_gate(&features).await?;
    let started = Instant::now();
    match reconcile_task_tree(&features, &adapter).await {
      Ok(counters) if !features.startup.stop().is_cancelled() => {
        tracing::info!(
          target: "charliebot_tasks",
          elapsed_ms = started.elapsed().as_millis() as u64,
          nodes = counters.nodes,
          resumed = counters.resumed,
          drained = counters.drained,
          followups = counters.followups,
          full = counters.full,
          skipped = counters.skipped,
          marker = ?counters.marker,
          keep_reasons = ?counters.keep_reasons,
          "task_tree_recovery_done"
        );
      }
      Ok(_) => {}
      Err(error) => {
        tracing::error!(target: "charliebot_tasks", error = %format!("{error:#}"), "task_tree_recovery_failed")
      }
    }
    Ok(())
  })
}

#[cfg(debug_assertions)]
async fn wait_test_recovery_gate(features: &FeatureContext) -> anyhow::Result<()> {
  const DELAY_ENV: &str = "CHARLIEBOT_TEST_TASK_RECOVERY_DELAY_MS";
  let Some(value) = std::env::var_os(DELAY_ENV) else {
    return Ok(());
  };
  let value = value.into_string().map_err(|_| anyhow!("{DELAY_ENV} is not Unicode"))?;
  let delay_ms = value.parse::<u64>().with_context(|| format!("parsing {DELAY_ENV}"))?;
  tracing::info!(target: "charliebot_tasks", delay_ms, "task_tree_recovery_test_gate");
  let stop = features.startup.stop();
  tokio::select! {
    _ = tokio::time::sleep(Duration::from_millis(delay_ms)) => {}
    _ = stop.cancelled() => {}
  }
  Ok(())
}

/// On SIGINT and bind failure, cancel each run launch and then each session consumer. A cancelled launch leaves its run
/// for the next boot (`TaskExecutionAdapter::let_go_launches`); each running turn detaches when its record is durable
/// and terminates otherwise. The launches go first so that a manager launch never reaches the stopped queue. SIGTERM
/// skips this step: the server restores the default disposition and raises SIGTERM again. A failure of the first half
/// still runs the second.
pub fn let_go_step(features: Arc<FeatureContext>) -> BoxFuture<'static, anyhow::Result<()>> {
  Box::pin(async move {
    let adapter = features.startup.require::<TaskExecutionAdapter>()?;
    let queue = features.startup.require::<MasterQueue>()?;
    let launches = adapter.let_go_launches().await;
    let consumers = queue.shutdown().await;
    match (launches, consumers) {
      (Ok(()), result) | (result, Ok(())) => result,
      (Err(launch_error), Err(consumer_error)) => {
        Err(consumer_error.context(format!("cancelling the run launches also failed: {launch_error:#}")))
      }
    }
  })
}

/// One session that reached the walk's full pass, with the metadata its absence proof reads.
struct FullPassSession {
  session_id: String,
  meta: SessionMetadata,
  /// The start of the last pass that completed every step for this node, when the boot marker names one. A run whose
  /// end is older had its follow-up applied by that pass, so this boot's replay serves it instead of re-running it.
  completed_before: Option<UtcDateTime>,
}

/// One session directory's verdict from the walk's skip check.
enum WalkEntry {
  /// No node: the directory, its metadata file or its metadata is missing.
  Absent,
  /// The boot marker or its skip proof skips the node's pass.
  Skipped { session_id: String, newest_backend: Option<String>, source: Option<SkipProofSource> },
  /// The node takes its full pass; the keep reason is absent when no marker was read.
  Kept { reason: Option<&'static str>, session: Box<FullPassSession> },
}

/// Reconcile every node this home owns, in sorted directory order (`task_recovery.reconcile_task_tree`). The sequence
/// controllers mark their interrupted state first.
///
/// A pass that runs to its end logs `task_tree_recovery_phases`, the milliseconds of each phase in run order: the
/// sequence controllers' reconcile, the session listing, the skip checks, the absence proofs, the fact scans with their
/// seals, the node passes and the marker write; the whole-log reads and run-record reads the node passes made; and
/// `recipient_fact_scans`, the report recipients outside the passes that the fact scans covered. Three fields split the
/// node passes by step ([`StepTotals`]): `node_pass_step_ms`, `node_pass_step_file_reads` and
/// `node_pass_step_file_read_bytes`. A run follow that an earlier pass started in the background can read while a later
/// pass runs; its reads count toward that later pass's running step.
///
/// A node pass serves most of its replay from the pass the marker names: a run that ended before that pass started
/// already had its follow-up applied there, so re-running it would land nothing. Only the session's newest run keeps
/// its full follow-up, which is the one place a close request that arrived later acts.
pub async fn reconcile_task_tree(
  features: &Arc<FeatureContext>,
  adapter: &TaskExecutionAdapter,
) -> anyhow::Result<PassCounters> {
  let mut phase_started = Instant::now();
  let mut phase_ms = || {
    let elapsed_ms = phase_started.elapsed().as_millis() as u64;
    phase_started = Instant::now();
    elapsed_ms
  };
  let stop = features.startup.stop();
  let cfg = features.config.get().context("reading the configuration for task recovery")?;
  let mut counters = PassCounters::default();
  if stop.is_cancelled() {
    return Ok(counters);
  }
  let sessions_dir = cfg.sessions_dir();
  if !sessions_dir.is_dir() {
    return Ok(counters);
  }
  let started_at = UtcDateTime::now();
  let marker = read_boot_recovery_marker(&cfg.charliebot_home)?;
  let marker_text = marker.as_ref().map(|marker| marker.pass_started_at.to_isoformat_text());
  // The last pass's skip verdicts, each valid only while its node's log and metadata keep the identities it recorded.
  let skip_proof = SkipProof::read(&cfg.charliebot_home);
  let mut failed_sessions = BTreeSet::new();
  let mut proven_skips: Vec<SkipProofRow> = Vec::new();

  let tree = sequence_tree(features, adapter)?;
  for controller in features.registry.sequence_controllers() {
    if stop.is_cancelled() {
      return Ok(counters);
    }
    controller.reconcile_interrupted(&tree).await?;
  }
  let sequence_reconcile_ms = phase_ms();

  let mut session_dirs = std::fs::read_dir(&sessions_dir)
    .with_context(|| format!("listing {}", sessions_dir.display()))?
    .map(|entry| entry.map(|entry| entry.path()))
    .collect::<io::Result<Vec<PathBuf>>>()
    .with_context(|| format!("listing {}", sessions_dir.display()))?;
  session_dirs.sort();
  let listing_ms = phase_ms();
  tracing::info!(target: "charliebot_tasks", sessions = session_dirs.len(), "task_tree_recovery_walk_started");
  // The skip checks only read, and no node pass has run yet, so the checks run in parallel and their verdicts are
  // applied in the walk's order: the first failing session in that order fails the walk.
  let check_features = Arc::clone(features);
  let (_, entries) = blocking_batch(session_dirs, None, &stop, move |session_dir| {
    examine_session_dir(&check_features, session_dir, marker.as_ref(), &skip_proof)
  })
  .await?;
  let Some(entries) = entries else {
    return Ok(counters);
  };
  let mut candidates: Vec<FullPassSession> = Vec::new();
  for entry in entries {
    match entry? {
      WalkEntry::Absent => {}
      WalkEntry::Skipped { session_id, newest_backend, source } => {
        counters.nodes += 1;
        if let Some(backend) = newest_backend.as_deref() {
          features.store.note_run_backend(&session_id, backend);
        }
        if let Some(source) = source {
          proven_skips.push(SkipProofRow {
            session_id: session_id.clone(),
            source,
            newest_backend: newest_backend.clone(),
          });
        }
        counters.skipped += 1;
      }
      WalkEntry::Kept { reason, session } => {
        counters.nodes += 1;
        if let Some(reason) = reason {
          *counters.keep_reasons.entry(reason.to_string()).or_default() += 1;
        }
        counters.full += 1;
        candidates.push(*session);
      }
    }
  }
  if stop.is_cancelled() {
    return Ok(counters);
  }
  let skip_checks_ms = phase_ms();
  // A session with no task boundary, no run directory and no prompt ref has nothing to reconcile: its pass can only
  // report the absence. `has_no_task_node` proves that from the raw event bytes, so the pass here reports the same
  // absence without folding the log. Each proof reads only its own session's files, so the whole batch proves in
  // parallel before the passes run in the walk's order. A probe that cannot read the session's files fails the node,
  // as a node pass that cannot read them does. Each proof also takes its log's size, the scan's cost below; a log that
  // cannot be stat'ed costs nothing there, and its scan reports the error.
  let proof_features = Arc::clone(features);
  let (candidates, proofs) = blocking_batch(candidates, None, &stop, move |candidate| {
    let _session_timer = SessionTimer { session_id: candidate.session_id.clone(), started: Instant::now() };
    let log_bytes = std::fs::metadata(proof_features.store.chat_events_path(&candidate.session_id))
      .map_or(0, |metadata| metadata.len());
    (proof_features.store.has_no_task_node(&candidate.meta), log_bytes)
  })
  .await?;
  let Some(proofs) = proofs else {
    return Ok(counters);
  };
  let (no_nodes, log_bytes): (Vec<_>, Vec<_>) = proofs.into_iter().unzip();
  let absence_proofs_ms = phase_ms();
  // Each node pass reads its node's facts, which a full read folds from the whole log, and lists its run records several
  // times. The fact scans and the run listings read only their own session's files, so they run in parallel here: each
  // pass starts from its scanned facts sealed to the log's identity and reads its runs from the held listing, and it
  // loads the log only where a step needs its chat content. A seal checks the log's identity, so a log that an earlier
  // pass appended to after its scan is read by its own pass; a run-record write drops its session's held listing. The
  // largest logs start first, so the phase ends close to its total work spread across the threads.
  let _run_lists = features.store.hold_run_lists();
  let pass_nodes: Vec<(String, bool)> = candidates
    .iter()
    .zip(&no_nodes)
    .map(|(candidate, no_node)| (candidate.session_id.clone(), matches!(no_node, Ok(false))))
    .collect();
  let scan_features = Arc::clone(features);
  let (_, scans) = blocking_batch(pass_nodes, Some(log_bytes), &stop, move |(session_id, takes_pass)| {
    takes_pass.then(|| {
      scan_features.store.hold_run_list(session_id)?;
      scan_features.store.scan_live_facts(session_id)
    })
  })
  .await?;
  let Some(scans) = scans else {
    return Ok(counters);
  };
  // Every scan is sealed before the first pass, so a pass that reads another node's facts answers from that node's
  // scan. A read that failed, or a scan that changed under its read, seals nothing. That node's pass then reads its
  // files itself and fails the node on an error that persists.
  let candidate_ids: HashSet<&str> = candidates.iter().map(|candidate| candidate.session_id.as_str()).collect();
  let mut recipients = BTreeSet::new();
  let mut fact_scan_bytes = 0u64;
  let mut fact_scan_checkpoint_bytes = 0u64;
  for (candidate, scan) in candidates.iter().zip(scans) {
    let sealed = scan.map(|scan| match scan? {
      Some(scan) => {
        fact_scan_bytes += scan.read_bytes();
        fact_scan_checkpoint_bytes += scan.checkpoint_bytes();
        let others = scan.report_recipients().filter(|recipient| !candidate_ids.contains(recipient));
        recipients.extend(others.map(str::to_string));
        Ok::<bool, charliebot_store::StoreError>(features.store.seal_scanned_facts(&candidate.session_id, &scan)?)
      }
      None => Ok(false),
    });
    if let Some(Err(error)) = sealed {
      tracing::warn!(target: "charliebot_tasks", session_id = %candidate.session_id, error = %error, "task_recovery_read_ahead_failed");
    }
  }
  // A pass checks each close report against its recipient's facts, so the recipients that no pass covers are scanned
  // and sealed here as well. A recipient that no scan seals is read whole by the pass that checks it, and that pass
  // meets any error there.
  let recipients: Vec<String> = recipients.into_iter().collect();
  let recipient_bytes = recipients
    .iter()
    .map(|recipient| std::fs::metadata(features.store.chat_events_path(recipient)).map_or(0, |metadata| metadata.len()))
    .collect();
  let recipient_features = Arc::clone(features);
  let (recipients, recipient_scans) = blocking_batch(recipients, Some(recipient_bytes), &stop, move |recipient| {
    recipient_features.store.scan_live_facts(recipient)
  })
  .await?;
  let Some(recipient_scans) = recipient_scans else {
    return Ok(counters);
  };
  for (recipient, scan) in recipients.iter().zip(recipient_scans) {
    let sealed = scan.and_then(|scan| {
      scan.map_or(Ok(false), |scan| {
        fact_scan_bytes += scan.read_bytes();
        fact_scan_checkpoint_bytes += scan.checkpoint_bytes();
        features.store.seal_scanned_facts(recipient, &scan)
      })
    });
    if let Err(error) = sealed {
      tracing::warn!(target: "charliebot_tasks", session_id = %recipient, error = %error, "task_recovery_recipient_scan_failed");
    }
  }
  let recipient_fact_scans = recipients.len();
  let fact_scans_ms = phase_ms();
  let file_reads_before_passes = features.store.history_work().file_reads;
  let run_record_reads_before_passes = features.store.run_record_read_count();
  let mut step_totals = StepTotals::default();
  for (candidate, no_node) in candidates.into_iter().zip(no_nodes) {
    if stop.is_cancelled() {
      return Ok(counters);
    }
    let session_id = candidate.session_id;
    let mut pass_timer = NodePassTimer::start(&features.store, session_id.clone());
    let result = match no_node {
      Ok(true) => Some(Err(anyhow::Error::new(charliebot_store::task_node_absence::not_a_task_tree_node(&session_id)))),
      Ok(false) => {
        // No session channel has a subscriber before the server binds, so the pass builds no aggregator for one.
        let node_pass = reconcile_node_inner(
          features,
          adapter,
          &session_id,
          &mut counters,
          None,
          candidate.completed_before,
          Some(&stop),
          &mut pass_timer.steps,
        );
        reconcile_node_until_stopped(&stop, charliebot_store::hold_aggregator_builds(node_pass)).await
      }
      Err(error) => Some(Err(anyhow::Error::new(error))),
    };
    let Some(result) = result else {
      return Ok(counters);
    };
    step_totals.add(&pass_timer.steps);
    if let Err(error) = result {
      tracing::error!(target: "charliebot_tasks", session_id = %session_id, error = %format!("{error:#}"), "task_recovery_node_failed");
      failed_sessions.insert(session_id.clone());
      // An out-of-space pass enters the end-landing retry: it re-runs the pass until a round survives.
      if is_out_of_space_error(error.as_ref()) {
        adapter.start_run_end_landing_retry(&session_id, None);
      }
      continue;
    }
  }
  let node_passes_ms = phase_ms();
  let node_pass_file_reads = features.store.history_work().file_reads - file_reads_before_passes;
  let node_pass_run_record_reads = features.store.run_record_read_count() - run_record_reads_before_passes;
  let (node_pass_step_ms, node_pass_step_file_reads, node_pass_step_file_read_bytes) = step_totals.texts();
  // The marker advances for every node that the pass completed. A failed node stays in `failed_sessions`, so the next
  // boot runs its full pass again while the other nodes keep the marker's rule or the proof's verdict.
  if !stop.is_cancelled() {
    write_boot_recovery_marker(&cfg.charliebot_home, started_at, &failed_sessions)?;
    if let Err(error) = crate::skip_proof::write(&cfg.charliebot_home, &proven_skips) {
      tracing::warn!(target: "charliebot_tasks", error = %error, "skip_proof_write_failed");
    }
  }
  let marker_ms = phase_ms();
  tracing::info!(
    target: "charliebot_tasks",
    sequence_reconcile_ms,
    listing_ms,
    skip_checks_ms,
    absence_proofs_ms,
    fact_scans_ms,
    node_passes_ms,
    marker_ms,
    node_pass_file_reads,
    node_pass_run_record_reads,
    recipient_fact_scans,
    fact_scan_bytes,
    fact_scan_checkpoint_bytes,
    node_pass_step_ms = %node_pass_step_ms,
    node_pass_step_file_reads = %node_pass_step_file_reads,
    node_pass_step_file_read_bytes = %node_pass_step_file_read_bytes,
    replay_served = counters.replay_served,
    "task_tree_recovery_phases"
  );
  counters.marker = marker_text;
  Ok(counters)
}

/// The skip check of one session directory: whether it holds a node, and whether the boot marker or its skip proof
/// skips that node.
fn examine_session_dir(
  features: &FeatureContext,
  session_dir: &std::path::Path,
  marker: Option<&BootMarker>,
  proof: &SkipProof,
) -> anyhow::Result<WalkEntry> {
  // The metadata file's stat answers both "this directory holds a node" and the proof's metadata identity, and the
  // log's stat completes the proof's identity. A served node therefore costs two stats, not four.
  let Some(meta_identity) = SkipProofSource::metadata_file(session_dir) else {
    return Ok(WalkEntry::Absent);
  };
  let session_id = session_dir
    .file_name()
    .and_then(|name| name.to_str())
    .with_context(|| format!("session directory name is not UTF-8: {}", session_dir.display()))?
    .to_string();
  let _session_timer = SessionTimer { session_id: session_id.clone(), started: Instant::now() };
  let source =
    SkipProofSource::read(session_dir, meta_identity).with_context(|| format!("stating {}", session_dir.display()))?;
  // A node whose last pass failed takes its full pass, whatever the marker or the proof says about its files.
  if let Some(marker) = marker {
    if marker.failed_sessions.contains(&session_id) {
      let Some(meta) = features.store.get_session_cached_or_read_sync(&session_id)? else {
        return Ok(WalkEntry::Absent);
      };
      return Ok(WalkEntry::Kept {
        reason: Some("failed_pass"),
        session: Box::new(FullPassSession { session_id, meta, completed_before: None }),
      });
    }
    // A row whose identities still hold answers the node's verdict without reading a tail, a run listing or a
    // worktree path, so the boot's skip-check cost follows the nodes that changed since the proof was written.
    if let Some(source) = source
      && let Some(newest_backend) = proof.served(&session_id, &source)
    {
      return Ok(WalkEntry::Skipped { session_id, newest_backend, source: Some(source) });
    }
  }
  let Some(meta) = features.store.get_session_cached_or_read_sync(&session_id)? else {
    return Ok(WalkEntry::Absent);
  };
  let reason = match marker {
    None => None,
    Some(marker) => match boot_skip_check(features, &session_id, &meta, marker, source.as_ref())? {
      SkipCheck::Keep(reason) => Some(reason),
      SkipCheck::Skip { newest_backend } => return Ok(WalkEntry::Skipped { session_id, newest_backend, source }),
    },
  };
  let completed_before = marker.map(|marker| marker.pass_started_at);
  Ok(WalkEntry::Kept { reason, session: Box::new(FullPassSession { session_id, meta, completed_before }) })
}

/// [`parallel_walk::map_until`] on the blocking pool, ended by the startup stop; the items come back with the results.
/// With `costs`, the items are claimed costliest first ([`parallel_walk::map_until_costliest_first`]).
///
/// A batch reads for seconds on a large home. Run in place, it would hold one of the async runtime's threads for that
/// long, and the stop request arrives through a runtime task: on a host with one runtime thread, the stop would wait for
/// the batch to end. A batch below [`parallel_walk::MIN_ITEMS`] runs on the calling thread, as the walk's reads did
/// before they were batched.
async fn blocking_batch<T, R>(
  items: Vec<T>,
  costs: Option<Vec<u64>>,
  stop: &StartupStop,
  job: impl Fn(&T) -> R + Send + Sync + 'static,
) -> anyhow::Result<(Vec<T>, Option<Vec<R>>)>
where
  T: Send + Sync + 'static,
  R: Send + 'static,
{
  let map = move |items: &[T], stop: &StartupStop| match &costs {
    Some(costs) => parallel_walk::map_until_costliest_first(items, costs, &|| stop.is_cancelled(), &job),
    None => parallel_walk::map_until(items, &|| stop.is_cancelled(), &job),
  };
  if items.len() < parallel_walk::MIN_ITEMS {
    let results = map(&items, stop);
    return Ok((items, results));
  }
  let stop = stop.clone();
  tokio::task::spawn_blocking(move || {
    let results = map(&items, &stop);
    (items, results)
  })
  .await
  .context("a startup walk batch task failed")
}

async fn reconcile_node_until_stopped<T>(stop: &StartupStop, node_pass: impl Future<Output = T>) -> Option<T> {
  tokio::select! {
    result = node_pass => Some(result),
    _ = stop.cancelled() => None,
  }
}

/// The boot-walk step time over which a session logs, so a slow boot names the session it waited in without growing
/// the log on a healthy one.
const SLOW_RECOVERY_STEP_MS: u64 = 200;

/// Logs one boot-walk step of one session that took [`SLOW_RECOVERY_STEP_MS`] or more. One line per slow step: the
/// log never grows with the number of events a healthy boot folds.
fn log_slow_recovery_step(session_id: &str, step: &str, started: Instant) {
  let elapsed_ms = started.elapsed().as_millis() as u64;
  if elapsed_ms >= SLOW_RECOVERY_STEP_MS {
    tracing::info!(target: "charliebot_tasks", session_id, step, elapsed_ms, "task_recovery_step_slow");
  }
}

/// Times one session's skip check or absence proof and logs it at [`SLOW_RECOVERY_STEP_MS`] or more, so a slow boot
/// names each session it waited in. A node pass logs the same line through [`NodePassTimer`].
struct SessionTimer {
  session_id: String,
  started: Instant,
}

impl Drop for SessionTimer {
  fn drop(&mut self) {
    let elapsed_ms = self.started.elapsed().as_millis() as u64;
    if elapsed_ms >= SLOW_RECOVERY_STEP_MS {
      tracing::info!(target: "charliebot_tasks", session_id = %self.session_id, elapsed_ms, "task_recovery_session_slow");
    }
  }
}

/// What one step cost: its elapsed time, and the whole-log reads the store made while it ran and their bytes. The time
/// keeps its full precision, so a sum over many sub-millisecond steps loses nothing to rounding.
#[derive(Clone, Copy, Default)]
struct StepCost {
  elapsed: Duration,
  file_reads: u64,
  file_read_bytes: u64,
}

impl StepCost {
  fn add(&mut self, other: StepCost) {
    self.elapsed += other.elapsed;
    self.file_reads += other.file_reads;
    self.file_read_bytes += other.file_read_bytes;
  }
}

/// The steps one node pass ran, in run order: each step's name and [`StepCost`]. A step runs from the end of the step
/// before it, so the steps cover the pass without gaps. A name is the store or adapter call the step makes. The read
/// counts come from the store's in-memory counters, so the record makes no file call.
struct PassSteps<'a> {
  store: &'a Store,
  step_started: Instant,
  reads_at_step_start: HistoryWork,
  steps: Vec<(&'static str, StepCost)>,
}

impl<'a> PassSteps<'a> {
  fn start(store: &'a Store) -> Self {
    PassSteps { store, step_started: Instant::now(), reads_at_step_start: store.history_work(), steps: Vec::new() }
  }

  /// Ends the running step as `name`. A step that repeats the one before it adds to that entry, so a pass over many
  /// runs keeps a short line.
  fn end(&mut self, name: &'static str) {
    let now = Instant::now();
    let reads = self.store.history_work();
    let cost = StepCost {
      elapsed: now.duration_since(self.step_started),
      file_reads: reads.file_reads - self.reads_at_step_start.file_reads,
      file_read_bytes: reads.file_read_bytes - self.reads_at_step_start.file_read_bytes,
    };
    self.step_started = now;
    self.reads_at_step_start = reads;
    match self.steps.last_mut() {
      Some((last, total)) if *last == name => total.add(cost),
      _ => self.steps.push((name, cost)),
    }
  }

  /// The steps as `name=ms` pairs joined by spaces.
  fn text(&self) -> String {
    self.steps.iter().map(|(name, cost)| format!("{name}={}", cost.elapsed.as_millis())).collect::<Vec<_>>().join(" ")
  }
}

/// The [`StepCost`] of each step name summed over every node pass of the walk, so the phase line names where all the
/// passes spent their time and reads, the passes under [`SLOW_RECOVERY_STEP_MS`] included.
#[derive(Default)]
struct StepTotals(Vec<(&'static str, StepCost)>);

impl StepTotals {
  fn add(&mut self, pass: &PassSteps<'_>) {
    for (name, cost) in &pass.steps {
      match self.0.iter_mut().find(|(total_name, _)| total_name == name) {
        Some((_, total)) => total.add(*cost),
        None => self.0.push((name, *cost)),
      }
    }
  }

  /// The totals as three texts of `name=value` pairs, one each for the ms, the whole-log reads and their bytes. Every
  /// text lists every step, in descending order of ms.
  fn texts(&self) -> (String, String, String) {
    let mut totals = self.0.clone();
    totals.sort_by_key(|(_, cost)| std::cmp::Reverse(cost.elapsed));
    let text = |value: fn(&StepCost) -> u64| {
      totals.iter().map(|(name, cost)| format!("{name}={}", value(cost))).collect::<Vec<_>>().join(" ")
    };
    (text(|cost| cost.elapsed.as_millis() as u64), text(|cost| cost.file_reads), text(|cost| cost.file_read_bytes))
  }
}

/// Times one boot-walk node pass and, at [`SLOW_RECOVERY_STEP_MS`] or more, logs `task_recovery_session_slow` with:
/// - `session_id` and `elapsed_ms`, as [`SessionTimer`] logs them;
/// - `step_ms`: the pass's [`PassSteps`];
/// - `file_reads` and `file_read_bytes`: the whole-log reads that the store made while the pass ran.
///
/// The walk runs one node pass at a time, so the store's read counters change only for that pass and for any run
/// follow an earlier pass started. The timer keeps its record in memory and makes no file call.
struct NodePassTimer<'a> {
  store: &'a Store,
  session_id: String,
  started: Instant,
  reads_before: HistoryWork,
  steps: PassSteps<'a>,
}

impl<'a> NodePassTimer<'a> {
  fn start(store: &'a Store, session_id: String) -> Self {
    let reads_before = store.history_work();
    NodePassTimer { store, session_id, started: Instant::now(), reads_before, steps: PassSteps::start(store) }
  }
}

impl Drop for NodePassTimer<'_> {
  fn drop(&mut self) {
    let elapsed_ms = self.started.elapsed().as_millis() as u64;
    if elapsed_ms >= SLOW_RECOVERY_STEP_MS {
      let reads = self.store.history_work();
      tracing::info!(
        target: "charliebot_tasks",
        session_id = %self.session_id,
        elapsed_ms,
        step_ms = %self.steps.text(),
        file_reads = reads.file_reads - self.reads_before.file_reads,
        file_read_bytes = reads.file_read_bytes - self.reads_before.file_read_bytes,
        "task_recovery_session_slow"
      );
    }
  }
}

/// One node's pass (`task_execution._reconcile_node`). `is_driven` marks the runs this process already drives: the
/// end-landing retry passes it so that steps 1 and 2 never start a second driver, and the boot pass passes None.
pub async fn reconcile_node(
  features: &Arc<FeatureContext>,
  adapter: &TaskExecutionAdapter,
  session_id: &str,
  counters: &mut PassCounters,
  is_driven: Option<&(dyn Fn(&RunRecord) -> bool + Sync)>,
) -> anyhow::Result<()> {
  reconcile_node_inner(
    features,
    adapter,
    session_id,
    counters,
    is_driven,
    None,
    None,
    &mut PassSteps::start(&features.store),
  )
  .await
}

/// The node pass, ending each step in `steps`. `completed_before` is the start of the last pass that completed every
/// step for this node, when this boot knows one; the replay serves a run that ended before it.
#[allow(clippy::too_many_arguments)] // the pass takes its node, its counters, the replay inputs, the stop and the steps
async fn reconcile_node_inner(
  features: &Arc<FeatureContext>,
  adapter: &TaskExecutionAdapter,
  session_id: &str,
  counters: &mut PassCounters,
  is_driven: Option<&(dyn Fn(&RunRecord) -> bool + Sync)>,
  completed_before: Option<UtcDateTime>,
  stop: Option<&StartupStop>,
  steps: &mut PassSteps<'_>,
) -> anyhow::Result<()> {
  #[cfg(feature = "test-hooks")]
  adapter.recovery.reconcile_node_calls.fetch_add(1, Ordering::SeqCst);

  let store = &features.store;
  let has_meta = store.load_meta(session_id).await?.is_some();
  steps.end("load_meta");
  if !has_meta {
    return Ok(());
  }
  // Steps 1 and 2 read Run facts from this snapshot. Only the closed-task stop refreshes it, where `_reconcile_node`
  // reloads its event list.
  let runs = store.run_facts_until(session_id, &mut || stop.is_some_and(StartupStop::is_cancelled))?;
  steps.end("run_facts");
  let Some(mut runs) = runs else {
    return Ok(());
  };
  let run_records = store.list_run_records_until(session_id, &mut || stop.is_some_and(StartupStop::is_cancelled))?;
  steps.end("list_run_records");
  let Some(run_records) = run_records else {
    return Ok(());
  };
  // Warm the display-backend map: a fresh boot has seen no liveness notification.
  if let Some(newest) = run_records.last()
    && let Some(backend) = newest.backend.as_deref()
  {
    store.note_run_backend(session_id, backend);
  }
  let driven = |run: &RunRecord| is_driven.is_some_and(|driven| driven(run));
  let mut run_records_changed = false;

  // --- 0. an interrupted prompt edit lands its missing fact ---------------
  {
    let guard = store.control_lock().await;
    steps.end("control_lock");
    if let Some(locked_meta) = store.load_meta(session_id).await? {
      for scope in [PromptScope::Subtree, PromptScope::Node] {
        store.ensure_prompt_changed_fact_locked(&guard, session_id, &locked_meta, scope).await?;
      }
    }
    steps.end("ensure_prompt_changed_fact");
  }

  // --- 1. stop requests take precedence over any launch or follow ---------
  for run in &run_records {
    if driven(run) || runs.terminal_outcome(&run.id)?.is_some() {
      continue;
    }
    if runs.stop_requested(&run.id, None) {
      store.reconcile_stop_request(session_id, &run.id).await?;
      steps.end("reconcile_stop_request");
      run_records_changed = true;
    }
  }
  // A run on a non-open node without a stop fact cannot continue dispatch or hold the node open.
  let task_open = store.node_facts(session_id)?.task()?.task_state == "open";
  steps.end("node_facts");
  if !task_open {
    for run in &run_records {
      if runs.terminal_outcome(&run.id)?.is_some() || runs.stop_requested(&run.id, None) {
        continue;
      }
      store.request_stop(session_id, &run.id, &closed_task_stop_request_id(&run.id)).await?;
      steps.end("request_stop");
      let updated_runs = store.run_facts_until(session_id, &mut || stop.is_some_and(StartupStop::is_cancelled))?;
      steps.end("run_facts");
      let Some(updated_runs) = updated_runs else {
        return Ok(());
      };
      runs = updated_runs;
      run_records_changed = true;
    }
  }

  // --- 2. launched, non-terminal runs re-attach or drain -------------------
  for run in &run_records {
    if driven(run) || run.pid.is_none() {
      continue;
    }
    if runs.terminal_outcome(&run.id)?.is_some() || runs.stop_requested(&run.id, None) {
      continue;
    }
    let boot = identity::read_host_boot_time().context("reading the host boot time for task recovery")?;
    let alive =
      identity::is_run_alive(run.pid, run.pid_start.as_deref(), run.started_at.map(UtcDateTime::into_inner), boot);
    if alive {
      counters.resumed += 1;
      tracing::info!(target: "charliebot_tasks", session_id, run_id = %run.id, "task_recovery_resume");
      // A live worker follow tails the process in the background, never holding startup open.
      adapter.follow_run_in_background(session_id, &run.id);
      steps.end("follow_run_in_background");
    } else {
      counters.drained += 1;
      tracing::warn!(target: "charliebot_tasks", session_id, run_id = %run.id, "task_recovery_drain");
      let ended = Arc::new(|| false) as ResumeLiveness;
      let drain_started = Instant::now();
      adapter.resume_run(session_id, &run.id, Some(ended)).await?;
      log_slow_recovery_step(session_id, "drain_resume_run", drain_started);
      steps.end("drain_resume_run");
      run_records_changed = true;
    }
  }

  // --- 3. terminal runs replay their missing follow-up once ----------------
  if run_records_changed {
    let refreshed_run_records =
      store.list_run_records_until(session_id, &mut || stop.is_some_and(StartupStop::is_cancelled))?;
    steps.end("list_run_records");
    let Some(refreshed_run_records) = refreshed_run_records else {
      return Ok(());
    };
    replay_followups_with_records(
      features,
      adapter,
      session_id,
      counters,
      &refreshed_run_records,
      completed_before,
      stop,
    )
    .await?;
  } else {
    // Reusing this sorted snapshot is equivalent when steps 1 and 2 made no record changes, and it avoids another
    // run-directory scan per node.
    replay_followups_with_records(features, adapter, session_id, counters, &run_records, completed_before, stop)
      .await?;
  }
  steps.end("replay_followups");
  if stop.is_some_and(StartupStop::is_cancelled) {
    return Ok(());
  }
  // --- 4. pending reports and pending inputs --------------------------------
  adapter.dispatch.recover_pending_reports(session_id).await?;
  steps.end("recover_pending_reports");
  adapter.dispatch.dispatch_pending(session_id).await?;
  steps.end("dispatch_pending");
  Ok(())
}

/// Re-drive every terminal run's follow-up that its crash window lost (`task_execution._replay_followups`). Each
/// follow-up is idempotent by stable ids, so a repeated pass lands nothing twice.
pub async fn replay_followups(
  features: &Arc<FeatureContext>,
  adapter: &TaskExecutionAdapter,
  session_id: &str,
  counters: &mut PassCounters,
) -> anyhow::Result<()> {
  let run_records = features.store.list_run_records_sync(session_id)?;
  replay_followups_with_records(features, adapter, session_id, counters, &run_records, None, None).await?;
  Ok(())
}

/// Whether this boot serves a run's follow-up from the last completed pass instead of re-running it.
///
/// That pass replayed every run that ended before it started and completed every step, and each follow-up lands its
/// effects through stable ids. Two cases then have nothing to land again. A manager turn that is not the session's
/// newest run answers without acting: the owner-close recheck returns an empty verdict for every run a later run
/// supersedes. A worker or review run of a task that is no longer open meets the completion refusal before any blocker
/// check, so its delivery line is the only write. The session's newest run keeps its full follow-up, because a close
/// request that arrived after it ended is actionable only there, and an open task's worker run keeps its own.
fn served_by_completed_pass(
  completed_before: Option<UtcDateTime>,
  run: &RunRecord,
  newest_run_id: Option<&str>,
  task_open: bool,
) -> bool {
  let ended_before_pass =
    completed_before.is_some_and(|pass_started| run.ended_at.is_some_and(|ended| ended < pass_started));
  ended_before_pass && newest_run_id != Some(run.id.as_str()) && (run.kind == RunKind::ManagerTurn || !task_open)
}

/// Replay from the current run-record snapshot, shared with the preceding reconcile steps when they changed no
/// records. `replay_followups` loads this list itself.
///
/// `completed_before` is the start of the last pass that completed every step for this session. A run that ended
/// before it had its follow-up applied there, and [`served_by_completed_pass`] names the runs whose follow-up has
/// nothing left to land: this boot serves those and replays the rest. The step still runs, still counts every run it
/// serves, and still logs the line an implement or cold-read delivery logs, so the walk's counters and log items are a
/// full replay's.
async fn replay_followups_with_records(
  features: &Arc<FeatureContext>,
  adapter: &TaskExecutionAdapter,
  session_id: &str,
  counters: &mut PassCounters,
  run_records: &[RunRecord],
  completed_before: Option<UtcDateTime>,
  stop: Option<&StartupStop>,
) -> anyhow::Result<()> {
  let store = &features.store;
  let Some(meta) = store.load_meta(session_id).await? else {
    return Ok(());
  };
  // Every run's outcome comes from this snapshot, taken where `_replay_followups` loads its event list. A follow-up
  // that lands a later run's fact does not change that run's replay.
  let Some(node_facts) = (match stop {
    Some(stop) => store.node_facts_until(session_id, &mut || stop.is_cancelled())?,
    None => Some(store.node_facts(session_id)?),
  }) else {
    return Ok(());
  };
  let task_facts = node_facts.task()?;
  let completion = features.startup.require::<TaskCompletion>()?;
  // The snapshot is in the listing's `(started_at, id)` order, so its last run is the session's newest run.
  let newest_run_id = run_records.last().map(|run| run.id.as_str());
  let Some(review_facts) = replay_review_facts_until(run_records, &mut || stop.is_some_and(StartupStop::is_cancelled))
  else {
    return Ok(());
  };
  for (run_index, run) in run_records.iter().enumerate() {
    if stop.is_some_and(StartupStop::is_cancelled) {
      return Ok(());
    }
    let outcome = task_facts.terminal_run_outcome(&run.id)?;
    // A terminal run whose end metadata write failed gets its ended_at and exit_code re-derived from the raw log.
    if let Some(outcome) = outcome
      && run.ended_at.is_none()
    {
      repair_end_metadata(adapter, session_id, run, outcome).await?;
    }
    if let Some(sequence) = &run.sequence_ref {
      match controller_for_sequence(features, &sequence.kind) {
        None => {
          // The owning task's controller has not landed, so no terminal fact is written for this run. The interrupted
          // finish below is the branch for a package that is gone.
          if UNLANDED_SEQUENCE_KINDS.contains(&sequence.kind.as_str()) {
            let step = format!("the recovery of {} sequence run {}", sequence.kind, run.id);
            bail!(owner_not_landed(&sequence.kind, &step));
          }
          // A registered-but-unlaunched run would sit queued forever, so it lands an interrupted finish.
          if outcome.is_none() && run.pid.is_none() {
            let guard = store.control_lock().await;
            store
              .record_finish_locked(&guard, session_id, &run.id, RunOutcome::new(RunOutcomeKind::Interrupted))
              .await?;
          }
          if outcome.is_some() || run.pid.is_none() {
            tracing::warn!(
              target: "charliebot_tasks",
              session_id,
              run_id = %run.id,
              owner_ref = %sequence.owner_ref,
              "sequence_run_without_controller"
            );
          }
        }
        Some(controller) => {
          let tree = sequence_tree(features, adapter)?;
          if controller.recover_run(session_id, run, outcome.map(outcome_name), &tree).await? {
            counters.followups += 1;
          }
        }
      }
      continue;
    }
    let Some(outcome) = outcome else {
      continue;
    };
    if meta.profile != TaskProfile::Worker && run.kind != RunKind::ManagerTurn {
      continue;
    }
    if served_by_completed_pass(completed_before, run, newest_run_id, task_facts.task_state == "open") {
      counters.followups += 1;
      counters.replay_served += 1;
      if outcome == RunOutcomeKind::Success && run.kind == RunKind::Work {
        // The lines `after_run_finished` writes before it returns for these task types: a full replay's items.
        match meta.task.as_ref().and_then(|task| task.task_type) {
          Some(TaskType::Implement) => {
            tracing::info!(target: "charliebot_tasks", session_id, run_id = %run.id, "worker_delivery_awaits_review");
          }
          Some(TaskType::ColdRead) => {
            tracing::info!(target: "charliebot_tasks", session_id, run_id = %run.id, "cold_read_delivery_awaits_report");
          }
          _ => {}
        }
      }
      continue;
    }
    if meta.profile == TaskProfile::Manager && run.kind == RunKind::ManagerTurn {
      // The durable write landed and the crash came before after_run_finished: the owner-close recheck replays once.
      counters.followups += 1;
      match stop {
        Some(stop) => {
          let newest = newest_run_id.expect("a manager turn replays inside a session that holds it");
          if completion.recheck_close_requests_newest_until(session_id, &run.id, newest, stop).await?.is_none() {
            return Ok(());
          }
        }
        None => {
          completion.recheck_close_requests(session_id, &run.id).await?;
        }
      }
      continue;
    }
    let task_type = meta.task.as_ref().and_then(|task| task.task_type);
    if run.kind == RunKind::Work {
      counters.followups += 1;
      if task_type == Some(TaskType::ColdRead) {
        // The execution layer owns a cold-read run's delivery; the generic completion skips its task type. A failed run
        // re-drives only when no retry run follows it, since a registered retry's own finish drives the rest. An
        // interrupted run reports through the generic failure entry, never the next-backend retry.
        match outcome {
          RunOutcomeKind::Success => {
            completion.after_run_finished(session_id, &run.id).await?;
            adapter.after_cold_read_run(&meta, run, outcome_name(outcome)).await?;
          }
          RunOutcomeKind::Failed => {
            let later_work = run_records[run_index + 1..].iter().any(|later| later.kind == RunKind::Work);
            if !later_work {
              adapter.after_cold_read_run(&meta, run, outcome_name(outcome)).await?;
            }
          }
          RunOutcomeKind::Interrupted => {
            adapter.report_failure_to_parent(session_id, run, outcome_name(outcome), None).await?;
          }
        }
        continue;
      }
      if outcome == RunOutcomeKind::Success {
        completion.after_run_finished(session_id, &run.id).await?;
        if meta.task.is_some() && spawner_prompt::reviewer_run_follows(meta.task.as_ref()) && run.repo_path.is_some() {
          if let Some(reason) = completion.pause_reason(session_id)? {
            tracing::info!(target: "charliebot_tasks", session_id, run_id = %run.id, reason, "worker_delivery_paused");
          } else if review_facts.owes_review(task_facts.task_state == "open", run)
            && let crate::execution::ReviewSpawnOutcome::NoReviewer(reason) =
              adapter.maybe_spawn_review(session_id, run).await?
          {
            let summary = format!("review of work run {} found no available reviewer backend; {reason}", run.id);
            adapter.report_failure_to_parent(session_id, run, "blocked", Some(&summary)).await?;
          }
        }
        continue;
      }
      // A failed run whose failover registered its retry already took its follow-up: replaying it would report the
      // parent alongside the retry. An operator's retry is not the follow-up, and its record carries a caller-chosen
      // id, so only the failover's own stable record skips the report.
      let failover_retry_id = TaskExecutionAdapter::failover_retry_id(session_id, &run.id);
      if run_records.iter().any(|candidate| {
        candidate.retry_of_run_id.as_deref() == Some(run.id.as_str()) && candidate.id == failover_retry_id
      }) {
        continue;
      }
      adapter.after_worker_run(&meta, run, outcome_name(outcome)).await?;
    } else if run.kind == RunKind::Review {
      counters.followups += 1;
      adapter.after_review_run(&meta, run, outcome_name(outcome)).await?;
    }
  }
  Ok(())
}

/// Whether the startup replay owes a successful implement work run its review (`_replay_owes_review`). The live finish
/// path's verdict, re-derived: the task is still open, and no other work run followed this one. A work run with no
/// `started_at` followed too, since the live path read its reservation as a pause.
pub fn replay_owes_review(task_open: bool, run_records: &[RunRecord], run: &RunRecord) -> bool {
  if !task_open {
    return false;
  }
  for other in run_records {
    if other.kind != RunKind::Work || other.id == run.id {
      continue;
    }
    let Some(other_started) = other.started_at else {
      return false;
    };
    if run.started_at.is_none_or(|run_started| (other_started, &other.id) > (run_started, &run.id)) {
      return false;
    }
  }
  true
}

#[derive(Default)]
struct ReplayWorkFacts {
  count: usize,
  unstarted_count: usize,
  latest_started_at: Option<UtcDateTime>,
}

#[derive(Default)]
struct ReplayReviewFacts<'a> {
  work_count: usize,
  unstarted_work_count: usize,
  work_by_id: std::collections::HashMap<&'a str, ReplayWorkFacts>,
  latest_started_work: Option<(UtcDateTime, &'a str)>,
  second_latest_started_work: Option<(UtcDateTime, &'a str)>,
}

impl ReplayReviewFacts<'_> {
  fn owes_review(&self, task_open: bool, run: &RunRecord) -> bool {
    if !task_open {
      return false;
    }
    let own = self.work_by_id.get(run.id.as_str());
    let own_work_count = own.map_or(0, |facts| facts.count);
    let Some(started_at) = run.started_at else {
      return self.work_count == own_work_count;
    };
    let own_unstarted_count = own.map_or(0, |facts| facts.unstarted_count);
    if self.unstarted_work_count > own_unstarted_count {
      return false;
    }
    let latest_other = if self.latest_started_work.is_some_and(|latest| latest.1 == run.id) {
      self.second_latest_started_work
    } else {
      self.latest_started_work
    };
    if latest_other.is_some_and(|latest| latest > (started_at, run.id.as_str())) {
      return false;
    }
    true
  }
}

fn replay_review_facts_until<'a>(
  run_records: &'a [RunRecord],
  should_stop: &mut impl FnMut() -> bool,
) -> Option<ReplayReviewFacts<'a>> {
  let mut facts = ReplayReviewFacts::default();
  for run in run_records {
    if should_stop() {
      return None;
    }
    if run.kind != RunKind::Work {
      continue;
    }
    facts.work_count += 1;
    let by_id = facts.work_by_id.entry(run.id.as_str()).or_default();
    by_id.count += 1;
    if let Some(started_at) = run.started_at {
      if by_id.latest_started_at.is_none_or(|latest| latest < started_at) {
        by_id.latest_started_at = Some(started_at);
      }
    } else {
      facts.unstarted_work_count += 1;
      by_id.unstarted_count += 1;
    }
  }
  if should_stop() {
    return None;
  }
  for (run_id, by_id) in &facts.work_by_id {
    if should_stop() {
      return None;
    }
    let Some(started_at) = by_id.latest_started_at else {
      continue;
    };
    let latest = (started_at, *run_id);
    if facts.latest_started_work.is_none_or(|current| current < latest) {
      facts.second_latest_started_work = facts.latest_started_work;
      facts.latest_started_work = Some(latest);
    } else if facts.second_latest_started_work.is_none_or(|current| current < latest) {
      facts.second_latest_started_work = Some(latest);
    }
  }
  if should_stop() {
    return None;
  }
  Some(facts)
}

/// `resume_run`'s checks and enqueue. True when the manager follow's tracker took over the registered pair.
async fn resume_run_checked(
  adapter: &TaskExecutionAdapter,
  session_id: &str,
  run_id: &str,
  is_alive: Option<ResumeLiveness>,
) -> anyhow::Result<bool> {
  let store = &adapter.features.store;
  let run = store
    .read_run_sync(session_id, run_id)?
    .ok_or_else(|| StoreError::not_found(run_not_found_in_task_text(run_id, session_id)))?;
  let (Some(pid), Some(pid_start)) = (run.pid, run.pid_start.clone()) else {
    bail!("run {run_id} records no launched process identity; nothing to re-attach");
  };
  if store.run_facts(session_id)?.terminal_outcome(run_id)?.is_some() {
    return Ok(false);
  }
  let is_alive = match is_alive {
    Some(is_alive) => is_alive,
    None => {
      let boot = identity::read_host_boot_time().context("reading the host boot time for a re-attach")?;
      let probe = run_alive_probe(Some(pid), Some(pid_start), run.started_at.map(UtcDateTime::into_inner), boot);
      Arc::new(probe) as ResumeLiveness
    }
  };
  let meta =
    store.load_meta(session_id).await?.ok_or_else(|| StoreError::not_found(format!("task {session_id} not found")))?;
  let ended_at = if is_alive() {
    dispatch::note_run_liveness(store, session_id, &run).await;
    None
  } else {
    // The drain rule: the process ended unseen, so its end time is the raw log's last write.
    let raw_path = store.run_dir(session_id, run_id).join(RAW_LOG_NAME);
    transport_recovery::raw_completion_time(&raw_path).map(to_utc).transpose()?
  };
  let cfg = adapter.features.config.get().context("reading the configuration for a re-attach")?;
  let option = adapter.resolve_run_backend(&cfg, &run)?;
  if meta.profile == TaskProfile::Worker && crate::prompts::WORKER_KINDS.contains(&run.kind.as_str()) {
    adapter.resume_worker_run(&meta, &run, &option, is_alive, ended_at).await?;
    return Ok(false);
  }
  if meta.profile != TaskProfile::Manager || run.kind != RunKind::ManagerTurn {
    bail!("run {run_id} (profile={}, kind={}) has no resume adapter", meta.profile.as_str(), run.kind.as_str());
  }
  let receiver = enqueue_manager_resume(adapter, &meta, &run, &option, is_alive, ended_at).await?;
  track_manager_follow(adapter, session_id, run_id, receiver);
  Ok(true)
}

/// Hand a live or drained manager turn to the per-session queue (`_resume_manager_turn`). The queue's consumer runs the
/// follow before any queued turn spawns, and resolves the returned receiver with the turn's result.
async fn enqueue_manager_resume(
  adapter: &TaskExecutionAdapter,
  meta: &SessionMetadata,
  run: &RunRecord,
  option: &BackendOption,
  is_alive: ResumeLiveness,
  ended_at: Option<UtcDateTime>,
) -> anyhow::Result<oneshot::Receiver<anyhow::Result<Option<String>>>> {
  let session_id = meta.id.as_str();
  let transport_dir = adapter.features.store.run_dir(session_id, &run.id);
  let raw_log = transport_dir.join(RAW_LOG_NAME);
  let started_at = run
    .started_at
    .with_context(|| format!("run {} records a process but no started_at; nothing to re-attach", run.id))?;
  let (Some(pid), Some(pid_start)) = (run.pid, run.pid_start.clone()) else {
    bail!("run {} records no launched process identity; nothing to re-attach", run.id);
  };
  let callbacks: SessionCallbacks = adapter.turn_callbacks();
  let spawn_run = run.id.clone();
  let on_task_spawn: TaskSpawnCallback = Arc::new(move |_pid, _pid_start| {
    let run_id = spawn_run.clone();
    Box::pin(async move { Err::<(), anyhow::Error>(anyhow!("resume follow of run {run_id} must not spawn a process")) })
  });
  let on_task_finish = adapter.manager_finish_recorder(
    meta,
    &run.id,
    option,
    raw_log.clone(),
    ended_at,
    std::sync::Arc::clone(&callbacks.persist_and_broadcast),
  );
  let cfg = adapter.features.config.get().context("reading the configuration for a re-attach")?;
  let extra_env = adapter.child_env(session_id, &run.id, &meta.name)?;
  let receiver = adapter
    .queue
    .enqueue_master_resume(MasterResumeRequest {
      cfg,
      session_meta: meta.clone(),
      record: MasterResumeRecord {
        pid: Some(pid),
        pid_start: Some(pid_start),
        started_at,
        raw_log,
        // A resumed turn records no input ids: its MasterRunRecord is built without them, and master_done input ids
        // are set only when the item has them.
        user_event_ids: Vec::new(),
      },
      callbacks,
      is_alive,
      task_run: TaskRunBinding {
        session_id: session_id.to_string(),
        run_id: run.id.clone(),
        transport_dir,
        fresh_native_context: false,
      },
      on_task_spawn,
      on_task_finish,
      extra_env: Some(extra_env),
    })
    .await?;
  Ok(receiver)
}

/// Release a manager follow's registry pair when the queue resolves it, and hand an out-of-space failure to the
/// end-landing retry. The follow is never awaited: the boot pass must not wait on the queue.
fn track_manager_follow(
  adapter: &TaskExecutionAdapter,
  session_id: &str,
  run_id: &str,
  receiver: oneshot::Receiver<anyhow::Result<Option<String>>>,
) {
  let adapter = adapter.clone();
  let session_id = session_id.to_string();
  let run_id = run_id.to_string();
  tokio::spawn(async move {
    let result = receiver.await;
    adapter
      .recovery
      .resume_follows
      .lock()
      .expect("resume follow set is not poisoned")
      .remove(&(session_id.clone(), run_id.clone()));
    match result {
      Ok(Ok(_)) => {}
      Ok(Err(error)) => {
        if error.downcast_ref::<master_run::TurnLetGo>().is_none()
          && error.downcast_ref::<master_run::TurnCancelled>().is_none()
        {
          land_manager_resume_failure(&adapter, &session_id, &run_id, &error).await;
        }
        if is_out_of_space_error(error.as_ref()) {
          adapter.start_run_end_landing_retry(&session_id, Some(&run_id));
        }
      }
      Err(_) => tracing::error!(
        target: "charliebot_tasks",
        session_id,
        run_id,
        "manager_resume_follow_dropped"
      ),
    }
  });
}

/// Land a manager resume's error as its turn's end, the way a first launch lands the same failure: a failed end record,
/// the "Turn failed" notice and the node's pending inputs. A run that already ended keeps its record.
async fn land_manager_resume_failure(
  adapter: &TaskExecutionAdapter,
  session_id: &str,
  run_id: &str,
  error: &anyhow::Error,
) {
  let store = &adapter.features.store;
  let landing = async {
    if store.run_facts(session_id)?.terminal_outcome(run_id)?.is_some() {
      return Ok(());
    }
    let Some(meta) = store.load_meta(session_id).await? else { return Ok(()) };
    let Some(run) = store.read_run_sync(session_id, run_id)? else { return Ok(()) };
    adapter.land_launch_failure(&meta, &run, error, "RuntimeError").await;
    Ok::<(), anyhow::Error>(())
  }
  .await;
  if let Err(landing_error) = landing {
    tracing::error!(target: "charliebot_tasks", session_id, run_id, error = %landing_error, "manager_resume_failure_landing_failed");
  }
}

/// Land a terminal run's end metadata from its raw log (`repair_end_metadata`). The terminal fact is authoritative; the
/// write fills only the empty fields.
pub async fn repair_end_metadata(
  adapter: &TaskExecutionAdapter,
  session_id: &str,
  run: &RunRecord,
  outcome: RunOutcomeKind,
) -> anyhow::Result<()> {
  let raw_path = adapter.features.store.run_dir(session_id, &run.id).join(RAW_LOG_NAME);
  let ended_at = transport_recovery::raw_completion_time(&raw_path).map(to_utc).transpose()?;
  let mut exit_code = -1;
  if raw_path.is_file() {
    let cfg = adapter.features.config.get().context("reading the configuration for a metadata repair")?;
    let option = adapter.resolve_run_backend(&cfg, run)?;
    let meta = adapter
      .features
      .store
      .get_session(session_id)
      .await?
      .with_context(|| format!("run {} repairs end metadata but session {session_id} has no metadata", run.id))?;
    let log_dir = raw_path.parent().context("run raw log has no parent directory")?;
    let translate = master_run::build_fresh_translate(&adapter.features, &cfg, Some(&option), &meta, &run.id, log_dir)?;
    let (_, _, scanned) = transport_recovery::scan_result_exit(&raw_path, translate)?;
    exit_code = scanned;
  }
  let mut finish = RunOutcome::new(outcome);
  finish.exit_code = Some(exit_code);
  finish.ended_at = ended_at;
  adapter.dispatch.finish_run(session_id, &run.id, finish).await?;
  Ok(())
}

/// Start the node's end-landing retry task (`start_run_end_landing_retry`). A second request for the node is a no-op,
/// and the task is process-local: the next boot's pass takes over after a stop.
pub(crate) fn start_end_landing_retry(adapter: TaskExecutionAdapter, session_id: &str, run_id: Option<&str>) {
  let session = session_id.to_string();
  let run = run_id.map(str::to_string);
  let inserted =
    adapter.recovery.landing_retries.lock().expect("landing retry set is not poisoned").insert(session.clone());
  if !inserted {
    return;
  }
  let interval = adapter.recovery.landing_retry_interval();
  tokio::spawn(async move {
    wait_for_triggering_run_release(|| run.as_deref().is_some_and(|run_id| adapter.drives_run(&session, run_id))).await;
    let pass_adapter = adapter.clone();
    let pass_session = session.clone();
    let result = retry_while_out_of_space(interval, &session, run.as_deref(), move || {
      let adapter = pass_adapter.clone();
      let session = pass_session.clone();
      async move {
        let drives = |candidate: &RunRecord| adapter.drives_run(&session, &candidate.id);
        let mut counters = PassCounters::default();
        reconcile_node(&adapter.features, &adapter, &session, &mut counters, Some(&drives)).await
      }
    })
    .await;
    adapter.recovery.landing_retries.lock().expect("landing retry set is not poisoned").remove(&session);
    if let Err(error) = result {
      tracing::error!(target: "charliebot_tasks", session_id = %session, error = %format!("{error:#}"), "task_recovery_node_failed");
    }
  });
}

async fn wait_for_triggering_run_release(mut is_driven: impl FnMut() -> bool) {
  while is_driven() {
    tokio::time::sleep(DRIVEN_RUN_RELEASE_POLL_INTERVAL).await;
  }
}

/// Run `pass` until a round completes (`_run_end_landing_retry`). A round that fails for lack of space waits
/// `interval` and runs again. Any other error ends the retry and is returned.
pub(crate) async fn retry_while_out_of_space<F, Fut>(
  interval: Duration,
  session_id: &str,
  run_id: Option<&str>,
  mut pass: F,
) -> anyhow::Result<()>
where
  F: FnMut() -> Fut,
  Fut: Future<Output = anyhow::Result<()>>,
{
  loop {
    match pass().await {
      Ok(()) => return Ok(()),
      Err(error) if is_out_of_space_error(error.as_ref()) => {
        tracing::warn!(
          target: "charliebot_tasks",
          session_id,
          run_id,
          error = %format!("{error:#}"),
          "run_end_landing_retry"
        );
        tokio::time::sleep(interval).await;
      }
      Err(error) => return Err(error),
    }
  }
}

fn controller_for_sequence(features: &FeatureContext, kind: &str) -> Option<Arc<dyn SequenceController>> {
  features.registry.sequence_controllers().find(|controller| controller.sequence_kind() == kind).cloned()
}

/// The registered controller that owns a sequence kind, or `None` when no registered controller claims it (its package
/// was deleted). A kind in `UNLANDED_SEQUENCE_KINDS` fails by name instead: its controller has not landed.
pub(crate) fn owning_controller(
  features: &FeatureContext,
  kind: &str,
  step: &str,
) -> anyhow::Result<Option<Arc<dyn SequenceController>>> {
  if let Some(controller) = controller_for_sequence(features, kind) {
    return Ok(Some(controller));
  }
  if UNLANDED_SEQUENCE_KINDS.contains(&kind) {
    bail!(owner_not_landed(kind, step));
  }
  Ok(None)
}

/// The task-tree handle the sequence controllers use: the dispatcher, the completion owner, the store's session events,
/// and the adapter as the launch executor.
pub fn sequence_tree(
  features: &Arc<FeatureContext>,
  adapter: &TaskExecutionAdapter,
) -> anyhow::Result<StoreSequenceTree> {
  let dispatch: Arc<dyn SequenceDispatch> = Arc::<Dispatcher>::clone(&adapter.dispatch);
  let completion: Arc<dyn SequenceCompletion> = features.startup.require::<TaskCompletion>()?;
  let session_events: Arc<dyn SequenceSessionEvents> = Arc::new(StoreSessionEvents(Arc::clone(&features.store)));
  let executor: Arc<dyn SequenceExecutor> = Arc::new(adapter.clone());
  Ok(StoreSequenceTree::new(
    Arc::clone(&features.store),
    SequenceTreeRuntime {
      dispatch,
      completion,
      session_events,
      executor: Some(executor),
      startup_stop: features.startup.stop(),
    },
  ))
}

/// The store's session-event aggregation, as the sequence tree reads it.
struct StoreSessionEvents(Arc<Store>);

impl SequenceSessionEvents for StoreSessionEvents {
  fn prime_aggregator<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, StoreResult<u64>> {
    Box::pin(async move { self.0.prime_aggregator(session_id).await })
  }

  fn announce_appended_event<'a>(
    &'a self,
    session_id: &'a str,
    event: &'a ChatEvent,
    epoch: u64,
  ) -> BoxFuture<'a, StoreResult<()>> {
    Box::pin(async move { self.0.announce_appended_event(session_id, event, epoch).await })
  }
}

/// The sequence kinds whose controllers have not landed: a run of one of these kinds fails
/// the node pass by name. The owning task is named by the kind. Each task deletes its entry when it registers its
/// controller.
const UNLANDED_SEQUENCE_KINDS: &[&str] = &["improve"];

/// The error of a replay step whose owning task has not landed: the node pass fails and names the owner.
pub(crate) fn owner_not_landed(owner: &str, step: &str) -> anyhow::Error {
  anyhow!("{step} is owned by the {owner} task, which has not landed; the node pass cannot replay it")
}

fn outcome_name(outcome: RunOutcomeKind) -> &'static str {
  match outcome {
    RunOutcomeKind::Success => "success",
    RunOutcomeKind::Failed => "failed",
    RunOutcomeKind::Interrupted => "interrupted",
  }
}

/// A file time as the store's UTC timestamp, truncated to microseconds.
fn to_utc(value: OffsetDateTime) -> anyhow::Result<UtcDateTime> {
  let micros = value.replace_nanosecond(value.nanosecond() / 1_000 * 1_000)?;
  let text = micros.format(&Rfc3339)?;
  UtcDateTime::parse(&text).with_context(|| format!("converting the file time {text}"))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::execution::pre_recovery_step;
  use charliebot_agents::backend_availability::BackendAvailability;
  use charliebot_agents::backend_pool::BackendPool;
  use charliebot_core::credentials::load_credentials;
  use charliebot_core::hot_reload::HotReload;
  use charliebot_registry::store_hooks::store_hooks;
  use charliebot_registry::{Registry, StartupState};
  use charliebot_store::layout;
  use charliebot_store::metadata::TaskSpec;
  use charliebot_store::run_records::{self, SequenceRef};
  use charliebot_store::{CreateTask, HistoryWork, TaskCaller};
  use serde_json::{Map, json};
  use std::sync::atomic::Ordering;
  use std::time::SystemTime;

  const QUEUED_RUN_ID: &str = "synthetic-queued-run";

  fn run(id: &str, kind: RunKind, started_at: Option<&str>) -> RunRecord {
    RunRecord {
      id: id.to_string(),
      session_id: "synthetic-node".to_string(),
      kind,
      input_event_ids: Vec::new(),
      task_spec_hash: None,
      task_spec_ref: None,
      prompt_snapshot_ref: None,
      retry_of_run_id: None,
      review_of_run_id: None,
      backend: Some("synthetic".to_string()),
      model: None,
      native_session_id: None,
      pid: None,
      pid_start: None,
      started_at: started_at.map(|text| UtcDateTime::parse(text).expect("the synthetic time parses")),
      ended_at: None,
      exit_code: None,
      repo_path: None,
      base_branch: None,
      branch_name: None,
      worktree_path: None,
      sequence_ref: None,
      raw_log_ref: None,
      events_ref: None,
      result_ref: None,
    }
  }

  #[test]
  fn a_review_is_owed_only_to_the_newest_unfollowed_work_run_of_an_open_task() {
    let older = run("older", RunKind::Work, Some("2026-10-18T10:00:00Z"));
    let newest = run("newest", RunKind::Work, Some("2026-10-18T11:00:00Z"));
    let runs = [older.clone(), newest.clone()];
    assert!(replay_owes_review(true, &runs, &newest));
    assert!(!replay_owes_review(true, &runs, &older));
    assert!(!replay_owes_review(false, &runs, &newest), "a closed task owes no review");
  }

  #[test]
  fn a_work_run_with_a_reservation_and_no_start_time_counts_as_a_follower() {
    let reserved = run("reserved", RunKind::Work, None);
    let finished = run("finished", RunKind::Work, Some("2026-10-18T10:00:00Z"));
    assert!(!replay_owes_review(true, &[reserved.clone(), finished.clone()], &finished));
    assert!(!replay_owes_review(true, &[reserved.clone(), finished], &reserved));
  }

  #[test]
  fn replay_review_scan_checks_each_run_and_stops_at_the_requested_count() {
    let runs = [
      run("older", RunKind::Work, Some("2026-10-18T10:00:00Z")),
      run("newest", RunKind::Work, Some("2026-10-18T11:00:00Z")),
      run("review", RunKind::Review, Some("2026-10-18T12:00:00Z")),
    ];
    let mut checks = 0;
    let facts = replay_review_facts_until(&runs, &mut || {
      checks += 1;
      checks == 2
    });

    assert!(facts.is_none(), "the stopped scan publishes no partial review index");
    assert_eq!(checks, 2, "the review walk checks before each run record");
  }

  #[test]
  fn replay_review_index_checks_stop_during_the_unique_run_pass() {
    let runs = [
      run("older", RunKind::Work, Some("2026-10-18T10:00:00Z")),
      run("newest", RunKind::Work, Some("2026-10-18T11:00:00Z")),
      run("review", RunKind::Review, Some("2026-10-18T12:00:00Z")),
    ];
    let mut checks = 0;
    let facts = replay_review_facts_until(&runs, &mut || {
      checks += 1;
      checks == 5
    });

    assert!(facts.is_none(), "the stop also interrupts the pass over unique work runs");
    assert_eq!(checks, 5, "the unique-run pass checks before each summary entry");
  }

  #[test]
  fn replay_review_index_answers_all_runs_after_one_scan() {
    let runs = [
      run("older", RunKind::Work, Some("2026-10-18T10:00:00Z")),
      run("newest", RunKind::Work, Some("2026-10-18T11:00:00Z")),
      run("review", RunKind::Review, Some("2026-10-18T12:00:00Z")),
    ];
    let mut checks = 0;
    let facts = replay_review_facts_until(&runs, &mut || {
      checks += 1;
      false
    })
    .unwrap();

    assert_eq!(checks, 7, "both review scans check each run and the end of each scan");
    for run in &runs {
      assert_eq!(facts.owes_review(true, run), replay_owes_review(true, &runs, run));
      assert_eq!(facts.owes_review(false, run), replay_owes_review(false, &runs, run));
    }
    let outside = run("outside", RunKind::Work, Some("2026-10-18T11:30:00Z"));
    assert_eq!(facts.owes_review(true, &outside), replay_owes_review(true, &runs, &outside));

    let duplicate_ids = [
      run("same", RunKind::Work, Some("2026-10-18T10:00:00Z")),
      run("same", RunKind::Work, None),
      run("other", RunKind::Work, Some("2026-10-18T11:00:00Z")),
    ];
    let facts = replay_review_facts_until(&duplicate_ids, &mut || false).unwrap();
    for run in &duplicate_ids {
      assert_eq!(facts.owes_review(true, run), replay_owes_review(true, &duplicate_ids, run));
    }
  }

  #[test]
  fn the_closed_task_stop_id_is_the_launch_paths_id() {
    assert_eq!(closed_task_stop_request_id("run-1"), "closed-task:run-1");
  }

  #[tokio::test]
  async fn the_retry_repeats_out_of_space_rounds_and_stops_at_the_first_clean_round() {
    let mut rounds = 0;
    let result = retry_while_out_of_space(Duration::ZERO, "synthetic-node", Some("run-1"), || {
      rounds += 1;
      let round = rounds;
      async move {
        if round < 3 {
          Err(anyhow::Error::new(io::Error::from_raw_os_error(libc::ENOSPC)).context("landing the end record"))
        } else {
          Ok(())
        }
      }
    })
    .await;
    assert!(result.is_ok());
    assert_eq!(rounds, 3);
  }

  #[tokio::test]
  async fn the_retry_ends_on_a_non_space_error_without_a_second_round() {
    let mut rounds = 0;
    let result = retry_while_out_of_space(Duration::ZERO, "synthetic-node", Some("run-1"), || {
      rounds += 1;
      async { Err(anyhow!("synthetic defect in the end path")) }
    })
    .await;
    assert!(result.is_err());
    assert_eq!(rounds, 1);
  }

  #[tokio::test]
  async fn a_retry_waits_for_its_triggering_run_to_leave_the_launch_set() {
    let driven = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let release = Arc::clone(&driven);
    tokio::spawn(async move {
      tokio::time::sleep(DRIVEN_RUN_RELEASE_POLL_INTERVAL * 2).await;
      release.store(false, std::sync::atomic::Ordering::Release);
    });

    wait_for_triggering_run_release(|| driven.load(std::sync::atomic::Ordering::Acquire)).await;

    assert!(!driven.load(std::sync::atomic::Ordering::Acquire));
  }

  /// A fresh synthetic home whose startup state holds the task execution runtime, as the startup step builds it.
  async fn startup_features() -> (tempfile::TempDir, Arc<FeatureContext>) {
    let home = tempfile::tempdir().expect("temporary directory exists");
    let registry = Arc::new(Registry::new());
    let loaded = registry.load_config(home.path()).expect("an empty home loads with the defaults");
    let reload_config = loaded.clone();
    let config = Arc::new(HotReload::new(
      loaded.config_file(),
      "recovery_test_config_reload_failed",
      Box::new(move || Ok(reload_config.clone())),
    ));
    let store = Arc::new(
      Store::new(loaded, registry.metadata_slots(), store_hooks(Arc::clone(&registry))).expect("the store opens"),
    );
    let credential_home = home.path().to_path_buf();
    let credentials = Arc::new(HotReload::new(
      home.path().join("credentials.yaml"),
      "recovery_test_credentials_reload_failed",
      Box::new(move || load_credentials(&credential_home)),
    ));
    let startup = Arc::new(StartupState::default());
    startup
      .insert(BackendAvailability::load(home.path()).expect("the availability owner loads"))
      .expect("the owner inserts once");
    startup
      .insert(BackendPool::new(
        startup.require::<BackendAvailability>().expect("the owner is in the startup state"),
        Arc::clone(&registry),
        Arc::new(charliebot_agents::model_pool::RotationPositions::default()),
      ))
      .expect("the pool inserts once");
    let features = Arc::new(FeatureContext { config, credentials, store, registry, startup });
    pre_recovery_step(Arc::clone(&features)).await.expect("the startup step runs");
    (home, features)
  }

  /// Creates an open task holding one queued run of `sequence_kind`, which no controller launches or claims.
  async fn queue_sequence_run(features: &Arc<FeatureContext>, sequence_kind: &str, run_kind: RunKind) -> String {
    let store = &features.store;
    let task = store
      .create_task(
        CreateTask {
          request_id: "synthetic-task-create".to_string(),
          task_parent_id: None,
          profile: TaskProfile::Manager,
          task: None,
          name: Some("Synthetic task".to_string()),
          backend: Some("synthetic-backend".to_string()),
          group: None,
          session_id: None,
        },
        TaskCaller::Operator,
        Map::new(),
      )
      .await
      .expect("the task node is created");
    let sequence_ref =
      SequenceRef { kind: sequence_kind.to_string(), owner_ref: "synthetic-owner".to_string(), position: 0 };
    let queued =
      RunRecord { session_id: task.id.clone(), sequence_ref: Some(sequence_ref), ..run(QUEUED_RUN_ID, run_kind, None) };
    let guard = store.control_lock().await;
    store.register_run_locked(&guard, &task.id, queued, None).await.expect("the queued run registers");
    task.id
  }

  /// One node pass over `session_id`, run with the startup adapter as the boot pass runs it.
  async fn node_pass(features: &Arc<FeatureContext>, session_id: &str) -> anyhow::Result<()> {
    let adapter = features.startup.require::<TaskExecutionAdapter>().expect("the startup step inserts the adapter");
    reconcile_node(features, &adapter, session_id, &mut PassCounters::default(), None).await
  }

  #[tokio::test]
  async fn a_walk_batch_keeps_each_result_in_order_and_ends_at_a_stop() {
    let (_home, features) = startup_features().await;
    let store = &features.store;
    let count = parallel_walk::MIN_ITEMS + 2;
    let mut candidates = Vec::with_capacity(count);
    for index in 0..count {
      let session_id = format!("batch-{index:03}");
      let dir = store.session_dir(&session_id);
      std::fs::create_dir_all(layout::data_dir(&dir)).expect("the data directory exists");
      let events = if index % 7 == 0 {
        "{\"id\": \"e1\", \"type\": \"task_created\", \"actor\": \"user\"}\n"
      } else {
        "{\"id\": \"e1\", \"type\": \"user\", \"content\": \"hello\"}\n"
      };
      std::fs::write(layout::chat_events_path(&dir), events).expect("the event log is written");
      if index % 11 == 0 {
        std::fs::create_dir_all(store.run_dir(&session_id, "run-1")).expect("the run directory exists");
      }
      let meta = SessionMetadata::new(session_id.clone(), "Batch session", TaskProfile::Manager);
      candidates.push(FullPassSession { session_id, meta, completed_before: None });
    }
    let stop = StartupStop::default();
    let proved = parallel_walk::map_until(&candidates, &|| stop.is_cancelled(), |candidate| {
      store.has_no_task_node(&candidate.meta)
    })
    .expect("no stop request arrived");
    assert_eq!(proved.len(), candidates.len(), "every candidate keeps its proof");
    for (index, (proved, candidate)) in proved.into_iter().zip(&candidates).enumerate() {
      let sequential = store.has_no_task_node(&candidate.meta).expect("the sequential proof reads the session");
      assert_eq!(proved.expect("the batch proof reads the session"), sequential, "candidate {index}");
      assert_eq!(sequential, index % 7 != 0 && index % 11 != 0, "candidate {index}");
    }

    // A worker checks the stop before each item it takes, so a stop ends the batch with the later items unread.
    let taken = std::sync::atomic::AtomicUsize::new(0);
    let stopped = parallel_walk::map_until(&candidates, &|| stop.is_cancelled(), |_| {
      taken.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
      stop.cancel();
    });
    assert!(stopped.is_none(), "a stopped batch returns no results");
    let taken = taken.into_inner();
    assert!(taken < candidates.len(), "the stop ended the batch after {taken} of {} items", candidates.len());
  }

  #[tokio::test]
  async fn startup_stop_cancels_one_in_flight_node_before_completion() {
    let stop = StartupStop::default();
    let completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let completion_count = Arc::clone(&completed);
    let pass_stop = stop.clone();
    let pass = tokio::spawn(async move {
      reconcile_node_until_stopped(&pass_stop, async move {
        std::future::pending::<()>().await;
        completion_count.fetch_add(1, Ordering::SeqCst);
      })
      .await
    });

    stop.cancel();

    assert_eq!(pass.await.unwrap(), None);
    assert_eq!(completed.load(Ordering::SeqCst), 0, "the cancelled node pass never completes");
  }

  /// The boot pass folds a node that needs no work from its fact scan: it loads no event log and keeps none, so a later
  /// fact read answers from the seal and the first content read loads the log.
  #[tokio::test]
  #[ignore = "integration"]
  async fn boot_recovery_folds_a_quiet_node_from_its_fact_scan_and_keeps_no_event_list() {
    let (_home, features) = startup_features().await;
    let node = features
      .store
      .create_task(
        CreateTask {
          request_id: "synthetic-retained-node".to_string(),
          task_parent_id: None,
          profile: TaskProfile::Manager,
          task: None,
          name: Some("Synthetic retained node".to_string()),
          backend: Some("synthetic".to_string()),
          group: None,
          session_id: None,
        },
        TaskCaller::Operator,
        Map::new(),
      )
      .await
      .unwrap();
    let adapter = features.startup.require::<TaskExecutionAdapter>().unwrap();
    let before = features.store.history_work();

    reconcile_task_tree(&features, &adapter).await.unwrap();

    let after_recovery = features.store.history_work();
    assert_eq!(after_recovery.file_reads, before.file_reads, "the pass reads the node's facts from its scan");
    assert_eq!(features.store.task_state(&node.id).unwrap(), "open");
    assert_eq!(features.store.history_work().file_reads, after_recovery.file_reads, "a later fact read uses the seal");
    let events = features.store.load_chat_events_shared_sync(&node.id).unwrap();
    assert!(!events.is_empty());
    assert_eq!(
      features.store.history_work().file_reads,
      after_recovery.file_reads + 1,
      "the boot keeps no event list, so the first content read loads the log"
    );
    let log_bytes = std::fs::metadata(features.store.chat_events_path(&node.id)).unwrap().len();
    assert_eq!(
      features.store.history_work().file_read_bytes,
      after_recovery.file_read_bytes + log_bytes,
      "the read counts the log's bytes"
    );
  }

  /// Each event the test thread logs, rendered as the server's log renders it.
  struct RenderedLines(Arc<std::sync::Mutex<Vec<String>>>);

  impl charliebot_core::test_support::EventSink for RenderedLines {
    fn event(&self, event: &tracing::Event<'_>) {
      let line = charliebot_core::logging::render_line(event);
      self.0.lock().expect("the line capture is not poisoned").push(line);
    }
  }

  /// The `run_id` of every captured line that renders `event` from `module`, the emitting module the rendered line's
  /// source location carries.
  fn captured_run_ids(lines: &std::sync::Mutex<Vec<String>>, event: &str, module: &str) -> Vec<String> {
    lines
      .lock()
      .expect("the line capture is not poisoned")
      .iter()
      .filter(|line| line.contains(module))
      .filter_map(|line| charliebot_core::logline::parse(line))
      .filter(|row| row.event == event)
      .map(|row| row.field("run_id").expect("a delivery line names its run").to_string())
      .collect()
  }

  /// A run that ended before the last completed pass started was followed up by that pass, so the boot replay serves it
  /// without re-running the delivery. The session's newest run keeps its full follow-up: it is the one run a close
  /// request that arrived later acts on. A served run still counts as a follow-up and still logs the line the delivery
  /// logs, so the walk's counters and log items are a full replay's.
  #[tokio::test]
  #[ignore = "integration"]
  async fn a_replay_serves_runs_that_ended_before_the_last_completed_pass() {
    let (_home, features) = startup_features().await;
    let session_id = closed_worker_node(&features, "synthetic-served-replay", 2).await;
    let adapter = features.startup.require::<TaskExecutionAdapter>().expect("the startup step inserts the adapter");
    let run_records = features.store.list_run_records_sync(&session_id).expect("the synthetic runs list");
    assert_eq!(run_records.len(), 2, "the fixture writes two runs");
    let newest = run_records.last().expect("the node holds two runs").id.clone();
    let last_end = run_records.last().and_then(|run| run.ended_at).expect("the synthetic run ended");
    let pass_started = UtcDateTime::from(last_end.into_inner() + time::Duration::seconds(1));

    let lines = Arc::new(std::sync::Mutex::new(Vec::new()));
    let _capture = charliebot_core::test_support::install_events(Arc::new(RenderedLines(Arc::clone(&lines))));
    let mut served = PassCounters::default();
    replay_followups_with_records(
      &features,
      &adapter,
      &session_id,
      &mut served,
      &run_records,
      Some(pass_started),
      None,
    )
    .await
    .expect("the served replay completes");
    assert_eq!(
      captured_run_ids(&lines, "worker_delivery_awaits_review", "tasks/recovery.rs"),
      [run_records[0].id.clone()],
      "the older run's delivery line comes from the serving branch"
    );
    assert_eq!(
      captured_run_ids(&lines, "worker_delivery_awaits_review", "tasks/completion.rs"),
      [newest],
      "only the newest run keeps its full follow-up"
    );
    assert_eq!(served.followups, 2, "a served run still counts as a follow-up");
    assert_eq!(served.replay_served, 1, "the older run comes from the serving branch");

    lines.lock().expect("the line capture is not poisoned").clear();
    let mut full = PassCounters::default();
    replay_followups_with_records(&features, &adapter, &session_id, &mut full, &run_records, None, None)
      .await
      .expect("the full replay completes");
    assert_eq!(full.followups, served.followups, "both replays count the same follow-ups");
    assert_eq!(
      captured_run_ids(&lines, "worker_delivery_awaits_review", "tasks/completion.rs").len(),
      2,
      "a full replay follows up every run"
    );
  }

  /// A run whose follow-up could still land something keeps its full replay. An open task's worker run can close the
  /// task, so it is never served. A manager turn a later run supersedes cannot act, so an open task serves it too.
  #[tokio::test]
  #[ignore = "integration"]
  async fn a_replay_serves_only_runs_whose_follow_up_has_nothing_left_to_land() {
    let (_home, features) = startup_features().await;
    let adapter = features.startup.require::<TaskExecutionAdapter>().expect("the startup step inserts the adapter");

    let open_worker =
      synthetic_node(&features, "synthetic-open-replay", TaskProfile::Worker, RunKind::Work, 2, false).await;
    let open_records = features.store.list_run_records_sync(&open_worker).expect("the open node's runs list");
    let open_end = open_records.last().and_then(|run| run.ended_at).expect("the synthetic run ended");
    let open_pass = UtcDateTime::from(open_end.into_inner() + time::Duration::seconds(1));
    let mut open = PassCounters::default();
    replay_followups_with_records(&features, &adapter, &open_worker, &mut open, &open_records, Some(open_pass), None)
      .await
      .expect("the open node's replay completes");
    assert_eq!(open.followups, 2, "both runs are followed up");
    assert_eq!(open.replay_served, 0, "an open task's worker run keeps its full follow-up");

    let manager =
      synthetic_node(&features, "synthetic-manager-replay", TaskProfile::Manager, RunKind::ManagerTurn, 2, false).await;
    let manager_records = features.store.list_run_records_sync(&manager).expect("the manager node's runs list");
    let manager_end = manager_records.last().and_then(|run| run.ended_at).expect("the synthetic turn ended");
    let manager_pass = UtcDateTime::from(manager_end.into_inner() + time::Duration::seconds(1));
    let mut manager_replay = PassCounters::default();
    replay_followups_with_records(
      &features,
      &adapter,
      &manager,
      &mut manager_replay,
      &manager_records,
      Some(manager_pass),
      None,
    )
    .await
    .expect("the manager node's replay completes");
    assert_eq!(manager_replay.followups, 2, "both manager turns are followed up");
    assert_eq!(manager_replay.replay_served, 1, "the superseded manager turn comes from the serving branch");
  }

  /// A node pass at the slow threshold logs each step it ran in run order, a repeated step once, and the whole-log
  /// reads the store made while it ran. A faster pass logs nothing.
  #[tokio::test]
  async fn a_slow_node_pass_logs_its_steps_and_whole_log_reads() {
    let (_home, features) = startup_features().await;
    let node = features
      .store
      .create_task(
        CreateTask {
          request_id: "synthetic-slow-node".to_string(),
          task_parent_id: None,
          profile: TaskProfile::Manager,
          task: None,
          name: Some("Synthetic slow node".to_string()),
          backend: Some("synthetic".to_string()),
          group: None,
          session_id: None,
        },
        TaskCaller::Operator,
        Map::new(),
      )
      .await
      .unwrap();
    let log_bytes = std::fs::metadata(features.store.chat_events_path(&node.id)).unwrap().len();
    let lines = Arc::new(std::sync::Mutex::new(Vec::new()));
    let _capture = charliebot_core::test_support::install_events(Arc::new(RenderedLines(Arc::clone(&lines))));

    drop(NodePassTimer::start(&features.store, node.id.clone()));
    assert!(lines.lock().unwrap().is_empty(), "a pass under the threshold logs nothing");

    features.store.evict_recovery_history(&node.id);

    let mut timer = NodePassTimer::start(&features.store, node.id.clone());
    timer.started -= Duration::from_millis(SLOW_RECOVERY_STEP_MS);
    timer.steps.end("load_meta");
    features.store.prime_aggregator(&node.id).await.unwrap();
    timer.steps.end("replay_followups");
    timer.steps.end("replay_followups");
    timer.steps.end("dispatch_pending");
    let mut totals = StepTotals::default();
    totals.add(&timer.steps);
    totals.add(&timer.steps);
    drop(timer);

    // The walk's totals keep each step's reads with the step that made them, summed over the passes.
    let (step_ms, step_file_reads, step_file_read_bytes) = totals.texts();
    let names =
      |text: &str| text.split(' ').map(|pair| pair.split_once('=').unwrap().0.to_string()).collect::<Vec<_>>();
    assert_eq!(names(&step_ms), names(&step_file_reads), "every text lists the same steps in the same order");
    assert_eq!(names(&step_ms), names(&step_file_read_bytes), "every text lists the same steps in the same order");
    let value = |text: &str, step: &str| {
      text.split(' ').find_map(|pair| pair.strip_prefix(&format!("{step}="))).map(str::to_string)
    };
    assert_eq!(value(&step_file_reads, "replay_followups"), Some("2".to_string()), "{step_file_reads}");
    assert_eq!(value(&step_file_reads, "load_meta"), Some("0".to_string()), "{step_file_reads}");
    assert_eq!(
      value(&step_file_read_bytes, "replay_followups"),
      Some((2 * log_bytes).to_string()),
      "{step_file_read_bytes}"
    );

    let lines = lines.lock().unwrap();
    let [line] = lines.as_slice() else { panic!("one slow pass logs one line: {lines:?}") };
    let row = charliebot_core::logline::parse(line).expect("the line reads back");
    assert_eq!(row.event, "task_recovery_session_slow");
    assert_eq!(row.field("session_id"), Some(node.id.as_str()));
    let elapsed_ms: u64 = row.field("elapsed_ms").expect("the line has elapsed_ms").parse().unwrap();
    assert!(elapsed_ms >= SLOW_RECOVERY_STEP_MS, "{line}");
    let steps: Vec<&str> = row
      .field("step_ms")
      .expect("the line has step_ms")
      .split(' ')
      .map(|pair| {
        let (name, elapsed_ms) = pair.split_once('=').expect("a step is name=ms");
        elapsed_ms.parse::<u64>().expect("a step's ms is a number");
        name
      })
      .collect();
    assert_eq!(steps, ["load_meta", "replay_followups", "dispatch_pending"], "{line}");
    assert_eq!(row.field("file_reads"), Some("1"), "{line}");
    assert_eq!(row.field("file_read_bytes"), Some(log_bytes.to_string().as_str()), "{line}");
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_queued_improve_run_fails_the_node_pass_and_keeps_no_terminal_fact() {
    let (_home, features) = startup_features().await;
    let session_id = queue_sequence_run(&features, "improve", RunKind::Iteration).await;

    let error = node_pass(&features, &session_id).await.expect_err("the improve run has no controller to replay it");

    let message = format!("{error:#}");
    assert!(
      message.contains(QUEUED_RUN_ID) && message.contains("improve task"),
      "the error names the owner: {message}"
    );
    let events = features.store.load_all_facts(&session_id).expect("the session facts load");
    assert!(run_records::terminal_outcome(&events, QUEUED_RUN_ID).expect("the terminal fact reads").is_none());
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_queued_run_of_an_unknown_sequence_kind_lands_an_interrupted_finish() {
    let (_home, features) = startup_features().await;
    let session_id = queue_sequence_run(&features, "synthetic-kind", RunKind::Work).await;

    node_pass(&features, &session_id).await.expect("the node pass finishes");

    let events = features.store.load_all_facts(&session_id).expect("the session facts load");
    let outcome = run_records::terminal_outcome(&events, QUEUED_RUN_ID).expect("the terminal fact reads");
    assert_eq!(outcome, Some(RunOutcomeKind::Interrupted));
  }

  /// A queued run on a node whose newest `task_closed` fact repeats an earlier event id. The fold skips a repeated id,
  /// so the node stays open, and the pass keeps the run unsettled. A read of the newest lifecycle line alone would call
  /// the node closed and settle the run with a closed-task stop.
  #[tokio::test]
  #[ignore = "integration"]
  async fn the_closed_node_check_reads_the_task_state_of_the_full_fold() {
    let (_home, features) = startup_features().await;
    let session_id = queue_sequence_run(&features, "synthetic-kind", RunKind::Work).await;
    for event in [
      json!({"id": "synthetic-repeated-id", "type": "synthetic_note"}),
      json!({"id": "synthetic-repeated-id", "type": "task_closed", "outcome": "completed"}),
    ] {
      features.store.append_chat_event(&session_id, event).await.expect("the synthetic fact appends");
    }
    assert_eq!(features.store.task_state(&session_id).expect("the task state folds"), "open");

    node_pass(&features, &session_id).await.expect("the node pass finishes");

    let events = features.store.load_all_facts(&session_id).expect("the session facts load");
    assert!(
      !run_records::stop_requested_in_events(&events, QUEUED_RUN_ID, None),
      "the pass settled a queued run of an open node with a closed-task stop"
    );
  }

  /// A pid above Linux's `PID_MAX_LIMIT` (2^22 on 64-bit), so no process holds it and no stop signal is ever sent.
  const UNUSED_PID: u32 = (1 << 22) + 95;

  /// A malformed `task_imported` fact fails the task fold. The Run facts still answer, so the pass lands the durable
  /// stop of a dead launched run before the closed-node check returns the fold's error.
  #[tokio::test]
  #[ignore = "integration"]
  async fn a_rejected_task_fold_fails_the_pass_after_the_stop_requests_land() {
    let (_home, features) = startup_features().await;
    let store = &features.store;
    let session_id = queue_sequence_run(&features, "synthetic-kind", RunKind::Work).await;
    let launched_id = "synthetic-launched-run";
    let launched = RunRecord {
      session_id: session_id.clone(),
      pid: Some(UNUSED_PID),
      pid_start: Some("1".to_string()),
      ..run(launched_id, RunKind::Work, Some("2026-10-18T10:00:00Z"))
    };
    {
      let guard = store.control_lock().await;
      store.register_run_locked(&guard, &session_id, launched, None).await.expect("the launched run registers");
      store
        .append_stop_request_locked(&guard, &session_id, launched_id, "synthetic-stop")
        .await
        .expect("the stop lands");
    }
    let import = json!({"id": "synthetic-import", "type": "task_imported", "pending_inputs": [{}]});
    store.append_chat_event(&session_id, import).await.expect("the malformed import fact appends");

    let error = node_pass(&features, &session_id).await.expect_err("the task fold rejects the import fact");

    assert!(format!("{error:#}").contains("without a stable input_id"), "the fold's error fails the pass: {error:#}");
    let events = store.load_all_facts(&session_id).expect("the session facts load");
    let outcome = run_records::terminal_outcome(&events, launched_id).expect("the terminal fact reads");
    assert_eq!(outcome, Some(RunOutcomeKind::Interrupted), "the stop request landed before the pass failed");
  }

  /// Creates a closed implement worker node with `run_count` successful work runs. Each run carries the repository path,
  /// so the replay of its follow-up reads the node's shared facts.
  async fn closed_worker_node(features: &Arc<FeatureContext>, request_id: &str, run_count: usize) -> String {
    synthetic_node(features, request_id, TaskProfile::Worker, RunKind::Work, run_count, true).await
  }

  /// Creates a node of `profile` with `run_count` successful `kind` runs, closed only when `close` is set. A worker
  /// node carries the implement task spec and the repository path its follow-up reads; a manager node carries no task.
  async fn synthetic_node(
    features: &Arc<FeatureContext>,
    request_id: &str,
    profile: TaskProfile,
    kind: RunKind,
    run_count: usize,
    close: bool,
  ) -> String {
    let worker = profile == TaskProfile::Worker;
    let task = features
      .store
      .create_task(
        CreateTask {
          request_id: request_id.to_string(),
          task_parent_id: None,
          profile,
          task: worker.then(|| TaskSpec {
            goal: "synthetic implementation".to_string(),
            repo_path: Some("/synthetic/repo".to_string()),
            task_type: Some(TaskType::Implement),
            ..TaskSpec::default()
          }),
          name: Some(request_id.to_string()),
          backend: Some("synthetic".to_string()),
          group: None,
          session_id: None,
        },
        TaskCaller::Operator,
        Map::new(),
      )
      .await
      .expect("the synthetic worker node is created");
    let guard = features.store.control_lock().await;
    for index in 0..run_count {
      let run_id = format!("synthetic-run-{index:04}");
      let run = RunRecord {
        session_id: task.id.clone(),
        repo_path: worker.then(|| "/synthetic/repo".to_string()),
        ..run(&run_id, kind, None)
      };
      features.store.register_run_locked(&guard, &task.id, run, None).await.expect("the synthetic run registers");
      features
        .store
        .record_finish_locked(&guard, &task.id, &run_id, RunOutcome::new(RunOutcomeKind::Success))
        .await
        .expect("the synthetic run finishes");
    }
    if close {
      features
        .store
        .append_control_fact(&guard, &task.id, json!({"type": "task_closed", "outcome": "completed"}))
        .await
        .expect("the synthetic node closes");
    }
    task.id
  }

  /// The store's history work for one node's recovery pass, measured from a cold snapshot of that node.
  async fn node_pass_history_work(
    features: &Arc<FeatureContext>,
    adapter: &TaskExecutionAdapter,
    session_id: &str,
  ) -> HistoryWork {
    features.store.evict_recovery_history(session_id);
    let before = features.store.history_work();
    reconcile_node(features, adapter, session_id, &mut PassCounters::default(), None)
      .await
      .expect("the node's pass finishes");
    let after = features.store.history_work();
    HistoryWork {
      file_reads: after.file_reads - before.file_reads,
      file_read_bytes: after.file_read_bytes - before.file_read_bytes,
      full_folds: after.full_folds - before.full_folds,
      folded_events: after.folded_events - before.folded_events,
      owned_event_clones: after.owned_event_clones - before.owned_event_clones,
      event_history_views: after.event_history_views - before.event_history_views,
      node_state_full_folds: after.node_state_full_folds - before.node_state_full_folds,
      node_state_folded_events: after.node_state_folded_events - before.node_state_folded_events,
      run_event_bytes_read: after.run_event_bytes_read - before.run_event_bytes_read,
    }
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn recovery_history_reads_and_folds_do_not_grow_with_run_count() {
    let (_home, features) = startup_features().await;
    let small_id = closed_worker_node(&features, "synthetic-one-run-node", 1).await;
    let large_id = closed_worker_node(&features, "synthetic-many-run-node", 8).await;
    let adapter = features.startup.require::<TaskExecutionAdapter>().expect("the startup step inserts the adapter");
    let small = node_pass_history_work(&features, &adapter, &small_id).await;
    let large = node_pass_history_work(&features, &adapter, &large_id).await;
    assert_eq!(small.file_reads, large.file_reads, "history reads grew with run count: one={small:?}, many={large:?}");
    assert_eq!(small.full_folds, large.full_folds, "full folds grew with run count: one={small:?}, many={large:?}");
    assert_eq!(small.owned_event_clones, 0, "one-run recovery copied event payloads: {small:?}");
    assert_eq!(large.owned_event_clones, 0, "many-run recovery copied event payloads: {large:?}");
    let small_events = features.store.load_all_facts(&small_id).expect("the one-run history loads").len();
    let large_events = features.store.load_all_facts(&large_id).expect("the many-run history loads").len();
    assert_eq!(small.folded_events, small_events as u64, "each event folds once: one={small:?}");
    assert_eq!(large.folded_events, large_events as u64, "each event folds once: many={large:?}");
  }

  #[cfg(feature = "test-hooks")]
  #[test]
  fn the_marker_write_syncs_the_temporary_file_once_before_the_rename() {
    let home = tempfile::tempdir().expect("temporary directory exists");
    let pass_started_at = UtcDateTime::parse("2026-10-21T03:00:00.123456+00:00").expect("the synthetic time parses");
    charliebot_core::fsutil::clear_synced_write_steps();

    write_boot_recovery_marker(home.path(), pass_started_at, &BTreeSet::new()).expect("the marker writes");

    let steps = charliebot_core::fsutil::synced_write_steps();
    assert_eq!(steps, ["sync", "rename"], "the temporary file syncs once, then the rename replaces the marker");
    let marker_path = boot_recovery_marker_path(home.path());
    let marker: Value =
      serde_json::from_slice(&std::fs::read(&marker_path).expect("the marker exists")).expect("the marker is JSON");
    assert_eq!(marker, json!({"pass_started_at": pass_started_at.to_isoformat_text()}));
    assert!(!marker_path.with_extension("json.tmp").exists(), "the rename consumes the temporary file");
  }

  #[tokio::test]
  async fn a_file_up_to_the_mtime_slack_before_the_marker_keeps_its_node() {
    let (_home, features) = startup_features().await;
    let session_id = closed_worker_node(&features, "synthetic-slack-node", 0).await;
    let meta = features.store.load_meta(&session_id).await.expect("the meta loads").expect("the node exists");
    let session_dir = features.store.session_dir(&session_id);
    let marker = UtcDateTime::parse("2026-10-18T10:00:00Z").expect("the synthetic marker parses");
    let marker_time = SystemTime::from(marker.into_inner());
    for (before, expected) in
      [(Duration::from_millis(500), Some("file_after_marker")), (Duration::from_millis(1500), None)]
    {
      let stamp = marker_time - before;
      for path in [features.store.chat_events_path(&session_id), features.store.metadata_path(&session_id)] {
        std::fs::File::open(&path).expect("the durable file exists").set_modified(stamp).expect("the mtime is set");
      }

      let boot_marker = BootMarker { pass_started_at: marker, failed_sessions: BTreeSet::new() };
      let source = SkipProofSource::read(
        &session_dir,
        SkipProofSource::metadata_file(&session_dir).expect("the metadata file stats"),
      )
      .expect("the session's log stats")
      .expect("both files exist");
      let reason = match boot_skip_check(&features, &session_id, &meta, &boot_marker, Some(&source))
        .expect("the skip check reads")
      {
        SkipCheck::Keep(reason) => Some(reason),
        SkipCheck::Skip { .. } => None,
      };

      assert_eq!(reason, expected, "a durable file {before:?} before the marker");
    }
  }

  /// The skip check's existence test is the metadata file's own stat, which also gives the proof its metadata identity.
  /// A plain file where a session directory would be, and a directory without a metadata file, are no node; a node
  /// whose log is absent is a node that keeps its pass, as it did before.
  #[tokio::test]
  async fn the_skip_check_takes_existence_and_the_proof_identity_from_one_metadata_stat() {
    let (_home, features) = startup_features().await;
    let sessions = features.config.get().expect("the test config loads").sessions_dir();
    std::fs::create_dir_all(&sessions).expect("the sessions directory exists");
    let plain_file = sessions.join("not-a-session");
    std::fs::write(&plain_file, b"").expect("the plain file writes");
    assert!(
      matches!(examine_session_dir(&features, &plain_file, None, &SkipProof::default()), Ok(WalkEntry::Absent)),
      "a plain file in the sessions directory is no node"
    );
    let empty = sessions.join("empty-session");
    std::fs::create_dir_all(&empty).expect("the empty directory exists");
    assert!(
      matches!(examine_session_dir(&features, &empty, None, &SkipProof::default()), Ok(WalkEntry::Absent)),
      "a directory without a metadata file is no node"
    );
    let session_id = closed_worker_node(&features, "synthetic-logless-node", 0).await;
    std::fs::remove_file(features.store.chat_events_path(&session_id)).expect("the log is removed");
    assert!(
      matches!(
        examine_session_dir(&features, &features.store.session_dir(&session_id), None, &SkipProof::default()),
        Ok(WalkEntry::Kept { .. })
      ),
      "a node whose log is absent keeps its pass"
    );
  }

  /// A skip proof serves a node's verdict while the node's log and metadata keep the identities the row recorded, with
  /// no tail read and no run-record read. The served verdict equals the one the node's own files give. A node changed
  /// after the row was written, and a missing, stale or torn proof, get the verdict from the node's own files.
  #[tokio::test]
  async fn a_skip_proof_serves_its_verdict_only_while_the_node_keeps_its_identities() {
    let (home, features) = startup_features().await;
    let session_id = closed_worker_node(&features, "synthetic-proof-node", 1).await;
    let session_dir = features.store.session_dir(&session_id);
    let meta = features.store.load_meta(&session_id).await.expect("the meta loads").expect("the node exists");
    // The marker follows both files, so the node's own check reads no file as changed.
    let marker = BootMarker {
      pass_started_at: UtcDateTime::parse("2030-01-01T00:00:00Z").expect("the synthetic marker parses"),
      failed_sessions: BTreeSet::new(),
    };
    let source = SkipProofSource::read(
      &session_dir,
      SkipProofSource::metadata_file(&session_dir).expect("the metadata file stats"),
    )
    .expect("the session's log stats")
    .expect("both files exist");

    // The verdict the node's own files give, and what that check reads.
    let tails = features.store.task_state_tail_read_count();
    let runs = features.store.run_record_read_count();
    let own =
      match boot_skip_check(&features, &session_id, &meta, &marker, Some(&source)).expect("the skip check reads") {
        SkipCheck::Keep(reason) => panic!("a closed worker node with ended runs skips, not keeps {reason}"),
        SkipCheck::Skip { newest_backend } => newest_backend,
      };
    let own_tails = features.store.task_state_tail_read_count() - tails;
    let own_runs = features.store.run_record_read_count() - runs;

    // The same verdict from the proof, without either read.
    crate::skip_proof::write(
      home.path(),
      &[SkipProofRow { session_id: session_id.clone(), source, newest_backend: own.clone() }],
    )
    .expect("the proof writes");
    let proof = SkipProof::read(home.path());
    let tails = features.store.task_state_tail_read_count();
    let runs = features.store.run_record_read_count();
    match examine_session_dir(&features, &session_dir, Some(&marker), &proof).expect("the session is examined") {
      WalkEntry::Skipped { newest_backend, .. } => assert_eq!(newest_backend, own, "the proof serves its own verdict"),
      _ => panic!("the proof skips the node"),
    }
    assert_eq!(features.store.task_state_tail_read_count(), tails, "the proof answers without a tail read");
    assert_eq!(features.store.run_record_read_count(), runs, "the proof answers without a run-record read");

    // A node whose log changed after the row was written runs its own check, and reaches the same verdict.
    features
      .store
      .append_chat_event(&session_id, json!({"id": "late", "type": "assistant", "content": "x"}))
      .await
      .expect("the synthetic event appends");
    let tails = features.store.task_state_tail_read_count();
    match examine_session_dir(&features, &session_dir, Some(&marker), &proof).expect("the session is examined") {
      WalkEntry::Skipped { newest_backend, .. } => assert_eq!(newest_backend, own),
      _ => panic!("a changed node still skips on its own files"),
    }
    assert_eq!(features.store.task_state_tail_read_count(), tails + 1, "a changed node reads its own tail");
    assert!(own_tails > 0 && own_runs > 0, "the reference check read the node's files: {own_tails} {own_runs}");

    // A missing or torn proof serves nothing, so each node reads its own files.
    std::fs::remove_file(charliebot_core::home::boot_skip_proof_path(home.path())).expect("the proof is removed");
    let missing = SkipProof::read(home.path());
    match examine_session_dir(&features, &session_dir, Some(&marker), &missing).expect("the session is examined") {
      WalkEntry::Skipped { .. } => {}
      _ => panic!("a node without a proof keeps the walk's own verdict"),
    }
    std::fs::write(charliebot_core::home::boot_skip_proof_path(home.path()), b"{\"version\": 1}\nnode\t1\t2")
      .expect("the torn proof writes");
    let torn = SkipProof::read(home.path());
    assert!(torn.served(&session_id, &source).is_none(), "a torn row serves nothing");
    match examine_session_dir(&features, &session_dir, Some(&marker), &torn).expect("the session is examined") {
      WalkEntry::Skipped { .. } => {}
      _ => panic!("a torn proof leaves the walk's own verdict"),
    }
    // A proof whose row names the pre-change identities serves nothing either: the row is stale for this node.
    crate::skip_proof::write(
      home.path(),
      &[SkipProofRow { session_id: session_id.clone(), source, newest_backend: own.clone() }],
    )
    .expect("the stale proof writes");
    let stale = SkipProof::read(home.path());
    let changed = SkipProofSource::read(
      &session_dir,
      SkipProofSource::metadata_file(&session_dir).expect("the metadata file stats"),
    )
    .expect("the session's log stats")
    .expect("both files exist");
    assert_eq!(stale.served(&session_id, &changed), None, "a row of other identities serves nothing");
    match examine_session_dir(&features, &session_dir, Some(&marker), &stale).expect("the session is examined") {
      WalkEntry::Skipped { .. } => {}
      _ => panic!("a stale proof leaves the walk's own verdict"),
    }
  }
}
