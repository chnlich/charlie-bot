//! The rounds of a scheduled task: what is in flight in its bound session, the round a bound delegate starts, and the
//! readers that `runtime.tasks` reads them through.
//!
//! A round is the worker that a task's fire or a `--bound-task` delegation started, under the bound session, and the
//! trigger input that a fire queued there. Both are read from persisted state, so a restart keeps a round in flight.

use charliebot_core::config::CharlieBotConfig;
use charliebot_core::event_types;
use charliebot_registry::FeatureContext;
use charliebot_registry::registry::{
  BoundRoundReaders, ScheduledBinding, ScheduledKind, ScheduledRound, ScheduledRoundStart,
};
use charliebot_store::CompactValue;
use charliebot_store::metadata::{TaskApprover, TaskSpec, UtcDateTime};
use charliebot_store::run_records::{HostBoot, RunRecord};
use futures_util::future::BoxFuture;

use crate::config::{ScheduledTaskConfig, TaskMode};
use crate::firing::{input_prefix, round_request_id, round_request_task};
use crate::steps::StepMember;
use crate::{host_gate, loader, steps};

/// The reader set that `runtime.tasks` reads scheduled tasks through, registered once by this crate.
pub(crate) fn bound_round_readers() -> BoundRoundReaders {
  BoundRoundReaders { binding: binding_reader, round: round_reader, round_in_flight: round_in_flight_reader }
}

/// The member kinds that the improve launcher accepts, one per line. The launch script, the remote runner, the login
/// readback and this check all read this one file.
const MEMBER_KINDS_TEXT: &str = include_str!("../../../../../dev/improve/member_kinds.txt");

/// The member kinds that a keyed round may run, read from the kind list text. A list with no kind is an error.
pub(crate) fn member_kinds() -> anyhow::Result<Vec<&'static str>> {
  parse_member_kinds(MEMBER_KINDS_TEXT)
}

pub(crate) fn parse_member_kinds(text: &str) -> anyhow::Result<Vec<&str>> {
  let kinds = text.lines().map(str::trim).filter(|line| !line.is_empty()).collect::<Vec<_>>();
  anyhow::ensure!(!kinds.is_empty(), "dev/improve/member_kinds.txt lists no member kind");
  Ok(kinds)
}

/// The member kind of a backend type: `claude` for the three types whose command starts with `claude`, `codex` for the
/// codex type, and `None` for any other type, which no keyed round may run on.
pub(crate) fn member_kind(backend_type: &str) -> Option<&'static str> {
  match backend_type {
    "cc-claude" | "cc-anthropic-compatible" | "cc-openai-compatible" => Some("claude"),
    "codex" => Some("codex"),
    _ => None,
  }
}

/// Why a session on `backend` cannot take a fire or a round of a task: the id names no current option or pool id, or,
/// for a task bound with `improve_host`, a member of the backend has no member kind the improve launcher accepts.
/// `None` when the session can take it.
pub(crate) fn session_backend_refusal(
  cfg: &CharlieBotConfig,
  backend: &str,
  keyed: bool,
) -> anyhow::Result<Option<String>> {
  if let Err(reason) = cfg.backends.check_backend_id(backend) {
    return Ok(Some(reason));
  }
  if !keyed {
    return Ok(None);
  }
  let kinds = member_kinds()?;
  let members = match cfg.backends.pool(backend) {
    Some(pool) => pool.members.iter().map(|member| member.backend.as_str()).collect(),
    None => vec![backend],
  };
  for member in members {
    let option = cfg
      .get_backend_option(member)
      .ok_or_else(|| anyhow::anyhow!("backend '{member}' is not a configured backends.options id"))?;
    if !member_kind(&option.backend_type).is_some_and(|kind| kinds.contains(&kind)) {
      return Ok(Some(format!(
        "backend '{member}' (type '{}') has no member kind that the improve launcher accepts; a round runs only claude or codex",
        option.backend_type
      )));
    }
  }
  Ok(None)
}

