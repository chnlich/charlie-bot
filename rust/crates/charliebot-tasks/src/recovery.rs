//! The startup reconcile of task-tree nodes (`src/runtime/task_recovery.py`, `task_execution.py:2668-2914`), the
//! end-landing retry, and the let-go step at shutdown.
//!
//! A node pass runs its steps in Python's order: an interrupted prompt edit lands its fact; stop requests win; a
//! launched run re-attaches (live) or drains (dead); the follow-ups of terminal runs replay once; undelivered reports
//! are recovered; pending inputs dispatch. A replay step whose owning task has not landed fails the pass with an error
//! that names the owner; the call sites are listed in `README.md`.

use std::collections::HashSet;
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail};
use charliebot_agents::runs::run_alive_probe;
use charliebot_agents::transport::{identity, recovery as transport_recovery};
use charliebot_core::config::BackendOption;
use charliebot_registry::context::FeatureContext;
use charliebot_registry::sequence::{
  SequenceCompletion, SequenceController, SequenceDispatch, SequenceExecutor, SequenceSessionEvents,
  SequenceTreeRuntime, StoreSequenceTree,
};
use charliebot_store::metadata::{SessionMetadata, TaskProfile, TaskType, UtcDateTime};
use charliebot_store::run_records::{self, RAW_LOG_NAME, RunKind, RunOutcome, RunOutcomeKind, RunRecord};
use charliebot_store::{PromptScope, Store, StoreError, StoreResult};
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

/// The end-landing retry's cadence: the first round runs at once and later rounds every interval, with no round limit
/// (`task_execution.py`, `RUN_END_LANDING_RETRY_INTERVAL_SECONDS`).
pub const RUN_END_LANDING_RETRY_INTERVAL: Duration = Duration::from_secs(30);
const DRIVEN_RUN_RELEASE_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// The counters of one boot pass, logged with `task_tree_recovery_done`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PassCounters {
  pub nodes: u64,
  pub resumed: u64,
  pub drained: u64,
  pub followups: u64,
}

/// The process-local state of the recovery: the resume follows this process drives, and the nodes that run an
/// end-landing retry. Nothing here is durable; every round re-derives its work from the facts on disk.
#[derive(Default)]
pub(crate) struct RecoveryState {
  pub(crate) resume_follows: Mutex<HashSet<(String, String)>>,
  landing_retries: Mutex<HashSet<String>>,
}

/// The startup recovery step (`_run_task_recovery`): a pass error is logged and the server starts anyway, as Python's
/// `_run_task_recovery` does. The adapter is a wiring fact, so its absence fails startup.
pub fn recovery_step(features: Arc<FeatureContext>) -> BoxFuture<'static, anyhow::Result<()>> {
  Box::pin(async move {
    let adapter = features.startup.require::<TaskExecutionAdapter>()?;
    let started = Instant::now();
    match reconcile_task_tree(&features, &adapter).await {
      Ok(counters) => tracing::info!(
        target: "charliebot_tasks",
        elapsed_ms = started.elapsed().as_millis() as u64,
        nodes = counters.nodes,
        resumed = counters.resumed,
        drained = counters.drained,
        followups = counters.followups,
        "task_tree_recovery_done"
      ),
      Err(error) => {
        tracing::error!(target: "charliebot_tasks", error = %format!("{error:#}"), "task_tree_recovery_failed")
      }
    }
    Ok(())
  })
}

/// The let-go step at shutdown: cancel every per-session manager consumer, so each running turn takes Python's shape
/// (`master_cc_run.py:648-691`). A covered run detaches, and an uncovered run terminates
/// (`master_run::run_cc`). Resumed turns are consumer items, so their follows are cancelled the same way.
pub fn let_go_step(features: Arc<FeatureContext>) -> BoxFuture<'static, anyhow::Result<()>> {
  Box::pin(async move {
    let queue = features.startup.require::<MasterQueue>()?;
    queue.shutdown().await
  })
}

