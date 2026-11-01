//! The v2 improve loop: one worker child task, iteration Runs, one final report.
//!
//! The loop keeps its state under `sessions/<id>/loops/<loop_id>/` (`goal.md`, re-read at every iteration, the optional
//! `plan.md`, and `state.json`). Each iteration is one Run on the loop's worker child, `kind = iteration`, with
//! `sequence_ref = (improve, owner_ref = <loop dir>, position = <n>)`. Each judged iteration is delivered to the parent
//! as one child report; the loop's end is one final report.
//!
//! The close step closes the child once under `improve:<loop id>:close`. The status is `completed` for a landed
//! merge-back, `cancelled` for a user stop, and `blocked` when the iterations ran out without proof. It is `failed` for
//! a quota or loop failure.
//!
//! Re-entry is stable: the child id derives from (parent, `improve:<loop id>`), and each iteration Run id from (child,
//! `improve:<loop id>:iter:<n>`). A replayed admission or finalization never duplicates either. After a restart the
//! loop is never resumed: the startup pass marks it `interrupted`, clears its lock and closes what the crash left open.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use charliebot_agents::backend_pool::BackendPool;
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::event_types::{RUN_FINISHED, TASK_CLOSED};
use charliebot_core::repo_root::RepoRoot;
use charliebot_core::str_methods::take_chars;
use charliebot_registry::sequence::SequenceTree;
use charliebot_store::compact::CompactValue;
use charliebot_store::metadata::{SessionMetadata, TaskProfile, TaskSpec, TaskType};
use charliebot_store::run_records::{RUN_EVENTS_NAME, RunKind, RunRecord, SequenceRef};
use charliebot_store::{CreateTask, SharedChatEvent, Store, StoreError, StoreResult, TaskCaller, control_events};
use charliebot_tasks::completion::RUN_REF_PREFIX;
use charliebot_tasks::execution::TaskExecutionAdapter;
use charliebot_tasks::{git, prompts, spawner_prompt};
use serde_json::{Map, Value, json};

use crate::command::{self, ImproveState, LoopStatus};
use crate::judgment::{
  ITERATION_SUMMARIES_HEADING, LOOP_FAILURE_ERROR_PREFIX, RUNNER_FALLBACK_REPORT_MARKER, ReportValidity,
  WORKTREE_CREATE_ERROR_PREFIX,
};
use crate::landing;

/// The sequence kind every iteration Run carries.
pub const SEQUENCE_KIND: &str = "improve";
/// The per-iteration progress event: chat visibility only.
pub const IMPROVE_ITERATION_COMPLETED: &str = "improve_iteration_completed";
/// The final report's type when the loop completed.
pub const IMPROVE_COMPLETED: &str = "improve_completed";
/// The final report's type when the loop failed or was blocked.
pub const IMPROVE_FAILED: &str = "improve_failed";
/// The final report's type when a stop request ended the loop.
pub const IMPROVE_STOPPED: &str = "improve_stopped";
/// The fixed loop parameters one controller run carries.
pub struct LoopSpec<'a> {
  pub session_id: &'a str,
  pub loop_id: i64,
  pub iterations: i64,
  pub child_id: &'a str,
  pub goal: &'a str,
}

/// Where a loop's iteration runs: the shared worktree's facts, pinned on each iteration Run.
struct Pin<'a> {
  backend: &'a str,
  model: Option<&'a str>,
  repo_path: &'a str,
  base_branch: &'a str,
  work_branch: &'a str,
  worktree_path: &'a str,
}

/// The loop position an iteration's report reads.
pub fn loop_owner_ref(cfg: &CharlieBotConfig, session_id: &str, loop_id: i64) -> String {
  command::loop_dir(cfg, session_id, loop_id).to_string_lossy().into_owned()
}

/// The stable create request id of one loop's worker child.
pub fn improve_child_request_id(loop_id: i64) -> String {
  format!("improve:{loop_id}")
}

/// The stable run request id of one iteration: a replayed finalization binds to the same Run.
pub fn iteration_run_request_id(loop_id: i64, iteration: i64) -> String {
  format!("improve:{loop_id}:iter:{iteration}")
}

/// The close request id every call of one loop's close step binds to.
pub fn improve_close_request_id(loop_id: i64) -> String {
  format!("improve:{loop_id}:close")
}

/// The stable id of one loop's worker child under its manager.
pub fn improve_child_id(session_id: &str, loop_id: i64) -> String {
  control_events::stable_task_id(Some(session_id), &improve_child_request_id(loop_id))
}

/// Create, or re-admit, the loop's one worker child under the manager. The child records the loop's resolved backend
/// and carries the goal as its task text with no task type: the improve loop is its own deliverable kind.
pub async fn create_improve_child(
  store: &Store,
  session_id: &str,
  loop_id: i64,
  goal: &str,
  repo_path: Option<&str>,
  base_branch: Option<&str>,
  backend: Option<&str>,
) -> StoreResult<SessionMetadata> {
  let request = CreateTask {
    request_id: improve_child_request_id(loop_id),
    task_parent_id: Some(session_id.to_string()),
    profile: TaskProfile::Worker,
    task: Some(TaskSpec {
      goal: goal.to_string(),
      repo_path: repo_path.map(str::to_string),
      base_branch: base_branch.map(str::to_string),
      task_type: None,
      ..TaskSpec::default()
    }),
    name: Some(format!("Improve loop {loop_id}")),
    backend: backend.map(str::to_string),
    group: None,
    session_id: None,
  };
  // The creation fact's actor is "agent": the task is created through the agent caller, whose runtime gate passes
  // here because the route has checked the manager's authorization and the child's parent is that manager.
  store.create_task(request, TaskCaller::Agent { session_id: session_id.to_string() }, Map::new()).await
}