/// The trigger input of `task` that is still queued in `session_id`, or claimed by a live run there. Returns the input
/// or the run that holds it. Other inputs of the session do not count.
pub(crate) fn trigger_input_in_flight(
  features: &FeatureContext,
  task: &ScheduledTaskConfig,
  session_id: &str,
) -> anyhow::Result<Option<String>> {
  let prefix = input_prefix(task);
  let is_trigger = |event: &CompactValue| event_id(event).starts_with(&prefix);
  let dispatcher = charliebot_tasks::dispatch::dispatcher(features)?;
  if let Some(event) = dispatcher.pending_inputs(session_id)?.iter().find(|event| is_trigger(event)) {
    return Ok(Some(format!("input {}", event_id(event))));
  }
  let runs = features.store.list_run_records_sync(session_id)?;
  let node = features.store.node_state(session_id)?;
  let mut boot = HostBoot::default();
  let mut claiming = Vec::new();
  for run in runs.iter().filter(|run| !run.input_event_ids.is_empty()) {
    if node.display_state(run, &mut boot)?.is_live() {
      claiming.push(run);
    }
  }
  if claiming.is_empty() {
    return Ok(None);
  }
  let events = features.store.load_all_facts(session_id)?;
  for run in claiming {
    let holds_trigger = run
      .input_event_ids
      .iter()
      .any(|input_id| events.iter().any(|event| event_id(event) == input_id.as_str() && is_trigger(event)));
    if holds_trigger {
      return Ok(Some(format!("run {}", run.id)));
    }
  }
  Ok(None)
}

/// The worker session of a round of `task` in flight under `session_id`, or `None`. A round is a worker whose creation
/// fact names the task and which still has a running, queued, or steps-awaiting run.
pub(crate) fn round_in_flight(
  features: &FeatureContext,
  task: &ScheduledTaskConfig,
  session_id: &str,
  cfg: &CharlieBotConfig,
) -> anyhow::Result<Option<String>> {
  let session = features
    .store
    .get_session_cached_or_read_sync(session_id)?
    .ok_or_else(|| anyhow::anyhow!("scheduled task '{}' bound session {session_id} does not exist", task.name))?;
  for leaf_id in features.store.child_ids(session_id)? {
    if leaf_has_live_run(features, &leaf_id, task, cfg, &session.backend)?
      && round_task_of(features, &leaf_id)?.as_deref() == Some(&task.name)
    {
      return Ok(Some(leaf_id));
    }
  }
  Ok(None)
}

/// The task spec of a round of a prompt task: its prompt, with the session note (when given) under a `Session note`
/// heading. The repo and base branch come from the task file; the approver is the user, and no review runs. The
/// `improve_host` of the task file goes into the spec, so the round's venue is the task's host on every path that
/// starts a round.
pub(crate) fn round_task_spec(task: &ScheduledTaskConfig, note: Option<&str>) -> TaskSpec {
  let mut goal = task.prompt.clone().unwrap_or_default();
  if let Some(note) = note.filter(|note| !note.is_empty()) {
    goal.push_str("\n\n## Session note\n\n");
    goal.push_str(note);
    goal.push('\n');
  }
  TaskSpec {
    goal,
    repo_path: task.repo.clone(),
    base_branch: None,
    task_type: None,
    keep_worktree: false,
    cold_read: None,
    approver: TaskApprover::User,
    improve_host: task.improve_host.clone(),
  }
}

/// The kind of a scheduled task, from its one source and its mode.
pub(crate) fn scheduled_kind(task: &ScheduledTaskConfig) -> ScheduledKind {
  if task.handler.is_some() {
    ScheduledKind::Handler
  } else if task.steps.is_some() {
    ScheduledKind::Steps
  } else if task.mode == Some(TaskMode::Master) {
    ScheduledKind::Master
  } else {
    ScheduledKind::Prompt
  }
}

/// Whether a worker session's runs keep a round in flight: a run is running or queued, or its steps chain awaits the
/// next step. A withheld run is neither, so a run that never launched does not block the task.
fn leaf_has_live_run(
  features: &FeatureContext,
  leaf_id: &str,
  task: &ScheduledTaskConfig,
  cfg: &CharlieBotConfig,
  session_backend: &str,
) -> anyhow::Result<bool> {
  let store = &features.store;
  let runs: Vec<RunRecord> = store.list_run_records_sync(leaf_id)?;
  let node = store.node_state(leaf_id)?;
  let mut boot = HostBoot::default();
  for run in &runs {
    if node.display_state(run, &mut boot)?.is_live() {
      return Ok(true);
    }
  }
  let facts = store.run_facts(leaf_id)?;
  Ok(
    store.task_state(leaf_id)? == "open"
      && steps::steps_chain_awaits_next_step(task, cfg, session_backend, &runs, &facts)?,
  )
}