/// Reconcile every node this home owns, in sorted directory order (`task_recovery.reconcile_task_tree`). The sequence
/// controllers mark their interrupted state first.
pub async fn reconcile_task_tree(
  features: &Arc<FeatureContext>,
  adapter: &TaskExecutionAdapter,
) -> anyhow::Result<PassCounters> {
  let cfg = features.config.get().context("reading the configuration for task recovery")?;
  let mut counters = PassCounters::default();
  let sessions_dir = cfg.sessions_dir();
  if !sessions_dir.is_dir() {
    return Ok(counters);
  }

  let tree = sequence_tree(features, adapter)?;
  for controller in features.registry.sequence_controllers() {
    controller.reconcile_interrupted(&tree).await?;
  }

  let mut session_dirs = std::fs::read_dir(&sessions_dir)
    .with_context(|| format!("listing {}", sessions_dir.display()))?
    .map(|entry| entry.map(|entry| entry.path()))
    .collect::<io::Result<Vec<PathBuf>>>()
    .with_context(|| format!("listing {}", sessions_dir.display()))?;
  session_dirs.sort();
  for session_dir in session_dirs {
    if !session_dir.is_dir() || !session_dir.join(run_records::METADATA_NAME).is_file() {
      continue;
    }
    let session_id = session_dir
      .file_name()
      .and_then(|name| name.to_str())
      .with_context(|| format!("session directory name is not UTF-8: {}", session_dir.display()))?
      .to_string();
    if features.store.load_meta(&session_id).await?.is_none() {
      continue;
    }
    counters.nodes += 1;
    if let Err(error) = reconcile_node(features, adapter, &session_id, &mut counters, None).await {
      tracing::error!(target: "charliebot_tasks", session_id = %session_id, error = %format!("{error:#}"), "task_recovery_node_failed");
      // An out-of-space pass enters the end-landing retry: it re-runs the pass until a round survives.
      if is_out_of_space_error(error.as_ref()) {
        adapter.start_run_end_landing_retry(&session_id, None);
      }
    }
  }
  Ok(counters)
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
  let store = &features.store;
  if store.load_meta(session_id).await?.is_none() {
    return Ok(());
  }
  let events = store.load_all_facts(session_id)?;
  let run_records = store.list_run_records_sync(session_id)?;
  // Warm the display-backend map: a fresh boot has seen no liveness notification.
  if let Some(newest) = store.newest_run_record_sync(session_id)?
    && let Some(backend) = newest.backend.as_deref()
  {
    store.note_run_backend(session_id, backend);
  }
  let driven = |run: &RunRecord| is_driven.is_some_and(|driven| driven(run));

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
    if driven(run) || has_terminal_fact(&events, &run.id)? {
      continue;
    }
    if run_records::stop_requested_in_events(&events, &run.id, None) {
      store.reconcile_stop_request(session_id, &run.id).await?;
    }
  }
  // A queued run on a non-open node never launches, and without a stop request it would sit queued forever and block
  // the node's archive. The request id is the launch path's, so both paths settle a run with one fact.
  if store.task_state(session_id)? != "open" {
    for run in &run_records {
      if run.pid.is_some()
        || has_terminal_fact(&events, &run.id)?
        || run_records::stop_requested_in_events(&events, &run.id, None)
      {
        continue;
      }
      store.request_stop(session_id, &run.id, &closed_task_stop_request_id(&run.id)).await?;
    }
  }

  // --- 2. launched, non-terminal runs re-attach or drain -------------------
  for run in &run_records {
    if driven(run) || run.pid.is_none() {
      continue;
    }
    if has_terminal_fact(&events, &run.id)? || run_records::stop_requested_in_events(&events, &run.id, None) {
      continue;
    }
    let boot = identity::read_host_boot_time().context("reading the host boot time for task recovery")?;
    let alive =
      identity::is_run_alive(run.pid, run.pid_start.as_deref(), run.started_at.map(UtcDateTime::into_inner), boot);
    if alive {
      counters.resumed += 1;
      tracing::info!(target: "charliebot_tasks", session_id, run_id = %run.id, "task_recovery_resume");
      // A live manager turn enqueues its follow and returns; the follow is tracked, never awaited.
      resume_run(adapter, session_id, &run.id, None).await?;
    } else {
      counters.drained += 1;
      tracing::warn!(target: "charliebot_tasks", session_id, run_id = %run.id, "task_recovery_drain");
      let ended = Arc::new(|| false) as ResumeLiveness;
      resume_run(adapter, session_id, &run.id, Some(ended)).await?;
    }
  }

  // --- 3. terminal runs replay their missing follow-up once ----------------
  replay_followups(features, adapter, session_id, counters).await?;

  // --- 4. pending reports and pending inputs --------------------------------
  adapter.dispatch.recover_pending_reports(session_id).await?;
  adapter.dispatch.dispatch_pending(session_id).await?;
  Ok(())
}

