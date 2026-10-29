//! The startup reconcile of task-tree nodes (`src/runtime/task_recovery.py`, `task_execution.py:2668-2914`), the
//! end-landing retry, and the let-go step at shutdown.
//!
//! A node pass runs its steps in Python's order: an interrupted prompt edit lands its fact; stop requests win; a
//! launched run re-attaches (live) or drains (dead); the follow-ups of terminal runs replay once; undelivered reports
//! are recovered; pending inputs dispatch. A replay step whose owning task has not landed fails the pass with an error
//! that names the owner; the call sites are listed in `README.md`.

use std::collections::BTreeMap;
#[cfg(feature = "test-hooks")]
use std::collections::HashSet;
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
use charliebot_registry::context::{FeatureContext, StartupStop};
use charliebot_registry::sequence::{
  SequenceCompletion, SequenceController, SequenceDispatch, SequenceExecutor, SequenceSessionEvents,
  SequenceTreeRuntime, StoreSequenceTree,
};
use charliebot_store::ChatEvent;
use charliebot_store::metadata::{SessionMetadata, TaskProfile, TaskType, UtcDateTime};
use charliebot_store::run_records::{
  self, RAW_LOG_NAME, RunKind, RunOutcome, RunOutcomeKind, RunRecord, run_not_found_in_task_text,
};
use charliebot_store::{PromptScope, Store, StoreError, StoreResult, layout};
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
use crate::spawner_prompt;

pub use crate::execution::{RUN_END_LANDING_RETRY_INTERVAL, closed_task_stop_request_id};
const DRIVEN_RUN_RELEASE_POLL_INTERVAL: Duration = Duration::from_millis(10);
const BOOT_RECOVERY_MARKER_PATH: &str = "state/boot_recovery.json";
/// A durable file whose mtime is no earlier than the marker minus this slack keeps its node
/// (`src/runtime/task_recovery.py`, `MARKER_MTIME_SLACK_SECONDS`). The kernel stamps `st_mtime` from its coarse clock,
/// which can lag the fine-clock pass start by about two ticks (~2 ms at 500 Hz), so the slack must exceed that lag.
const MARKER_MTIME_SLACK_SECONDS: f64 = 1.0;

fn boot_recovery_marker_path(home: &std::path::Path) -> PathBuf {
  home.join(BOOT_RECOVERY_MARKER_PATH)
}

/// The last clean pass's start time, or None when the marker is absent or unreadable. None makes the pass check every
/// node in full (`src/runtime/task_recovery.py:58-74`). Other read errors, such as a permission error, propagate.
fn read_boot_recovery_marker(home: &std::path::Path) -> anyhow::Result<Option<UtcDateTime>> {
  let path = boot_recovery_marker_path(home);
  let raw = match std::fs::read(&path) {
    Ok(raw) => raw,
    Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
    Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
  };
  match parse_boot_recovery_marker(&raw) {
    Ok(pass_started_at) => Ok(Some(pass_started_at)),
    Err(error_type) => {
      tracing::error!(target: "charliebot_tasks", size = raw.len(), error_type, "boot_recovery_marker_unreadable");
      Ok(None)
    }
  }
}

/// The pass start that a marker's bytes hold. On failure, the Python exception name that the boot logs as `error_type`.
/// Python logs only that name, so the parser's own message is not kept.
fn parse_boot_recovery_marker(raw: &[u8]) -> Result<UtcDateTime, &'static str> {
  let text = std::str::from_utf8(raw).map_err(|_| "UnicodeDecodeError")?;
  let payload: Value = serde_json::from_str(text).map_err(|_| "JSONDecodeError")?;
  let Value::Object(fields) = payload else {
    return Err("TypeError");
  };
  match fields.get("pass_started_at") {
    None => Err("KeyError"),
    Some(Value::String(pass_started_at)) => UtcDateTime::parse(pass_started_at).map_err(|_| "ValueError"),
    Some(_) => Err("TypeError"),
  }
}

fn write_boot_recovery_marker(home: &std::path::Path, pass_started_at: UtcDateTime) -> anyhow::Result<()> {
  let path = boot_recovery_marker_path(home);
  let parent = path.parent().context("boot-recovery marker has no parent directory")?;
  std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
  let payload = serde_json::json!({"pass_started_at": pass_started_at.to_python_isoformat()});
  let text = charliebot_core::pyfmt::json_dumps(&payload, &charliebot_core::pyfmt::PYTHON_JSON_DEFAULT);
  charliebot_core::fsutil::atomic_write_text_synced(&path, &text)
    .with_context(|| format!("writing {}", path.display()))?;
  Ok(())
}