/// The task a worker session was created for, read from its creation fact. `None` for a session no round created.
fn round_task_of(features: &FeatureContext, session_id: &str) -> anyhow::Result<Option<String>> {
  let events = features.store.load_all_facts(session_id)?;
  let created =
    events.iter().find(|event| event.get("type").and_then(CompactValue::as_str) == Some(event_types::TASK_CREATED));
  Ok(
    created
      .and_then(|event| event.get("request_id"))
      .and_then(CompactValue::as_str)
      .and_then(round_request_task)
      .map(str::to_string),
  )
}

/// The id of a task input. An admitted input takes its `input_id` as its event id, so a trigger input's id is its
/// firing reference.
fn event_id(event: &CompactValue) -> &str {
  event.get("id").and_then(CompactValue::as_str).unwrap_or_default()
}

fn find_task<'a>(snapshot: &'a crate::loader::CronSnapshot, name: &str) -> Option<&'a ScheduledTaskConfig> {
  snapshot.tasks.iter().find(|task| task.name == name)
}

fn binding_reader<'a>(
  features: &'a FeatureContext,
  name: &'a str,
) -> BoxFuture<'a, anyhow::Result<Option<ScheduledBinding>>> {
  Box::pin(async move {
    let cfg = features.config.get()?;
    let (snapshot, _reloaded) = loader::read_snapshot(features, &cfg)?;
    let Some(task) = find_task(&snapshot, name) else { return Ok(None) };
    let kind = scheduled_kind(task);
    let host_hold = if kind == ScheduledKind::Prompt { host_gate::hold_reason(features, task).await? } else { None };
    Ok(Some(ScheduledBinding { session_id: task.session_id.clone(), kind, has_repo: task.repo.is_some(), host_hold }))
  })
}

fn round_reader<'a>(
  features: &'a FeatureContext,
  name: &'a str,
  note: Option<&'a str>,
) -> BoxFuture<'a, anyhow::Result<ScheduledRoundStart>> {
  Box::pin(async move {
    let cfg = features.config.get()?;
    let (snapshot, _reloaded) = loader::read_snapshot(features, &cfg)?;
    let task =
      find_task(&snapshot, name).ok_or_else(|| anyhow::anyhow!("scheduled task '{name}' is not a valid task file"))?;
    let session_id =
      task.session_id.as_deref().ok_or_else(|| anyhow::anyhow!("scheduled task '{name}' has no session binding"))?;
    let session = features
      .store
      .load_meta(session_id)
      .await?
      .ok_or_else(|| anyhow::anyhow!("scheduled task '{name}' bound session {session_id} does not exist"))?;
    let configured = session.backend;
    if let Some(reason) = session_backend_refusal(&cfg, &configured, task.improve_host.is_some())? {
      return Ok(ScheduledRoundStart::Refused(reason));
    }
    let option = match steps::select_step_member(features, &cfg, &configured, &[])? {
      StepMember::Selected(option) => option,
      StepMember::Exhausted(message) => return Ok(ScheduledRoundStart::Refused(message)),
    };
    let session_backend = cfg.backends.pool(&configured).is_some().then(|| configured.clone());
    Ok(ScheduledRoundStart::Ready(Box::new(ScheduledRound {
      request_id: round_request_id(task, &UtcDateTime::now().to_isoformat_text()),
      task_spec: round_task_spec(task, note),
      session_backend,
      run_backend: option.id.clone(),
      model: steps::option_model(&option),
    })))
  })
}

fn round_in_flight_reader(features: &FeatureContext, name: &str, session_id: &str) -> anyhow::Result<Option<String>> {
  let cfg = features.config.get()?;
  let (snapshot, _reloaded) = loader::read_snapshot(features, &cfg)?;
  let task =
    find_task(&snapshot, name).ok_or_else(|| anyhow::anyhow!("scheduled task '{name}' is not a valid task file"))?;
  round_in_flight(features, task, session_id, &cfg)
}