/// Re-drive every terminal run's follow-up that its crash window lost (`task_execution._replay_followups`). Each
/// follow-up is idempotent by stable ids, so a repeated pass lands nothing twice.
async fn replay_followups(
  features: &Arc<FeatureContext>,
  adapter: &TaskExecutionAdapter,
  session_id: &str,
  counters: &mut PassCounters,
) -> anyhow::Result<()> {
  let store = &features.store;
  let Some(meta) = store.load_meta(session_id).await? else {
    return Ok(());
  };
  let events = store.load_all_facts(session_id)?;
  let run_records = store.list_run_records_sync(session_id)?;
  let task_open = store.task_state(session_id)? == "open";
  let completion = features.startup.require::<TaskCompletion>()?;
  for (run_index, run) in run_records.iter().enumerate() {
    let outcome = run_records::terminal_outcome(&events, &run.id)?;
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
      completion.recheck_close_requests(session_id, &run.id).await?;
      continue;
    }
    let task_type = meta.task.as_ref().and_then(|task| task.task_type);
    if run.kind == RunKind::Work {
      counters.followups += 1;
      if task_type == Some(TaskType::ColdRead) {
        if outcome == RunOutcomeKind::Success {
          completion.after_run_finished(session_id, &run.id).await?;
          bail!(owner_not_landed("cold-read", &format!("the cold-read delivery of run {}", run.id)));
        }
        let later_work = run_records[run_index + 1..].iter().any(|later| later.kind == RunKind::Work);
        if outcome == RunOutcomeKind::Failed && !later_work {
          bail!(owner_not_landed("cold-read", &format!("the cold-read failure chain of run {}", run.id)));
        }
        if outcome == RunOutcomeKind::Interrupted {
          bail!(owner_not_landed("cold-read", &format!("the cold-read interrupted report of run {}", run.id)));
        }
        continue;
      }
      if outcome == RunOutcomeKind::Success {
        // The completion owner's post-success follow-up: the own close-request recheck and the automatic completion.
        completion.after_run_finished(session_id, &run.id).await?;
        if task_type == Some(TaskType::Implement) && run.repo_path.is_some() {
          if let Some(reason) = completion.pause_reason(session_id)? {
            tracing::info!(target: "charliebot_tasks", session_id, run_id = %run.id, reason, "worker_delivery_paused");
          } else if replay_owes_review(task_open, &run_records, run) {
            bail!(owner_not_landed("review-chain", &format!("the review spawn of work run {}", run.id)));
          }
        }
      } else {
        bail!(owner_not_landed("worker-run", &format!("the failure report of work run {} to its parent", run.id)));
      }
    } else if run.kind == RunKind::Review {
      counters.followups += 1;
      bail!(owner_not_landed("review-chain", &format!("the review follow-up of run {}", run.id)));
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
    let Some(run_started) = run.started_at else {
      return false;
    };
    if (other_started, &other.id) > (run_started, &run.id) {
      return false;
    }
  }
  true
}

/// The stop-request id settling a never-launched run on a closed node (`closed_task_stop_request_id`). The launch path
/// and the startup pass derive the same id, so a run both settle carries one `run_stop_requested` fact.
pub fn closed_task_stop_request_id(run_id: &str) -> String {
  format!("closed-task:{run_id}")
}