fn boot_skip_reason(
  features: &FeatureContext,
  session_id: &str,
  session_dir: &std::path::Path,
  meta: &SessionMetadata,
  marker: UtcDateTime,
) -> anyhow::Result<Option<&'static str>> {
  if features.store.task_state_fast(session_id)? == "open" {
    return Ok(Some("state_open"));
  }

  let marker_instant = marker.into_inner();
  let marker_seconds = charliebot_core::timefmt::python_float_seconds(
    marker_instant.unix_timestamp(),
    i64::from(marker_instant.nanosecond()),
  );
  for path in [layout::chat_events_path(session_dir), session_dir.join(run_records::METADATA_NAME)] {
    let modified = match std::fs::metadata(&path) {
      Ok(metadata) => metadata.modified().with_context(|| format!("reading mtime of {}", path.display()))?,
      Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Some("file_after_marker")),
      Err(error) => return Err(error).with_context(|| format!("stating {}", path.display())),
    };
    let modified = OffsetDateTime::from(modified);
    let modified_seconds =
      charliebot_core::timefmt::python_float_seconds(modified.unix_timestamp(), i64::from(modified.nanosecond()));
    if modified_seconds >= marker_seconds - MARKER_MTIME_SLACK_SECONDS {
      return Ok(Some("file_after_marker"));
    }
  }

  let runs = features.store.list_run_records_sync(session_id)?;
  for run in &runs {
    if run.ended_at.is_none() {
      return Ok(Some("run_unended"));
    }
    if run.sequence_ref.is_some() {
      return Ok(Some("sequence_ref"));
    }
    if let Some(worktree_path) = run.worktree_path.as_deref().filter(|path| !path.is_empty()) {
      // Python's os.path.lexists (src/runtime/task_recovery.py:119) reads any OSError as absent, so the pass
      // continues past a path that cannot be stat'ed. The warning keeps that error visible.
      match std::fs::symlink_metadata(worktree_path) {
        Ok(_) => return Ok(Some("worktree_exists")),
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
    return Ok(Some("cold_read_failed"));
  }
  Ok(None)
}

/// The counters of one boot pass, logged with `task_tree_recovery_done`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PassCounters {
  pub nodes: u64,
  pub resumed: u64,
  pub drained: u64,
  pub followups: u64,
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

/// The startup recovery step (`_run_task_recovery`): a pass error is logged and the server starts anyway, as Python's
/// `_run_task_recovery` does. The adapter is a wiring fact, so its absence fails startup.
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

/// On SIGINT and bind failure, cancel each run launch and then each session consumer after `server.py:293-310`, as
/// `asyncio.run` does (`asyncio/runners.py:66-75`). A cancelled launch leaves its run for the next boot
/// (`TaskExecutionAdapter::let_go_launches`); each running turn follows `master_cc_run.py:648-664`. The launches go
/// first so that a manager launch never reaches the stopped queue. SIGTERM skips this step because Uvicorn re-raises it
/// first (`uvicorn/server.py:322-339`). A failure of the first half still runs the second.
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

/// Reconcile every node this home owns, in sorted directory order (`task_recovery.reconcile_task_tree`). The sequence
/// controllers mark their interrupted state first.
pub async fn reconcile_task_tree(
  features: &Arc<FeatureContext>,
  adapter: &TaskExecutionAdapter,
) -> anyhow::Result<PassCounters> {
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
  let mut failed_node = false;

  let tree = sequence_tree(features, adapter)?;
  for controller in features.registry.sequence_controllers() {
    if stop.is_cancelled() {
      return Ok(counters);
    }
    controller.reconcile_interrupted(&tree).await?;
  }

  let mut session_dirs = std::fs::read_dir(&sessions_dir)
    .with_context(|| format!("listing {}", sessions_dir.display()))?
    .map(|entry| entry.map(|entry| entry.path()))
    .collect::<io::Result<Vec<PathBuf>>>()
    .with_context(|| format!("listing {}", sessions_dir.display()))?;
  session_dirs.sort();
  tracing::info!(target: "charliebot_tasks", sessions = session_dirs.len(), "task_tree_recovery_walk_started");
  for session_dir in session_dirs {
    if stop.is_cancelled() {
      return Ok(counters);
    }
    if !session_dir.is_dir() || !session_dir.join(run_records::METADATA_NAME).is_file() {
      continue;
    }
    let session_id = session_dir
      .file_name()
      .and_then(|name| name.to_str())
      .with_context(|| format!("session directory name is not UTF-8: {}", session_dir.display()))?
      .to_string();
    let Some(meta) = features.store.load_meta(&session_id).await? else {
      continue;
    };
    if stop.is_cancelled() {
      return Ok(counters);
    }
    counters.nodes += 1;
    if let Some(marker) = marker {
      if let Some(reason) = boot_skip_reason(features, &session_id, &session_dir, &meta, marker)? {
        *counters.keep_reasons.entry(reason.to_string()).or_default() += 1;
      } else {
        if let Some(newest) = features.store.newest_run_record_sync(&session_id)?
          && let Some(backend) = newest.backend.as_deref()
        {
          features.store.note_run_backend(&session_id, backend);
        }
        counters.skipped += 1;
        continue;
      }
    }
    if stop.is_cancelled() {
      return Ok(counters);
    }
    counters.full += 1;
    // A session with no task boundary, no run directory and no prompt ref has nothing to reconcile: its pass can
    // only report the absence. `has_no_task_node` proves that from the raw event bytes, so the pass here reports
    // the same absence without folding the log. A probe that cannot read the session's files fails the node, as a
    // node pass that cannot read them does.
    let no_node = features.store.has_no_task_node(&meta);
    if stop.is_cancelled() {
      return Ok(counters);
    }
    let result = match no_node {
      Ok(true) => Some(Err(anyhow::Error::new(charliebot_store::task_node_absence::not_a_task_tree_node(&session_id)))),
      Ok(false) => {
        reconcile_node_until_stopped(
          &stop,
          reconcile_node_inner(features, adapter, &session_id, &mut counters, None, Some(&stop)),
        )
        .await
      }
      Err(error) => Some(Err(anyhow::Error::new(error))),
    };
    let Some(result) = result else {
      return Ok(counters);
    };
    if let Err(error) = result {
      tracing::error!(target: "charliebot_tasks", session_id = %session_id, error = %format!("{error:#}"), "task_recovery_node_failed");
      failed_node = true;
      // An out-of-space pass enters the end-landing retry: it re-runs the pass until a round survives.
      if is_out_of_space_error(error.as_ref()) {
        adapter.start_run_end_landing_retry(&session_id, None);
      }
    }
  }
  if !failed_node && !stop.is_cancelled() {
    write_boot_recovery_marker(&cfg.charliebot_home, started_at)?;
  }
  counters.marker = marker.map(UtcDateTime::to_python_isoformat);
  Ok(counters)
}

async fn reconcile_node_until_stopped<T>(stop: &StartupStop, node_pass: impl Future<Output = T>) -> Option<T> {
  tokio::select! {
    result = node_pass => Some(result),
    _ = stop.cancelled() => None,
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
  reconcile_node_inner(features, adapter, session_id, counters, is_driven, None).await
}

async fn reconcile_node_inner(
  features: &Arc<FeatureContext>,
  adapter: &TaskExecutionAdapter,
  session_id: &str,
  counters: &mut PassCounters,
  is_driven: Option<&(dyn Fn(&RunRecord) -> bool + Sync)>,
  stop: Option<&StartupStop>,
) -> anyhow::Result<()> {
  #[cfg(feature = "test-hooks")]
  adapter.recovery.reconcile_node_calls.fetch_add(1, Ordering::SeqCst);

  let store = &features.store;
  if store.load_meta(session_id).await?.is_none() {
    return Ok(());
  }
  // Steps 1 and 2 read Run facts from this snapshot. Only the closed-task stop refreshes it, where `_reconcile_node`
  // reloads its event list.
  let Some(mut runs) = store.run_facts_until(session_id, &mut || stop.is_some_and(StartupStop::is_cancelled))? else {
    return Ok(());
  };
  let Some(run_records) =
    store.list_run_records_until(session_id, &mut || stop.is_some_and(StartupStop::is_cancelled))?
  else {
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
    if let Some(locked_meta) = store.load_meta(session_id).await? {
      for scope in [PromptScope::Subtree, PromptScope::Node] {
        store.ensure_prompt_changed_fact_locked(&guard, session_id, &locked_meta, scope).await?;
      }
    }
  }

  // --- 1. stop requests take precedence over any launch or follow ---------
  for run in &run_records {
    if driven(run) || runs.terminal_outcome(&run.id)?.is_some() {
      continue;
    }
    if runs.stop_requested(&run.id, None) {
      store.reconcile_stop_request(session_id, &run.id).await?;
      run_records_changed = true;
    }
  }
  // A run on a non-open node without a stop fact cannot continue dispatch or hold the node open.
  if store.node_facts(session_id)?.task()?.task_state != "open" {
    for run in &run_records {
      if runs.terminal_outcome(&run.id)?.is_some() || runs.stop_requested(&run.id, None) {
        continue;
      }
      store.request_stop(session_id, &run.id, &closed_task_stop_request_id(&run.id)).await?;
      let Some(updated_runs) =
        store.run_facts_until(session_id, &mut || stop.is_some_and(StartupStop::is_cancelled))?
      else {
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
    } else {
      counters.drained += 1;
      tracing::warn!(target: "charliebot_tasks", session_id, run_id = %run.id, "task_recovery_drain");
      let ended = Arc::new(|| false) as ResumeLiveness;
      adapter.resume_run(session_id, &run.id, Some(ended)).await?;
      run_records_changed = true;
    }
  }

  // --- 3. terminal runs replay their missing follow-up once ----------------
  if run_records_changed {
    let Some(refreshed_run_records) =
      store.list_run_records_until(session_id, &mut || stop.is_some_and(StartupStop::is_cancelled))?
    else {
      return Ok(());
    };
    replay_followups_with_records(features, adapter, session_id, counters, &refreshed_run_records, stop).await?;
  } else {
    // Python refreshes the records in `_replay_followups` (`task_execution.py:2846-2864`). Reusing this sorted
    // snapshot is equivalent when steps 1 and 2 made no record changes, avoiding another run-directory scan per node.
    replay_followups_with_records(features, adapter, session_id, counters, &run_records, stop).await?;
  }
  if stop.is_some_and(StartupStop::is_cancelled) {
    return Ok(());
  }
  // --- 4. pending reports and pending inputs --------------------------------
  adapter.dispatch.recover_pending_reports(session_id).await?;
  adapter.dispatch.dispatch_pending(session_id).await?;
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
  replay_followups_with_records(features, adapter, session_id, counters, &run_records, None).await?;
  Ok(())
}

/// Replay from the current run-record snapshot, shared with the preceding reconcile steps when they changed no
/// records. Python loads this list separately in `_replay_followups` (`task_execution.py:2846-2864`).
async fn replay_followups_with_records(
  features: &Arc<FeatureContext>,
  adapter: &TaskExecutionAdapter,
  session_id: &str,
  counters: &mut PassCounters,
  run_records: &[RunRecord],
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
          // The owning task's controller is not ported, so no terminal fact is written for this run. The interrupted
          // finish below is Python's branch for a package that is gone.
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
    if meta.profile == TaskProfile::Manager && run.kind == RunKind::ManagerTurn {
      // The durable write landed and the crash came before after_run_finished: the owner-close recheck replays once.
      counters.followups += 1;
      match stop {
        Some(stop) => {
          if completion.recheck_close_requests_until(session_id, &run.id, stop).await?.is_none() {
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
            && adapter.maybe_spawn_review(session_id, run).await?.is_none()
          {
            let cfg = features.config.get()?;
            let pool = charliebot_agents::backend_pool::backend_pool(features)?;
            let summary = format!(
              "review of work run {} found no available reviewer backend; backends.pool: {}",
              run.id,
              pool.recovery_report(&cfg)
            );
            adapter.report_failure_to_parent(session_id, run, "blocked", Some(&summary)).await?;
          }
        }
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
    dispatch::note_run_liveness(store, session_id, &run, true).await;
    None
  } else {
    // The drain rule: the process ended unseen, so its end time is the raw log's last write.
    let raw_path = store.run_dir(session_id, run_id).join(RAW_LOG_NAME);
    transport_recovery::raw_completion_time(&raw_path).map(to_utc).transpose()?
  };
  let cfg = adapter.features.config.get().context("reading the configuration for a re-attach")?;
  let option = adapter.resolve_run_backend(&cfg, &run)?;
  if meta.profile == TaskProfile::Worker && crate::prompts::WORKER_KINDS.contains(&run_kind_name(run.kind)) {
    adapter.resume_worker_run(&meta, &run, &option, is_alive, ended_at).await?;
    return Ok(false);
  }
  if meta.profile != TaskProfile::Manager || run.kind != RunKind::ManagerTurn {
    bail!(
      "run {run_id} (profile={}, kind={}) has no resume adapter",
      profile_name(meta.profile),
      run_kind_name(run.kind)
    );
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
        // A resumed turn records no input ids: task_execution._resume_manager_turn builds its
        // MasterRunRecord without them, and master_cc_queue.py only sets master_done input ids when
        // the item has them.
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
        tracing::warn!(target: "charliebot_tasks", session_id, run_id, error = %format!("{error:#}"), "manager_resume_follow_ended");
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

/// The sequence kinds whose controllers Python registers but the port has not landed: a run of one of these kinds fails
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

fn run_kind_name(kind: RunKind) -> &'static str {
  match kind {
    RunKind::ManagerTurn => "manager_turn",
    RunKind::Work => "work",
    RunKind::Review => "review",
    RunKind::Iteration => "iteration",
    RunKind::ScheduledStep => "scheduled_step",
  }
}

fn profile_name(profile: TaskProfile) -> &'static str {
  match profile {
    TaskProfile::Manager => "manager",
    TaskProfile::Worker => "worker",
  }
}

/// A file time as the store's UTC timestamp, truncated to microseconds as Python's datetime is.
fn to_utc(value: OffsetDateTime) -> anyhow::Result<UtcDateTime> {
  let micros = value.replace_nanosecond(value.nanosecond() / 1_000 * 1_000)?;
  let text = micros.format(&Rfc3339)?;
  UtcDateTime::parse(&text).with_context(|| format!("converting the file time {text}"))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::execution::pre_recovery_step;
  use charliebot_agents::backend_pool::BackendPool;
  use charliebot_core::credentials::load_credentials;
  use charliebot_core::hot_reload::HotReload;
  use charliebot_registry::store_hooks::store_hooks;
  use charliebot_registry::{Registry, StartupState};
  use charliebot_store::metadata::TaskSpec;
  use charliebot_store::run_records::SequenceRef;
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
    // Python: tests/test_task_recovery.py::test_retry_round_skips_runs_this_process_already_drives.
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
    startup.insert(BackendPool::default()).expect("the pool inserts once");
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

  #[tokio::test]
  async fn boot_recovery_retains_the_parsed_event_list_for_later_reads() {
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
    assert_eq!(after_recovery.file_reads - before.file_reads, 1, "recovery parses this event file once");
    let events = features.store.load_chat_events_shared_sync(&node.id).unwrap();
    assert!(!events.is_empty());
    assert_eq!(
      features.store.history_work().file_reads,
      after_recovery.file_reads,
      "the post-boot read reuses its one cached list"
    );
  }

  #[tokio::test]
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
    let task = features
      .store
      .create_task(
        CreateTask {
          request_id: request_id.to_string(),
          task_parent_id: None,
          profile: TaskProfile::Worker,
          task: Some(TaskSpec {
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
        repo_path: Some("/synthetic/repo".to_string()),
        ..run(&run_id, RunKind::Work, None)
      };
      features.store.register_run_locked(&guard, &task.id, run, None).await.expect("the synthetic run registers");
      features
        .store
        .record_finish_locked(&guard, &task.id, &run_id, RunOutcome::new(RunOutcomeKind::Success))
        .await
        .expect("the synthetic run finishes");
    }
    features
      .store
      .append_control_fact(&guard, &task.id, json!({"type": "task_closed", "outcome": "completed"}))
      .await
      .expect("the synthetic node closes");
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

  /// Python: `test_marker_write_fsyncs_the_temp_file_before_the_rename`.
  #[cfg(feature = "test-hooks")]
  #[test]
  fn the_marker_write_syncs_the_temporary_file_once_before_the_rename() {
    let home = tempfile::tempdir().expect("temporary directory exists");
    let pass_started_at = UtcDateTime::parse("2026-10-21T03:00:00.123456+00:00").expect("the synthetic time parses");
    charliebot_core::fsutil::clear_synced_write_steps();

    write_boot_recovery_marker(home.path(), pass_started_at).expect("the marker writes");

    let steps = charliebot_core::fsutil::synced_write_steps();
    assert_eq!(steps, ["sync", "rename"], "the temporary file syncs once, then the rename replaces the marker");
    let marker_path = home.path().join("state").join("boot_recovery.json");
    let marker: Value =
      serde_json::from_slice(&std::fs::read(&marker_path).expect("the marker exists")).expect("the marker is JSON");
    assert_eq!(marker, json!({"pass_started_at": pass_started_at.to_python_isoformat()}));
    assert!(!marker_path.with_extension("json.tmp").exists(), "the rename consumes the temporary file");
  }

  /// Python: `test_mtime_slack_bounds_the_marker_comparison`.
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

      let reason = boot_skip_reason(&features, &session_id, &session_dir, &meta, marker).expect("the skip check reads");

      assert_eq!(reason, expected, "a durable file {before:?} before the marker");
    }
  }
}