/// The iteration report contract, rendered from the worker prompt's `iteration_reports` section.
pub fn render_iteration_reports(repo_root: &RepoRoot, loop_dir: &str, iteration: i64) -> anyhow::Result<String> {
  let sections = spawner_prompt::load_marker_sections(
    &repo_root.path().join(spawner_prompt::WORKER_PROMPT_PATH),
    &["iteration_reports"],
    "worker-prompt",
  )?;
  let tokens = vec![
    ("{{loop_dir}}", loop_dir.to_string()),
    ("{{iteration_number_padded}}", format!("{iteration:04}")),
    ("{{iteration_number}}", iteration.to_string()),
  ];
  Ok(spawner_prompt::substitute_tokens(&sections["iteration_reports"], &tokens))
}

/// One iteration's full launch context: the session facts, the shared-worktree bindings, the loop's description and the
/// report contract. A Run without its pinned worktree facts is refused rather than given a worktree of its own.
pub fn iteration_context(
  repo_root: &RepoRoot,
  meta: &SessionMetadata,
  run: &RunRecord,
  seq: &SequenceRef,
  description: &str,
) -> anyhow::Result<String> {
  let (Some(repo_path), Some(base_branch), Some(branch), Some(worktree)) =
    (&run.repo_path, &run.base_branch, &run.branch_name, &run.worktree_path)
  else {
    bail!(
      "iteration run {} is missing its pinned shared-worktree provenance (repo/base/branch/worktree); refusing to create a divergent worktree",
      run.id
    );
  };
  let mut parts = vec![prompts::render_session_info(repo_root, &meta.name)?];
  parts.push(prompts::render_worktree_bindings(
    repo_root,
    TaskType::Implement,
    &prompts::binding_intro(repo_root, run)?,
    branch,
    &format!("`{base_branch}`"),
    worktree,
    repo_path,
  )?);
  parts.push(prompts::render_task_body(repo_root, description)?);
  parts.push(render_iteration_reports(repo_root, &seq.owner_ref, seq.position)?);
  Ok(parts.into_iter().filter(|part| !part.is_empty()).collect::<Vec<_>>().join("\n\n"))
}

/// The iteration description: the goal, the plan when one exists, and the previous iterations' summaries.
pub fn compose_iteration_description(goal: &str, plan: Option<&str>, previous_summaries: &[String]) -> String {
  let mut parts = vec![goal.to_string()];
  if let Some(plan) = plan {
    parts.push(format!("Plan:\n{plan}"));
  }
  if !previous_summaries.is_empty() {
    parts.push(format!("{ITERATION_SUMMARIES_HEADING}{}", previous_summaries.join("\n\n")));
  }
  parts.join("\n")
}

/// Register one iteration Run with its shared-worktree provenance, before its launch. Stable by (child, request id):
/// a replayed registration returns the original Run.
async fn register_iteration_run(
  tree: &dyn SequenceTree,
  spec: &LoopSpec<'_>,
  cfg: &CharlieBotConfig,
  iteration: i64,
  pin: &Pin<'_>,
) -> anyhow::Result<RunRecord> {
  let run_id = control_events::stable_run_id(spec.child_id, &iteration_run_request_id(spec.loop_id, iteration));
  if let Some(existing) = tree.get_run(spec.child_id, &run_id).await? {
    return Ok(existing);
  }
  let record = RunRecord {
    kind: RunKind::Iteration,
    backend: Some(pin.backend.to_string()),
    model: pin.model.map(str::to_string),
    repo_path: Some(pin.repo_path.to_string()),
    base_branch: Some(pin.base_branch.to_string()),
    branch_name: Some(pin.work_branch.to_string()),
    worktree_path: Some(pin.worktree_path.to_string()),
    sequence_ref: Some(SequenceRef {
      kind: SEQUENCE_KIND.to_string(),
      owner_ref: loop_owner_ref(cfg, spec.session_id, spec.loop_id),
      position: iteration,
    }),
    ..empty_run_record(run_id.clone(), spec.child_id)
  };
  {
    let guard = tree.control_lock().await;
    tree.register_run_locked(&guard, spec.child_id, record, None).await?;
  }
  tree.get_run(spec.child_id, &run_id).await?.context("the registered iteration run reads back")
}