/// Re-attach one launched run, or drain it when its process is dead (`resume_run`). A manager turn re-enters the
/// per-session queue; the follow is tracked and this call returns before it ends.
pub async fn resume_run(
  adapter: &TaskExecutionAdapter,
  session_id: &str,
  run_id: &str,
  is_alive: Option<ResumeLiveness>,
) -> anyhow::Result<()> {
  let key = (session_id.to_string(), run_id.to_string());
  adapter.recovery.resume_follows.lock().expect("resume follow set is not poisoned").insert(key.clone());
  let result = resume_run_checked(adapter, session_id, run_id, is_alive).await;
  // Ownership passes to the manager follow's tracker only on success; every other exit releases the pair here.
  if !matches!(result, Ok(true)) {
    adapter.recovery.resume_follows.lock().expect("resume follow set is not poisoned").remove(&key);
  }
  result.map(|_| ())
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
    .ok_or_else(|| StoreError::not_found(format!("run {run_id} not found in session {session_id}")))?;
  let (Some(pid), Some(pid_start)) = (run.pid, run.pid_start.clone()) else {
    bail!("run {run_id} records no launched process identity; nothing to re-attach");
  };
  if has_terminal_fact(&store.load_all_facts(session_id)?, run_id)? {
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
  if meta.profile != TaskProfile::Manager || run.kind != RunKind::ManagerTurn {
    bail!(owner_not_landed("worker-run", &format!("re-attaching run {run_id} (kind {})", run_kind_name(run.kind))));
  }
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
    session_id,
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
    let translate = master_run::build_fresh_translate(&adapter.features, &cfg, Some(&option));
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
  tokio::spawn(async move {
    wait_for_triggering_run_release(|| run.as_deref().is_some_and(|run_id| adapter.drives_run(&session, run_id))).await;
    let pass_adapter = adapter.clone();
    let pass_session = session.clone();
    let result = retry_while_out_of_space(RUN_END_LANDING_RETRY_INTERVAL, &session, run.as_deref(), move || {
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

fn has_terminal_fact(events: &[Value], run_id: &str) -> StoreResult<bool> {
  Ok(run_records::terminal_outcome(events, run_id)?.is_some())
}

fn controller_for_sequence(features: &FeatureContext, kind: &str) -> Option<Arc<dyn SequenceController>> {
  features.registry.sequence_controllers().find(|controller| controller.sequence_kind() == kind).cloned()
}

/// The task-tree handle the sequence controllers use: the dispatcher, the completion owner, the store's session events,
/// and the adapter as the launch executor.
fn sequence_tree(features: &Arc<FeatureContext>, adapter: &TaskExecutionAdapter) -> anyhow::Result<StoreSequenceTree> {
  let dispatch: Arc<dyn SequenceDispatch> = Arc::<Dispatcher>::clone(&adapter.dispatch);
  let completion: Arc<dyn SequenceCompletion> = features.startup.require::<TaskCompletion>()?;
  let session_events: Arc<dyn SequenceSessionEvents> = Arc::new(StoreSessionEvents(Arc::clone(&features.store)));
  let executor: Arc<dyn SequenceExecutor> = Arc::new(adapter.clone());
  Ok(StoreSequenceTree::new(
    Arc::clone(&features.store),
    SequenceTreeRuntime { dispatch, completion, session_events, executor: Some(executor) },
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
    event: &'a Value,
    epoch: u64,
  ) -> BoxFuture<'a, StoreResult<()>> {
    Box::pin(async move { self.0.announce_appended_event(session_id, event, epoch).await })
  }
}

/// The sequence kinds whose controllers Python registers but the port has not landed: a run of one of these kinds fails
/// the node pass by name. The owning task is named by the kind. Each task deletes its entry when it registers its
/// controller.
const UNLANDED_SEQUENCE_KINDS: &[&str] = &["improve", "cron"];

/// The error of a replay step whose owning task has not landed: the node pass fails and names the owner.
fn owner_not_landed(owner: &str, step: &str) -> anyhow::Error {
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
  use charliebot_store::run_records::SequenceRef;
  use charliebot_store::{CreateTask, TaskCaller};
  use serde_json::Map;

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
    assert!(!has_terminal_fact(&events, QUEUED_RUN_ID).expect("the terminal fact reads"));
  }

  #[tokio::test]
  async fn a_queued_cron_run_fails_the_node_pass_and_keeps_no_terminal_fact() {
    let (_home, features) = startup_features().await;
    let session_id = queue_sequence_run(&features, "cron", RunKind::ScheduledStep).await;

    let error = node_pass(&features, &session_id).await.expect_err("the cron run has no controller to replay it");

    let message = format!("{error:#}");
    assert!(message.contains(QUEUED_RUN_ID) && message.contains("cron task"), "the error names the owner: {message}");
    let events = features.store.load_all_facts(&session_id).expect("the session facts load");
    assert!(!has_terminal_fact(&events, QUEUED_RUN_ID).expect("the terminal fact reads"));
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
}