/// A run record with every field empty, for the fields a caller sets.
fn empty_run_record(id: String, session_id: &str) -> RunRecord {
  RunRecord {
    id,
    session_id: session_id.to_string(),
    kind: RunKind::default(),
    input_event_ids: Vec::new(),
    task_spec_hash: None,
    task_spec_ref: None,
    prompt_snapshot_ref: None,
    retry_of_run_id: None,
    review_of_run_id: None,
    backend: None,
    model: None,
    native_session_id: None,
    pid: None,
    pid_start: None,
    started_at: None,
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

/// The failed iteration's quota blocker and summary, from its own events log.
fn iteration_blocker(
  tree: &dyn SequenceTree,
  child_id: &str,
  run_id: &str,
  iteration: i64,
  outcome: &str,
) -> anyhow::Result<(Option<String>, String)> {
  let events_path = tree.run_dir(child_id, run_id).join(RUN_EVENTS_NAME);
  if !events_path.is_file() {
    return Ok((None, format!("Iteration {iteration} {outcome} (no events log).")));
  }
  crate::judgment::failed_iteration_judgments_from_events(
    crate::judgment::newest_first_event_iter(&events_path)?,
    iteration,
    outcome,
  )
}

/// The one iteration's mechanical judgment and the evidence its report carries.
struct IterationJudgment {
  summary: String,
  validity: ReportValidity,
  tip: String,
  commits_added: i64,
  report_path: PathBuf,
}

/// The loop's progress the controller carries through its iterations.
struct Progress {
  goal: String,
  previous_summaries: Vec<String>,
  /// The blocked iteration, its reason and its summary.
  blocked: Option<(i64, String, String)>,
  completed_iterations: i64,
}

/// Run the improve sequence on the loop's child. Every iteration launches through the shared adapter and is awaited to
/// its terminal fact; a failure ends the loop with a final report that names it. The close step runs at every end.
pub async fn run_improve_sequence(
  spec: &LoopSpec<'_>,
  cfg: &CharlieBotConfig,
  tree: &dyn SequenceTree,
  repo_root: &RepoRoot,
  pool: &BackendPool,
  adapter: &TaskExecutionAdapter,
) {
  let mut progress =
    Progress { goal: spec.goal.to_string(), previous_summaries: Vec::new(), blocked: None, completed_iterations: 0 };
  if let Err(error) = run_loop(spec, cfg, tree, repo_root, pool, adapter, &mut progress).await {
    tracing::error!(
      target: "charliebot_improve",
      session = spec.session_id,
      loop_id = spec.loop_id,
      error = format!("{error:#}").as_str(),
      "improve_sequence_failed"
    );
    settle_failure(spec, cfg, tree, &progress, &error).await;
  }
}

/// The failure path: the state is marked failed with the controller's error, the lock is released, the final report
/// names the error, and the close step runs.
async fn settle_failure(
  spec: &LoopSpec<'_>,
  cfg: &CharlieBotConfig,
  tree: &dyn SequenceTree,
  progress: &Progress,
  error: &anyhow::Error,
) {
  let loaded = command::load_loop_state(cfg, spec.session_id, spec.loop_id);
  match loaded {
    Ok(Some(mut state)) => {
      state.status = LoopStatus::Failed;
      let controller_error = format!("Controller error: {error:#}");
      state.end_error = Some(match state.end_error.take() {
        Some(end_error) if !end_error.is_empty() => format!("{end_error} {controller_error}"),
        _ => controller_error,
      });
      if let Err(save_error) = command::save_loop_state(cfg, spec.session_id, &state) {
        tracing::error!(target: "charliebot_improve", session = spec.session_id, error = format!("{save_error:#}").as_str(), "improve_sequence_state_save_failed");
      }
    }
    Ok(None) => {}
    Err(load_error) => {
      tracing::error!(target: "charliebot_improve", session = spec.session_id, error = format!("{load_error:#}").as_str(), "improve_sequence_state_load_failed");
    }
  }
  if let Err(clear_error) = command::clear_active_loop_lock(cfg, spec.session_id) {
    tracing::error!(target: "charliebot_improve", session = spec.session_id, error = format!("{clear_error:#}").as_str(), "improve_sequence_lock_clear_failed");
  }
  let report = json!({
    "type": IMPROVE_FAILED,
    "goal": progress.goal,
    "error": format!("{LOOP_FAILURE_ERROR_PREFIX}{error:#}"),
    "iterations_completed": progress.completed_iterations,
  });
  if let Err(report_error) = tree.deliver_to_successor(spec.session_id, report).await {
    tracing::error!(target: "charliebot_improve", session = spec.session_id, error = %report_error, "improve_sequence_failure_report_failed");
  }
  close_ended_loop_child(tree, cfg, spec.session_id, spec.loop_id).await;
}

/// The controller's main path. Each early return has settled the loop before it returns.
async fn run_loop(
  spec: &LoopSpec<'_>,
  cfg: &CharlieBotConfig,
  tree: &dyn SequenceTree,
  repo_root: &RepoRoot,
  pool: &BackendPool,
  adapter: &TaskExecutionAdapter,
  progress: &mut Progress,
) -> anyhow::Result<()> {
  let state = command::require_loop_state(cfg, spec.session_id, spec.loop_id)?;
  let loop_dir = command::loop_dir(cfg, spec.session_id, spec.loop_id);
  let resolved_repo = PathBuf::from(&state.repo_path);
  let work_branch = state.work_branch.clone();
  let merge_back = state.merge_back;
  let executor = tree.executor().context("task execution adapter is not installed; cannot run an improve sequence")?;

  let wt_path = PathBuf::from(&cfg.paths.worktree_dir).join(work_branch.replace('/', "-"));
  let base_branch = match create_worktree(cfg, spec, &state, &resolved_repo, &wt_path).await {
    Ok(canonical) => canonical,
    Err(error) => {
      let mut failed = state.clone();
      failed.status = LoopStatus::Failed;
      failed.end_error = Some(format!("{WORKTREE_CREATE_ERROR_PREFIX}{error:#}"));
      command::save_loop_state(cfg, spec.session_id, &failed)?;
      command::clear_active_loop_lock(cfg, spec.session_id)?;
      tracing::error!(target: "charliebot_improve", session = spec.session_id, loop_id = spec.loop_id, error = format!("{error:#}").as_str(), "improve_sequence_worktree_failed");
      tree
        .deliver_to_successor(
          spec.session_id,
          json!({
            "type": IMPROVE_FAILED,
            "goal": spec.goal,
            "error": format!("{WORKTREE_CREATE_ERROR_PREFIX}{error:#}"),
          }),
        )
        .await?;
      close_ended_loop_child(tree, cfg, spec.session_id, spec.loop_id).await;
      return Ok(());
    }
  };

  let mut next_pool_member = None;
  for iteration in 1..=spec.iterations {
    let state = command::require_loop_state(cfg, spec.session_id, spec.loop_id)?;
    if state.status == LoopStatus::Stopped {
      break;
    }
    // The live goal and plan are re-read every iteration, so a mid-loop edit steers the next one.
    progress.goal = command::read_loop_goal(&loop_dir)?;
    let plan = command::read_loop_plan(&loop_dir)?;
    let description = compose_iteration_description(&progress.goal, plan.as_deref(), &progress.previous_summaries);
    let tip_before = git::git_rev_parse(&wt_path, "HEAD").await.unwrap_or_default();

    let (iteration_backend, iteration_model) = if state.backend_from_pool {
      let member = match next_pool_member.take() {
        Some(member) => Some(member),
        None => pool.select(cfg, "improve", &[])?,
      };
      match member {
        Some(member) => {
          let model = member.required_model("pool member").map_err(anyhow::Error::msg)?;
          (member.id.clone(), Some(model))
        }
        None => {
          progress.blocked = Some((iteration, pool.no_member_message(cfg), String::new()));
          break;
        }
      }
    } else {
      (state.backend.clone().unwrap_or_default(), state.model.clone())
    };
    let base = base_branch.clone();
    let worktree = wt_path.to_string_lossy().into_owned();
    let repo = resolved_repo.to_string_lossy().into_owned();
    let pin = Pin {
      backend: &iteration_backend,
      model: iteration_model.as_deref(),
      repo_path: &repo,
      base_branch: &base,
      work_branch: &work_branch,
      worktree_path: &worktree,
    };
    let run = register_iteration_run(tree, spec, cfg, iteration, &pin).await?;
    let mut outcome = tree.terminal_outcome_of(spec.child_id, &run.id).await?;
    if outcome.is_none() {
      // The iteration's full context is composed here, where the repo root is in hand: the controller's launch hook
      // receives no startup state. A replayed registration of a live Run is followed, never relaunched.
      let context = {
        let meta = tree.load_meta(spec.child_id).await?.context("the loop's child has no metadata")?;
        let seq = run.sequence_ref.as_ref().context("an iteration run carries its sequence_ref")?;
        iteration_context(repo_root, &meta, &run, seq, &description)?
      };
      let settlement = executor.launch_and_settle(spec.child_id, &run.id, Some(&context)).await?;
      if let Some(reason) = settlement.withheld {
        settle_withheld_iteration(spec, cfg, tree, progress, iteration, &run.id, &reason).await?;
        return Ok(());
      }
      outcome = settlement.outcome;
    }
    let outcome = outcome.context("the iteration's launch settled with no outcome")?;
    let run =
      tree.read_run_sync(spec.child_id, &run.id)?.context("the iteration run disappeared after its launch settled")?;
    progress.completed_iterations = iteration;

    if outcome != "success" {
      let (blocker_reason, failed_summary) = iteration_blocker(tree, spec.child_id, &run.id, iteration, &outcome)?;
      let quota_failure = adapter.take_quota_failure(spec.child_id, &run.id);
      if let Some(reason) = blocker_reason {
        if state.backend_from_pool {
          pool.cool(&iteration_backend, quota_failure.flatten());
          if quota_failure.is_some() && run.pid.is_none() && iteration < spec.iterations {
            match pool.select(cfg, "improve", &[])? {
              Some(member) => next_pool_member = Some(member),
              None => {
                progress.blocked = Some((iteration, reason, failed_summary));
                break;
              }
            }
          }
        } else {
          tracing::warn!(
            target: "charliebot_improve",
            session = spec.session_id,
            loop_id = spec.loop_id,
            iteration,
            reason = reason.as_str(),
            "improve_sequence_blocked"
          );
          progress.blocked = Some((iteration, reason, failed_summary));
          break;
        }
      }
    }

    let judgment = judge_iteration(tree, spec, &run.id, iteration, &wt_path, &loop_dir, &tip_before).await?;
    progress.previous_summaries.push(judgment.summary.clone());
    broadcast_iteration_progress(tree, spec, &run.id, iteration, &outcome, &judgment).await?;
    deliver_iteration_report(tree, spec, &run, iteration, &outcome, &judgment).await?;
  }

  let state = command::require_loop_state(cfg, spec.session_id, spec.loop_id)?;
  let stopped_by_user = state.status == LoopStatus::Stopped;
  let mut merge_result = None;
  let mut end_error = None;
  if progress.blocked.is_none() && !stopped_by_user {
    let landing = landing::land_work_branch_after_loop(
      &resolved_repo,
      &work_branch,
      &base_branch,
      merge_back,
      stopped_by_user,
      progress.previous_summaries.len(),
      spec.session_id,
    )
    .await;
    merge_result = landing.merge_result;
    end_error = landing.end_error;
  }

  let mut state = state;
  if let Some((iteration, reason, _)) = &progress.blocked {
    state.status = LoopStatus::Failed;
    state.blocked_iteration = Some(*iteration);
    state.blocked_reason = Some(reason.clone());
  } else if stopped_by_user {
    state.status = LoopStatus::Stopped;
  } else if merge_result.as_ref().and_then(|merge| merge.get("merged")) == Some(&Value::Bool(true)) {
    state.status = LoopStatus::Completed;
  } else {
    // Exhausted iterations without proven delivery: never a fabricated success.
    state.status = LoopStatus::Blocked;
  }
  if let Some(end_error) = end_error {
    state.end_error = Some(end_error);
  }
  command::save_loop_state(cfg, spec.session_id, &state)?;
  command::clear_active_loop_lock(cfg, spec.session_id)?;

  let payload_type = match state.status {
    LoopStatus::Completed => IMPROVE_COMPLETED,
    LoopStatus::Stopped => IMPROVE_STOPPED,
    LoopStatus::Failed | LoopStatus::Blocked => IMPROVE_FAILED,
    LoopStatus::Running | LoopStatus::Interrupted => {
      bail!("improve loop {} ended without a terminal status {:?}", spec.loop_id, state.status)
    }
  };
  let mut payload = build_summary_payload(payload_type, &progress.goal, &progress.previous_summaries);
  if let Some((iteration, reason, summary)) = &progress.blocked {
    payload.insert("blocked_iteration".to_string(), json!(iteration));
    payload.insert("reason".to_string(), json!(reason));
    payload.insert("blocked_summary".to_string(), json!(take_chars(summary, 500)));
  }
  payload.insert("work_branch".to_string(), json!(work_branch));
  payload.insert("base_branch".to_string(), json!(base_branch));
  if let Some(merge) = merge_result {
    payload.insert("merge_result".to_string(), merge);
  }
  tree.deliver_to_successor(spec.session_id, Value::Object(payload)).await?;
  close_ended_loop_child(tree, cfg, spec.session_id, spec.loop_id).await;
  Ok(())
}

/// Create the loop's one worktree and pin its canonical base on the state. Returns the canonical base branch.
async fn create_worktree(
  cfg: &CharlieBotConfig,
  spec: &LoopSpec<'_>,
  state: &ImproveState,
  resolved_repo: &Path,
  wt_path: &Path,
) -> anyhow::Result<String> {
  std::fs::create_dir_all(&cfg.paths.worktree_dir).context("create the worktree directory")?;
  let base = match state.base_branch.as_deref().filter(|branch| !branch.is_empty()) {
    Some(branch) => branch.to_string(),
    None => git::git_current_branch(resolved_repo).await?,
  };
  let resolution = git::git_create_worktree(resolved_repo, &base, &state.work_branch, wt_path, None).await?;
  let mut saved = state.clone();
  saved.base_branch = Some(resolution.canonical.clone());
  command::save_loop_state(cfg, spec.session_id, &saved)?;
  Ok(resolution.canonical)
}

/// Settle a loop whose iteration launch was withheld: no terminal fact will arrive, so the state is blocked, the lock is
/// released and the failed report names the reason. The close step then stops the queued Run and closes the child.
async fn settle_withheld_iteration(
  spec: &LoopSpec<'_>,
  cfg: &CharlieBotConfig,
  tree: &dyn SequenceTree,
  progress: &Progress,
  iteration: i64,
  run_id: &str,
  reason: &str,
) -> anyhow::Result<()> {
  let mut state = command::require_loop_state(cfg, spec.session_id, spec.loop_id)?;
  state.status = LoopStatus::Blocked;
  command::save_loop_state(cfg, spec.session_id, &state)?;
  command::clear_active_loop_lock(cfg, spec.session_id)?;
  tracing::warn!(
    target: "charliebot_improve",
    session = spec.session_id,
    loop_id = spec.loop_id,
    iteration,
    run_id,
    reason,
    "improve_sequence_launch_withheld"
  );
  let mut payload = build_summary_payload(IMPROVE_FAILED, &progress.goal, &progress.previous_summaries);
  payload.insert("blocked_iteration".to_string(), json!(iteration));
  payload.insert("reason".to_string(), json!(reason));
  payload.insert("withheld_run_id".to_string(), json!(run_id));
  payload.insert("iterations_requested".to_string(), json!(spec.iterations));
  tree.deliver_to_successor(spec.session_id, Value::Object(payload)).await?;
  close_ended_loop_child(tree, cfg, spec.session_id, spec.loop_id).await;
  Ok(())
}

/// The payload the final report and the failure reports share.
fn build_summary_payload(payload_type: &str, goal: &str, summaries: &[String]) -> Map<String, Value> {
  let mut payload = Map::new();
  payload.insert("type".to_string(), json!(payload_type));
  payload.insert("goal".to_string(), json!(goal));
  payload.insert("iterations_completed".to_string(), json!(summaries.len()));
  payload.insert("summaries".to_string(), json!(summaries));
  payload
}

/// The mechanical iteration judgment: report validity over the commit delta the iteration added to the worktree.
/// When the worker wrote no report, the verdict is decided first and the fallback file is written after it.
async fn judge_iteration(
  tree: &dyn SequenceTree,
  spec: &LoopSpec<'_>,
  run_id: &str,
  iteration: i64,
  wt_path: &Path,
  loop_dir: &Path,
  tip_before: &str,
) -> anyhow::Result<IterationJudgment> {
  let report_path = loop_dir.join(format!("iter_{iteration:04}.md"));
  let (tip_after, commits_added) = landing::commit_delta(wt_path, tip_before).await;
  if !report_path.exists() {
    let mut fallback = format!("Iteration {iteration} finished without a report file.");
    let events_path = tree.run_dir(spec.child_id, run_id).join(RUN_EVENTS_NAME);
    if events_path.is_file() {
      fallback = crate::judgment::extract_iteration_summary_from_events(
        crate::judgment::newest_first_event_iter(&events_path)?,
        iteration,
        "finished",
      )?;
    }
    std::fs::write(&report_path, format!("{RUNNER_FALLBACK_REPORT_MARKER}{fallback}"))
      .with_context(|| format!("write {}", report_path.display()))?;
    return Ok(IterationJudgment {
      summary: fallback,
      validity: ReportValidity::Invalid("no report file"),
      tip: tip_after,
      commits_added,
      report_path,
    });
  }
  let validity = crate::judgment::iter_report_validity(&report_path, iteration, commits_added)?;
  let summary = if validity.is_valid() {
    take_chars(&std::fs::read_to_string(&report_path).with_context(|| format!("read {}", report_path.display()))?, 500)
  } else {
    crate::judgment::invalid_iteration_summary(
      iteration,
      validity.reason().unwrap_or_default(),
      commits_added,
      tip_before,
      &tip_after,
      &report_path,
    )
  };
  Ok(IterationJudgment { summary, validity, tip: tip_after, commits_added, report_path })
}

/// The per-iteration progress event: chat visibility only. The parent's input is the child report.
async fn broadcast_iteration_progress(
  tree: &dyn SequenceTree,
  spec: &LoopSpec<'_>,
  run_id: &str,
  iteration: i64,
  status: &str,
  judgment: &IterationJudgment,
) -> anyhow::Result<()> {
  let total = spec.iterations;
  tree
    .deliver_to_successor(
      spec.session_id,
      json!({
        "type": IMPROVE_ITERATION_COMPLETED,
        "iteration": iteration,
        "total_iterations": total,
        "status": status,
        "summary": take_chars(&judgment.summary, 200),
        "report_path": judgment.report_path.to_string_lossy(),
        "child_session_id": spec.child_id,
        "run_id": run_id,
      }),
    )
    .await?;
  Ok(())
}

/// The one per-iteration report: the parent's wake and audit input. Its source event carries the `improve-iteration:`
/// prefix over this Run's own `run_finished` event id, so it never collides with the final report's id.
async fn deliver_iteration_report(
  tree: &dyn SequenceTree,
  spec: &LoopSpec<'_>,
  run: &RunRecord,
  iteration: i64,
  outcome: &str,
  judgment: &IterationJudgment,
) -> anyhow::Result<()> {
  let Some(meta) = tree.load_meta(spec.child_id).await? else {
    tracing::warn!(target: "charliebot_improve", session = spec.session_id, child = spec.child_id, "improve_sequence_report_no_parent");
    return Ok(());
  };
  let Some(parent) = meta.task_parent_id.clone() else {
    tracing::warn!(target: "charliebot_improve", session = spec.session_id, child = spec.child_id, "improve_sequence_report_no_parent");
    return Ok(());
  };
  let events = tree.load_events_sync(spec.child_id)?;
  let finished = events
    .iter()
    .rev()
    .find(|event| {
      event.get("type").and_then(CompactValue::as_str) == Some(RUN_FINISHED)
        && event.get("run_id").and_then(CompactValue::as_str) == Some(run.id.as_str())
    })
    .with_context(|| {
      format!(
        "improve loop {} iteration {iteration} (run {}) has no run_finished event to source its report from",
        spec.loop_id, run.id
      )
    })?;
  let finished_id = finished.get("id").and_then(CompactValue::as_str).unwrap_or_default();
  let mut header = format!(
    "[Improve loop {} · iteration {iteration}/{}] report_valid={}",
    spec.loop_id,
    spec.iterations,
    if judgment.validity.is_valid() { "true" } else { "false" }
  );
  if let Some(reason) = judgment.validity.reason() {
    header.push_str(&format!(" [invalid: {reason}]"));
  }
  header.push_str(&format!(
    " tip={} commits_added={} report={} Audit per the improve-goal skill.",
    judgment.tip,
    judgment.commits_added,
    judgment.report_path.display()
  ));
  let result_refs = vec![format!("loop:{}", spec.loop_id), format!("{RUN_REF_PREFIX}{}", run.id)];
  tree
    .dispatch()
    .deliver_child_report(
      spec.child_id,
      json!({"id": format!("improve-iteration:{finished_id}")}).into(),
      outcome,
      &format!("{header}\n\n{}", judgment.summary),
      Some(result_refs.as_slice()),
      Some(&parent),
    )
    .await?;
  tracing::info!(
    target: "charliebot_improve",
    session = spec.session_id,
    child = spec.child_id,
    loop_id = spec.loop_id,
    iteration,
    outcome,
    "improve_sequence_iteration_report_delivered"
  );
  Ok(())
}

/// Close the loop's worker child once the loop has ended. It returns while the loop can still progress, and when a
/// launched iteration still owes its terminal fact; that Run's finish runs the step again. A failure is logged and the
/// child stays open for the next call point.
pub async fn close_ended_loop_child(tree: &dyn SequenceTree, cfg: &CharlieBotConfig, session_id: &str, loop_id: i64) {
  if let Err(error) = close_ended_loop_child_inner(tree, cfg, session_id, loop_id).await {
    tracing::error!(
      target: "charliebot_improve",
      session = session_id,
      loop_id,
      error = format!("{error:#}").as_str(),
      "improve_sequence_close_failed"
    );
  }
}

async fn close_ended_loop_child_inner(
  tree: &dyn SequenceTree,
  cfg: &CharlieBotConfig,
  session_id: &str,
  loop_id: i64,
) -> anyhow::Result<()> {
  let Some(state) = command::load_loop_state(cfg, session_id, loop_id)? else {
    return Ok(());
  };
  if state.status == LoopStatus::Running {
    return Ok(());
  }
  if state.server_pid == Some(i64::from(std::process::id())) {
    let active = command::active_lock_path(cfg, session_id);
    let named = if active.is_file() { std::fs::read_to_string(&active)? } else { String::new() };
    if named.trim() == loop_id.to_string() {
      // This process's controller still owns the loop: its own end path runs this step when the loop ends.
      return Ok(());
    }
  }
  let child_id = improve_child_id(session_id, loop_id);
  if tree.load_meta(&child_id).await?.is_none() {
    return Ok(());
  }
  if tree.task_state(&child_id)? != "open" {
    return Ok(());
  }
  let request_id = improve_close_request_id(loop_id);
  let events = tree.load_events_sync(&child_id)?;
  if has_close_fact(&events, &request_id) {
    return Ok(());
  }
  let run_records = tree.list_run_records_sync(&child_id)?;
  for run in &run_records {
    if tree.run_has_terminal_fact(run, &events) || run.pid.is_some() {
      continue;
    }
    // A queued iteration never launches on its own, so the durable stop request settles what would block the close.
    tree.request_stop(&child_id, &run.id, &request_id).await?;
  }
  for run in &run_records {
    if run.pid.is_some() && !tree.run_has_terminal_fact(run, &events) {
      // A launched iteration still owes its terminal fact; its finish runs after_run, which runs this step again.
      return Ok(());
    }
  }
  let (outcome, evidence) = loop_close_verdict(tree, cfg, session_id, &state, &child_id, &run_records, &events).await?;
  match tree.completion().close_sequence_child(&child_id, &request_id, outcome, &evidence).await {
    Ok(_) => {}
    Err(error) => {
      // A raced close either landed under this request id (the loser returns quietly) or met a changed node.
      let events = tree.load_events_sync(&child_id)?;
      if matches!(error, StoreError::Conflict(_)) && has_close_fact(&events, &request_id) {
        return Ok(());
      }
      return Err(error.into());
    }
  }
  tracing::info!(target: "charliebot_improve", session = session_id, child = child_id.as_str(), loop_id, outcome, "improve_sequence_child_closed");
  Ok(())
}

fn has_close_fact(events: &[SharedChatEvent], request_id: &str) -> bool {
  events.iter().any(|event| {
    event.get("type").and_then(CompactValue::as_str) == Some(TASK_CLOSED)
      && event.get("request_id").and_then(CompactValue::as_str) == Some(request_id)
  })
}

/// The loop's outcome and the close evidence, derived from durable records only.
async fn loop_close_verdict(
  tree: &dyn SequenceTree,
  cfg: &CharlieBotConfig,
  session_id: &str,
  state: &ImproveState,
  child_id: &str,
  run_records: &[RunRecord],
  events: &[SharedChatEvent],
) -> anyhow::Result<(&'static str, Value)> {
  let loop_id = state.loop_id;
  let mut terminal = Vec::new();
  for run in run_records {
    if let Some(outcome) = tree.terminal_outcome(events, &run.id) {
      terminal.push((run, outcome));
    }
  }
  let iteration_count = terminal.len();
  let successful_ids: Vec<String> =
    terminal.iter().filter(|(_, outcome)| outcome == "success").map(|(run, _)| run.id.clone()).collect();
  // The blocked iteration the controller recorded. A failed state the controller never stamped falls back to the
  // quota-blocked iteration that the records still prove.
  let mut blocked_iter: Option<(i64, String)> = None;
  if state.status == LoopStatus::Failed {
    if let Some(reason) = &state.blocked_reason {
      blocked_iter = Some((state.blocked_iteration.unwrap_or(0), reason.clone()));
    } else {
      for (run, outcome) in &terminal {
        if outcome == "success" {
          continue;
        }
        let position = run.sequence_ref.as_ref().map_or(0, |seq| seq.position);
        let (reason, _summary) = iteration_blocker(tree, child_id, &run.id, position, outcome)?;
        if let Some(reason) = reason {
          blocked_iter = Some((position, reason));
          break;
        }
      }
    }
  }
  let sentence = match state.status {
    LoopStatus::Completed => format!(
      "Improve loop completed {iteration_count} iteration(s); work branch {} landed on {}.",
      state.work_branch,
      state.base_branch.as_deref().unwrap_or_default()
    ),
    LoopStatus::Blocked => format!(
      "Improve loop ran {iteration_count} iteration(s) on branch {}; the goal is not proven and the sequence delivered no landing. Decide whether to land the branch, continue iterating, or close the task.",
      state.work_branch
    ),
    LoopStatus::Stopped => format!(
      "Improve loop stopped by user after {iteration_count} iteration(s); evidence retained on the loop's task, no further iterations will run."
    ),
    LoopStatus::Failed if blocked_iter.is_some() => {
      let (iteration, reason) = blocked_iter.as_ref().expect("a blocked iteration is present");
      landing::blocked_loop_summary(*iteration, reason)
    }
    LoopStatus::Failed => "Improve loop failed.".to_string(),
    LoopStatus::Interrupted => {
      "Improve loop interrupted by a server restart; the loop is NOT resumed automatically.".to_string()
    }
    LoopStatus::Running => bail!("improve loop {loop_id} has the non-terminal status running"),
  };
  let loop_dir = command::loop_dir(cfg, session_id, loop_id);
  let mut summaries = Vec::new();
  for (run, _outcome) in &terminal {
    let Some(seq) = run.sequence_ref.as_ref() else { continue };
    if blocked_iter.as_ref().is_some_and(|(iteration, _)| *iteration == seq.position) {
      continue;
    }
    let report_path = loop_dir.join(format!("iter_{:04}.md", seq.position));
    if !report_path.is_file() {
      // An iteration the loop never judged has no summary.
      continue;
    }
    let text = std::fs::read_to_string(&report_path).with_context(|| format!("read {}", report_path.display()))?;
    summaries.push(take_chars(text.strip_prefix(RUNNER_FALLBACK_REPORT_MARKER).unwrap_or(&text), 500));
  }
  let detail =
    if summaries.is_empty() { String::new() } else { format!("\n\nIteration summaries:\n{}", summaries.join("\n\n")) };
  // The end error rides after the outcome sentence, before the iteration summaries, verbatim.
  let end_error_part =
    state.end_error.as_deref().filter(|text| !text.is_empty()).map_or(String::new(), |text| format!(" {text}"));
  let base_to_land = state.base_branch.as_deref().filter(|_| state.status == LoopStatus::Completed);
  let landing = match base_to_land {
    Some(base) => git::git_rev_parse(Path::new(&state.repo_path), &state.work_branch).await.map(|commit| {
      json!({
        "branch": format!("origin/{base}"),
        "commit": commit,
        "repo_path": state.repo_path,
      })
    }),
    None => None,
  };
  let evidence = json!({
    "summary": format!("[Improve loop {loop_id}] {sentence}{end_error_part}{detail}"),
    "result_refs": [format!("loop:{loop_id}")],
    "run_ids": successful_ids,
    "review_run_ids": [],
    "landing": landing,
  });
  let outcome = match state.status {
    LoopStatus::Completed => "completed",
    LoopStatus::Blocked => "blocked",
    LoopStatus::Failed | LoopStatus::Interrupted => "failed",
    LoopStatus::Stopped => "cancelled",
    LoopStatus::Running => bail!("improve loop {loop_id} has the non-terminal status running"),
  };
  Ok((outcome, evidence))
}

/// Mark every improve loop whose controller died with the old process, then close what ended. A loop whose state says
/// `running` under this process's pid is left alone: its controller is live. Returns how many loops were marked.
pub async fn reconcile_interrupted_sequences(
  cfg: &CharlieBotConfig,
  tree: &dyn SequenceTree,
  boot_pid: Option<i64>,
) -> anyhow::Result<usize> {
  let stop = tree.startup_stop();
  let pid = boot_pid.unwrap_or_else(|| i64::from(std::process::id()));
  let sessions_dir = cfg.sessions_dir();
  if !sessions_dir.is_dir() {
    return Ok(0);
  }
  let mut repaired = 0;
  let mut session_dirs = Vec::new();
  for entry in std::fs::read_dir(&sessions_dir)? {
    if stop.is_cancelled() {
      return Ok(repaired);
    }
    session_dirs.push(entry?.path());
  }
  session_dirs.sort();
  for session_dir in session_dirs {
    if stop.is_cancelled() {
      return Ok(repaired);
    }
    if !session_dir.is_dir() || !session_dir.join("loops").is_dir() {
      continue;
    }
    let session_id = session_dir.file_name().and_then(|name| name.to_str()).unwrap_or_default().to_string();
    let loops_dir = session_dir.join("loops");
    let mut loop_ids = Vec::new();
    for entry in std::fs::read_dir(&loops_dir)? {
      if stop.is_cancelled() {
        return Ok(repaired);
      }
      let Ok(entry) = entry else { continue };
      if !entry.path().join(command::STATE_FILE).exists() {
        continue;
      }
      if let Some(loop_id) = entry.file_name().to_str().and_then(|name| name.parse::<i64>().ok()) {
        loop_ids.push(loop_id);
      }
    }
    loop_ids.sort_unstable();
    for loop_id in loop_ids {
      if stop.is_cancelled() {
        return Ok(repaired);
      }
      if let Some(mut state) = command::load_loop_state(cfg, &session_id, loop_id)?
        && state.status == LoopStatus::Running
        && state.server_pid != Some(pid)
      {
        let recorded_pid = state.server_pid;
        state.status = LoopStatus::Interrupted;
        command::save_loop_state(cfg, &session_id, &state)?;
        let active = command::active_lock_path(cfg, &session_id);
        if active.exists() {
          std::fs::remove_file(&active).with_context(|| format!("remove {}", active.display()))?;
        }
        repaired += 1;
        tracing::warn!(
          target: "charliebot_improve",
          session = session_id.as_str(),
          loop_id,
          recorded_pid,
          "improve_sequence_interrupted"
        );
        if tree.load_meta(&session_id).await?.is_some() {
          tree
            .deliver_to_successor(
              &session_id,
              json!({
                "type": IMPROVE_FAILED,
                "goal": state.goal,
                "error": format!(
                  "Improve loop {loop_id} was interrupted by a server restart; the loop is NOT resumed automatically (the existing improve boundary). Its state is marked interrupted, the stale lock is cleared, and the launched iteration's terminal fact was reconciled. Restart the loop explicitly if you want it to continue."
                ),
              }),
            )
            .await?;
        }
      }
      // The startup pass is the close step's crash-window repair; it returns by itself for loops still running or busy.
      close_ended_loop_child(tree, cfg, &session_id, loop_id).await;
    }
  }
  Ok(repaired)
}
