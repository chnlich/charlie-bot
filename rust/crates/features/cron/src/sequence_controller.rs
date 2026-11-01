//! The cron task binding and durable `cron_steps` sequence controller.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

use charliebot_core::config::CharlieBotConfig;
use charliebot_core::event_types;
use charliebot_core::home;
use charliebot_core::repo_root::REPO_ROOT_ENV;
use charliebot_registry::Registry;
use charliebot_registry::context::FeatureContext;
use charliebot_registry::sequence::{SequenceBinding, SequenceController, SequenceTree};
use charliebot_registry::wiring::{Phase, Service, ServiceContext};
use charliebot_store::StoreResult;
use charliebot_store::metadata::{
  SCHEDULE_ALLOW_FAILURE, SCHEDULE_CRON, SCHEDULE_ENABLED, SCHEDULE_NEXT_RUN, SCHEDULE_TIMEZONE, SessionMetadata,
  UtcDateTime,
};
use charliebot_store::run_records::{self, RunKind, RunRecord, SequenceRef};
use chrono::{Datelike, TimeZone, Utc};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value};
use tokio::sync::watch;

use crate::config::{ScheduledTaskConfig, StepConfig};
use crate::{loader, scheduler};

/// A set of task configs bound to the same manager node.
pub struct CronBinding {
  name: String,
  names: Vec<String>,
  registry: watch::Receiver<Option<Weak<Registry>>>,
}

impl CronBinding {
  async fn feature_context(&self) -> anyhow::Result<Arc<FeatureContext>> {
    let mut registry_receiver = self.registry.clone();
    let registry = loop {
      if let Some(registry) = registry_receiver.borrow_and_update().as_ref() {
        break registry
          .upgrade()
          .ok_or_else(|| anyhow::anyhow!("the cron binding registry was dropped before its duties completed"))?;
      }
      registry_receiver
        .changed()
        .await
        .map_err(|_| anyhow::anyhow!("the cron binding registry context service stopped before its duties"))?;
    };
    registry.feature_context().ok_or_else(|| anyhow::anyhow!("the cron binding registry has no live FeatureContext"))
  }
}

impl SequenceBinding for CronBinding {
  fn name(&self) -> &str {
    &self.name
  }

  fn dedicated_backend(&self) -> bool {
    true
  }

  fn backend_lock_detail(&self, target_backend: &str) -> String {
    format!(
      "backend '{target_backend}' cannot be switched to in place: this session is the bound node of scheduled task '{}', whose cron config decides its backend and whose scheduler re-aligns the node to that config on every tick. Edit the task's config in the cron editor, or clone/fork the session with the target backend instead.",
      self.name
    )
  }

  fn on_wake<'a>(
    &'a self,
    meta: &'a SessionMetadata,
    input_events: &'a [Value],
  ) -> BoxFuture<'a, anyhow::Result<Option<String>>> {
    Box::pin(async move {
      let features = self.feature_context().await?;
      let mut cc_session_id = meta.cc_session_id.clone();
      if let (Some(_), Some(started_at)) = (cc_session_id.as_ref(), meta.cc_session_started_at) {
        let now = UtcDateTime::now();
        let instant = now.into_inner();
        let now_chrono = chrono::DateTime::<Utc>::from_timestamp(instant.unix_timestamp(), instant.nanosecond())
          .ok_or_else(|| anyhow::anyhow!("the current time is outside chrono's supported range"))?;
        let cutoff = last_saturday_1am_utc(now_chrono)?;
        if started_at < cutoff && cutoff < now {
          tracing::info!(session = %meta.id, started_at = %started_at, "scheduled_cc_session_expired");
          features.store.clear_cc_session_anchor(&meta.id).await?;
          cc_session_id = None;
          match features.store.archive_events_before(&meta.id, cutoff).await {
            Ok(archive_path) => {
              tracing::info!(session = %meta.id, archive = ?archive_path, "scheduled_session_recycled");
            }
            Err(error) => {
              tracing::error!(session = %meta.id, error = %error, "scheduled_session_recycle_failed");
            }
          }
        }
      }
      let woken_by_report =
        input_events.iter().any(|event| event.get("type").and_then(Value::as_str) == Some(event_types::CHILD_REPORT));
      if woken_by_report && cc_session_id.is_none() {
        return Ok(Some(format!(
          "[Auto-triggered scheduled task result for '{}']\nReview the worker/reviewer results below. Check: was the branch merged? Are there errors? Summarize the outcome.\n\n",
          self.name
        )));
      }
      Ok(None)
    })
  }

  fn on_archive<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move {
      let features = self.feature_context().await?;
      let cfg = features.config.get()?;
      let home = cfg.charliebot_home.clone();
      let names = self.names.clone();
      tokio::task::spawn_blocking(move || {
        for name in &names {
          write_task_key(&home, name, "enabled", Value::Bool(false))?;
        }
        Ok::<(), anyhow::Error>(())
      })
      .await??;
      Ok(())
    })
  }
}

/// The durable `cron_steps` sequence controller.
pub struct CronSequenceController {
  registry: watch::Receiver<Option<Weak<Registry>>>,
  row_schedule_memo: Mutex<Option<RowScheduleMemo>>,
}

#[derive(Clone)]
struct RowScheduleMemo {
  fingerprint: u64,
  session_ids: Vec<String>,
  fields: HashMap<String, Map<String, Value>>,
  earliest_next_fire: Option<UtcDateTime>,
  #[cfg(test)]
  builds: usize,
}

impl CronSequenceController {
  pub fn new(registry: watch::Receiver<Option<Weak<Registry>>>) -> Self {
    Self { registry, row_schedule_memo: Mutex::new(None) }
  }

  fn snapshot_for_listing(&self) -> loader::CronSnapshot {
    if let Some(registry) = self.registry.borrow().as_ref().and_then(Weak::upgrade)
      && let Some(features) = registry.feature_context()
    {
      return match features.config.get() {
        Ok(cfg) => match loader::load_snapshot(&features, &cfg) {
          Ok(snapshot) => snapshot,
          Err(error) => {
            tracing::error!(error = %error, "cron_listing_snapshot_failed");
            loader::CronSnapshot::default()
          }
        },
        Err(error) => {
          tracing::error!(error = %error, "cron_listing_config_failed");
          loader::CronSnapshot::default()
        }
      };
    }

    // The server binds FeatureContext before requests. The path loader keeps isolated registry tests on the same
    // fingerprinted snapshot when they invoke listing hooks without starting that service.
    let home = cron_home();
    let directory = loader::cron_dir_for_home(&home);
    match std::env::var_os(REPO_ROOT_ENV).map(PathBuf::from) {
      Some(repo) => match loader::load_snapshot_for_paths(&directory, &repo) {
        Ok((snapshot, _)) => snapshot,
        Err(error) => {
          tracing::error!(error = %error, "cron_listing_snapshot_failed");
          loader::CronSnapshot::default()
        }
      },
      None => {
        tracing::error!(variable = REPO_ROOT_ENV, "cron_listing_repo_root_missing");
        loader::CronSnapshot { tasks: bound_configs_for_listing(&home), ..loader::CronSnapshot::default() }
      }
    }
  }
}

fn row_schedule_fields(
  memo: &mut Option<RowScheduleMemo>,
  tasks: &[ScheduledTaskConfig],
  fingerprint: u64,
  session_ids: &[String],
  now: UtcDateTime,
) -> HashMap<String, Map<String, Value>> {
  // Match Python's sorted ID set, snapshot fingerprint and earliest-fire expiry (`src/features/cron/sequence_controller.py:181-209`).
  let mut ids = session_ids.to_vec();
  ids.sort();
  ids.dedup();
  if let Some(cached) = memo.as_ref()
    && cached.fingerprint == fingerprint
    && cached.session_ids == ids
    && cached.earliest_next_fire.is_none_or(|expiry| now < expiry)
  {
    return cached.fields.clone();
  }

  #[cfg(test)]
  let builds = memo.as_ref().map_or(1, |cached| cached.builds + 1);

  let mut bound_tasks = HashMap::new();
  for task in tasks {
    if let Some(session_id) = task.session_id.as_deref() {
      bound_tasks.entry(session_id).or_insert(task);
    }
  }
  let mut fields_by_session = HashMap::new();
  let mut earliest_next_fire = None;
  for session_id in &ids {
    // A session with no bound task owns no row fields: the row writer emits `schedule_task: null` itself.
    // An entry holding only that null would make one map per listed session for every listing response.
    let Some(task) = bound_tasks.get(session_id.as_str()).copied() else { continue };
    let mut fields = Map::new();
    fields.insert("schedule_task".to_string(), Value::String(task.name.clone()));
    fields.insert(SCHEDULE_CRON.to_string(), Value::String(task.cron.clone()));
    fields.insert(SCHEDULE_TIMEZONE.to_string(), Value::String(task.timezone.clone()));
    fields.insert(SCHEDULE_ENABLED.to_string(), Value::Bool(task.enabled));
    fields.insert(SCHEDULE_ALLOW_FAILURE.to_string(), Value::Bool(task.allow_failure));
    match scheduler::next_fire_iso(&task.cron, &task.timezone, now) {
      Ok(next) => {
        earliest_next_fire = Some(earliest_next_fire.map_or_else(
          || UtcDateTime::parse(&next).expect("the cron next-fire value is an ISO datetime"),
          |earliest: UtcDateTime| {
            earliest.min(UtcDateTime::parse(&next).expect("the cron next-fire value is an ISO datetime"))
          },
        ));
        fields.insert(SCHEDULE_NEXT_RUN.to_string(), Value::String(next));
      }
      Err(error) => tracing::error!(task = %task.name, error = %error, "cron_listing_next_run_failed"),
    }
    fields_by_session.insert(session_id.clone(), fields);
  }
  *memo = Some(RowScheduleMemo {
    fingerprint,
    session_ids: ids,
    fields: fields_by_session.clone(),
    earliest_next_fire,
    #[cfg(test)]
    builds,
  });
  fields_by_session
}

/// Makes the registry available to cron bindings before crash recovery can wake a session.
pub struct BindingContextService {
  registry: watch::Sender<Option<Weak<Registry>>>,
}

impl BindingContextService {
  pub fn new(registry: watch::Sender<Option<Weak<Registry>>>) -> Self {
    Self { registry }
  }
}

impl Service for BindingContextService {
  fn name(&self) -> &'static str {
    "cron_binding_context"
  }

  fn phase(&self) -> Phase {
    Phase::Early
  }

  fn start<'a>(&'a self, ctx: ServiceContext) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move {
      self.registry.send_replace(Some(Arc::downgrade(&ctx.features.registry)));
      Ok(())
    })
  }

  fn stop<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async { Ok(()) })
  }
}

impl SequenceController for CronSequenceController {
  fn sequence_kind(&self) -> &'static str {
    "cron_steps"
  }

  fn redrive<'a>(&'a self, session_id: &'a str, tree: &'a dyn SequenceTree) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move { redrive_firing(session_id, tree).await.map(|_| ()) })
  }

  fn launch_context<'a>(
    &'a self,
    _meta: &'a SessionMetadata,
    _run: &'a RunRecord,
    _launch_text: &'a str,
  ) -> BoxFuture<'a, anyhow::Result<Option<String>>> {
    Box::pin(async { Ok(None) })
  }

  fn after_run<'a>(
    &'a self,
    session_id: &'a str,
    run: &'a RunRecord,
    tree: &'a dyn SequenceTree,
  ) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move {
      let owner_ref = run
        .sequence_ref
        .as_ref()
        .filter(|sequence| sequence.kind == "cron_steps")
        .map(|sequence| sequence.owner_ref.clone());
      let result = redrive_firing(session_id, tree).await;
      if let Some(owner_ref) = owner_ref {
        match &result {
          Ok(done) => crate::boundary::notify_boundary(session_id, &owner_ref, *done, false),
          Err(_) => crate::boundary::notify_boundary(session_id, &owner_ref, false, true),
        }
      }
      result.map(|_| ())
    })
  }

  fn recover_run<'a>(
    &'a self,
    session_id: &'a str,
    run: &'a RunRecord,
    outcome: Option<&'a str>,
    tree: &'a dyn SequenceTree,
  ) -> BoxFuture<'a, anyhow::Result<bool>> {
    Box::pin(async move {
      if outcome.is_none() && run.pid.is_some() {
        return Ok(false);
      }
      redrive_firing(session_id, tree).await?;
      Ok(true)
    })
  }

  fn reconcile_interrupted<'a>(&'a self, _tree: &'a dyn SequenceTree) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async { Ok(()) })
  }

  fn binding(&self, session_id: &str) -> Option<Arc<dyn SequenceBinding>> {
    // Binding lookups use the same loader snapshot as Python's cached task list (`src/features/cron/cron_sequence.py:94-106`).
    let snapshot = self.snapshot_for_listing();
    let names = names_for_binding(&snapshot.tasks, session_id);
    let name = names.first()?.clone();
    Some(Arc::new(CronBinding { name, names, registry: self.registry.clone() }))
  }

  fn owns_session(&self, meta: &SessionMetadata) -> bool {
    meta.extra.get("scheduled_task").is_some_and(|value| !value.is_null())
  }

  fn listing_fields(&self, session_ids: &[String], now: UtcDateTime) -> HashMap<String, Map<String, Value>> {
    let tasks = self.snapshot_for_listing();
    let mut memo = self.row_schedule_memo.lock().expect("cron row schedule memo is not poisoned");
    row_schedule_fields(&mut memo, &tasks.tasks, tasks.fingerprint, session_ids, now)
  }

  fn bound_session_ids(&self) -> Option<Vec<String>> {
    let tasks = self.snapshot_for_listing();
    Some(tasks.tasks.iter().filter_map(|task| task.session_id.clone()).collect())
  }

  fn move_bindings<'a>(&'a self, source: &'a str, new: &'a str) -> BoxFuture<'a, anyhow::Result<bool>> {
    Box::pin(async move {
      let home = cron_home();
      let snapshot = self.snapshot_for_listing();
      let names = names_for_binding(&snapshot.tasks, source);
      let moved_to = new.to_string();
      let source = source.to_string();
      let writes = names.clone();
      tokio::task::spawn_blocking(move || {
        for name in &writes {
          write_task_key(&home, name, "session_id", Value::String(moved_to.clone()))?;
          tracing::info!(task = %name, source, session = %moved_to, "scheduled_task_moved");
        }
        Ok::<(), anyhow::Error>(())
      })
      .await??;
      Ok(!names.is_empty())
    })
  }
}

fn last_saturday_1am_utc(now: chrono::DateTime<Utc>) -> anyhow::Result<UtcDateTime> {
  let timezone = chrono_tz::America::Los_Angeles;
  let now_pt = now.with_timezone(&timezone);
  let days_since_saturday = (i64::from(now_pt.weekday().num_days_from_sunday()) + 1).rem_euclid(7);
  let mut saturday_1am = now_pt
    .date_naive()
    .and_hms_opt(1, 0, 0)
    .ok_or_else(|| anyhow::anyhow!("Saturday 01:00 is not a valid local time"))?
    - chrono::Duration::days(days_since_saturday);
  // On a Saturday before 01:00 PT, this week's boundary is still ahead; the last one is a week back.
  if saturday_1am > now_pt.naive_local() {
    saturday_1am -= chrono::Duration::days(7);
  }
  let saturday_1am = timezone
    .from_local_datetime(&saturday_1am)
    .single()
    .ok_or_else(|| anyhow::anyhow!("Saturday 01:00 is ambiguous or nonexistent in America/Los_Angeles"))?;
  UtcDateTime::parse(&saturday_1am.with_timezone(&Utc).to_rfc3339()).map_err(Into::into)
}

async fn redrive_firing(leaf_id: &str, tree: &dyn SequenceTree) -> anyhow::Result<bool> {
  let records = tree.list_run_records_sync(leaf_id)?;
  let Some(sequence) =
    records.iter().filter_map(|run| run.sequence_ref.as_ref()).find(|sequence| sequence.kind == "cron_steps")
  else {
    return Ok(true);
  };
  let Some((task_name, firing)) = parse_firing_ref(&sequence.owner_ref) else {
    tracing::warn!(leaf = leaf_id, owner_ref = %sequence.owner_ref, "cron_firing_ref_unparsable");
    return Ok(true);
  };
  let Some(task) = load_bound_task(&task_name, tree.config())? else {
    tracing::warn!(leaf = leaf_id, task = task_name, "cron_firing_task_missing");
    return Ok(true);
  };
  let Some(leaf_meta) = tree.load_meta(leaf_id).await? else { return Ok(true) };
  let Some(parent_id) = leaf_meta.task_parent_id.as_deref() else { return Ok(true) };
  let Some(parent) = tree.load_meta(parent_id).await? else { return Ok(true) };
  reconcile_bound_firing(&task, &parent, tree, &firing, leaf_id).await
}

async fn reconcile_bound_firing(
  task: &ScheduledTaskConfig,
  parent: &SessionMetadata,
  tree: &dyn SequenceTree,
  firing: &str,
  leaf_id: &str,
) -> anyhow::Result<bool> {
  if tree.task_state(leaf_id)? != "open" {
    return Ok(true);
  }
  let Some(steps) = task.steps.as_ref().filter(|steps| !steps.is_empty()) else {
    return Ok(true);
  };
  let records = tree.list_run_records_sync(leaf_id)?;
  let events = tree.load_events_sync(leaf_id)?;
  let mut by_position = BTreeMap::new();
  for run in records {
    if let Some(sequence) = run
      .sequence_ref
      .as_ref()
      .filter(|sequence| sequence.kind == "cron_steps" && sequence.owner_ref == firing_ref(task, firing))
    {
      by_position.insert(sequence.position as usize, run);
    }
  }
  let executed = by_position
    .iter()
    .filter(|(_, run)| tree.run_has_terminal_fact(run, &events))
    .map(|(position, _)| *position)
    .collect::<Vec<_>>();
  let pending = by_position
    .iter()
    .filter(|(_, run)| !tree.run_has_terminal_fact(run, &events))
    .map(|(position, _)| *position)
    .collect::<Vec<_>>();
  if let Some(position) = pending.first().copied() {
    let run = &by_position[&position];
    if run.pid.is_some() {
      return Ok(false);
    }
    if let Some(conflict) = distinct_backend_conflict(task, tree.config())? {
      let summary = format!(
        "Scheduled task '{}' stopped before relaunching step '{}': {conflict}.",
        task.name, steps[position].name
      );
      return deliver_boundary_report(tree, leaf_id, parent.id.as_str(), task, firing, "failed", &summary)
        .await
        .map(|()| true);
    }
    let prompt = step_prompt(steps, position, &by_position, tree)?;
    let (backend, model) = resolved_backend(task, tree.config(), steps[position].backend.as_deref())?;
    let run = register_step_run(tree, leaf_id, task, firing, position, &backend, model.as_deref()).await?;
    schedule_settle(tree.executor(), leaf_id.to_string(), run.id, prompt);
    return Ok(false);
  }
  let Some(last_position) = executed.last().copied() else { return Ok(true) };
  let last_run = &by_position[&last_position];
  let last_outcome = tree.terminal_outcome(&events, &last_run.id);
  if last_position + 1 < steps.len() && last_outcome.as_deref() == Some("success") && last_run.exit_code == Some(0) {
    if let Some(conflict) = distinct_backend_conflict(task, tree.config())? {
      let summary = format!(
        "Scheduled task '{}' stopped before launching step '{}': {conflict}.",
        task.name,
        steps[last_position + 1].name
      );
      return deliver_boundary_report(tree, leaf_id, parent.id.as_str(), task, firing, "failed", &summary)
        .await
        .map(|()| true);
    }
    let position = last_position + 1;
    let (backend, model) = resolved_backend(task, tree.config(), steps[position].backend.as_deref())?;
    let next = register_step_run(tree, leaf_id, task, firing, position, &backend, model.as_deref()).await?;
    let prompt = step_prompt(steps, position, &by_position, tree)?;
    schedule_settle(tree.executor(), leaf_id.to_string(), next.id, prompt);
    return Ok(false);
  }
  if last_outcome.as_deref() == Some("success") && last_position + 1 == steps.len() {
    return finish_successful_chain(tree, leaf_id, parent.id.as_str(), task, firing, steps, &by_position)
      .await
      .map(|()| true);
  }
  let mut blocks = Vec::new();
  for position in &executed {
    let result = run_result_text(tree, leaf_id, &by_position[position].id)?;
    blocks.push(format!(
      "**{} result:**\n{}",
      steps[*position].name,
      if result.is_empty() { "(no result)" } else { &result }
    ));
  }
  let summary = format!(
    "Scheduled task '{}' stopped at step '{}' (outcome {}); no later step ran.\n\n{}",
    task.name,
    steps[last_position].name,
    last_outcome.as_deref().unwrap_or("None"),
    blocks.join("\n\n")
  );
  deliver_boundary_report(tree, leaf_id, parent.id.as_str(), task, firing, "failed", &summary).await.map(|()| true)
}

async fn finish_successful_chain(
  tree: &dyn SequenceTree,
  leaf_id: &str,
  parent_id: &str,
  task: &ScheduledTaskConfig,
  firing: &str,
  steps: &[StepConfig],
  by_position: &BTreeMap<usize, RunRecord>,
) -> anyhow::Result<()> {
  let mut blocks = Vec::new();
  let mut result_refs = Vec::new();
  for (position, step) in steps.iter().enumerate() {
    let Some(run) = by_position.get(&position) else { continue };
    let result = run_result_text(tree, leaf_id, &run.id)?;
    let backend = run.backend.as_deref().unwrap_or("None");
    blocks.push(format!(
      "**{} result ({}):**\n{}",
      step.name,
      backend,
      if result.is_empty() { "(no result)" } else { &result }
    ));
    result_refs.push(format!("run:{}", run.id));
  }
  let summary =
    format!("Scheduled task '{}' completed all {} step(s).\n\n{}", task.name, result_refs.len(), blocks.join("\n\n"));
  let last_run =
    by_position.values().next_back().ok_or_else(|| anyhow::anyhow!("cron step sequence has no final run"))?;
  let request_id = format!("auto:{}", firing_ref(task, firing));
  match tree
    .completion()
    .evaluate_automatic_completion(leaf_id, &last_run.id, Some(&summary), Some(&result_refs), Some(&request_id))
    .await
  {
    Ok(_) => Ok(()),
    Err(error) => {
      let blocked = format!(
        "Scheduled task '{}' completed all {} step(s) but the leaf's automatic close is blocked ({}); the task stays open with its evidence.\n\n{}",
        task.name,
        result_refs.len(),
        error,
        blocks.join("\n\n")
      );
      deliver_boundary_report(tree, leaf_id, parent_id, task, firing, "blocked", &blocked).await
    }
  }
}

async fn deliver_boundary_report(
  tree: &dyn SequenceTree,
  leaf_id: &str,
  recipient: &str,
  task: &ScheduledTaskConfig,
  firing: &str,
  outcome: &str,
  summary: &str,
) -> anyhow::Result<()> {
  let source = tree.dispatch().report_source_event(leaf_id, "scheduled firing leaf")?;
  let epoch = tree.session_events().prime_aggregator(recipient).await?;
  let (report, created) = {
    let guard = tree.control_lock().await;
    tree
      .dispatch()
      .deliver_child_report_locked(
        &guard,
        leaf_id,
        source,
        outcome,
        summary,
        Some(&[firing_ref(task, firing)]),
        Some(recipient),
      )
      .await?
  };
  if created {
    tree.session_events().announce_appended_event(recipient, &report, epoch).await?;
    tree.dispatch().wake_parent(recipient, report).await?;
  }
  if outcome == "failed"
    && let Some(run_id) = tree.last_ended_run_id(leaf_id)?
  {
    tree.completion().archive_unsuccessful_worker(leaf_id, &run_id, "failed").await?;
  }
  tracing::info!(task = %task.name, leaf = leaf_id, firing, outcome, created, "cron_sequence_report_delivered");
  Ok(())
}

fn schedule_settle(
  executor: Option<Arc<dyn charliebot_registry::sequence::SequenceExecutor>>,
  leaf_id: String,
  run_id: String,
  prompt: String,
) {
  let Some(executor) = executor else {
    tracing::error!(leaf = %leaf_id, run = %run_id, "cron_sequence_executor_missing");
    return;
  };
  tokio::spawn(async move {
    if let Err(error) = executor.launch_and_settle(&leaf_id, &run_id, Some(&prompt)).await {
      tracing::error!(leaf = %leaf_id, run = %run_id, error = %error, "cron_recovered_step_failed");
    }
  });
}

async fn register_step_run(
  tree: &dyn SequenceTree,
  leaf_id: &str,
  task: &ScheduledTaskConfig,
  firing: &str,
  position: usize,
  backend: &str,
  model: Option<&str>,
) -> StoreResult<RunRecord> {
  let request_id = format!("{}:step:{position}", firing_ref(task, firing));
  let run_id = charliebot_store::control_events::stable_run_id(leaf_id, &request_id);
  if let Some(run) = tree.get_run(leaf_id, &run_id).await? {
    return Ok(run);
  }
  let task_spec_text = task.steps.as_ref().expect("a scheduled-step run has step configuration")[position]
    .prompt
    .as_deref()
    .expect("validated scheduled steps have prompt text");
  let run = RunRecord {
    id: run_id,
    session_id: leaf_id.to_string(),
    kind: RunKind::ScheduledStep,
    input_event_ids: Vec::new(),
    task_spec_hash: None,
    task_spec_ref: None,
    prompt_snapshot_ref: None,
    retry_of_run_id: None,
    review_of_run_id: None,
    backend: Some(backend.to_string()),
    model: model.map(str::to_string),
    native_session_id: None,
    pid: None,
    pid_start: None,
    started_at: None,
    ended_at: None,
    exit_code: None,
    repo_path: task.repo.clone(),
    base_branch: None,
    branch_name: None,
    worktree_path: None,
    sequence_ref: Some(SequenceRef {
      kind: "cron_steps".to_string(),
      owner_ref: firing_ref(task, firing),
      position: position as i64,
    }),
    raw_log_ref: None,
    events_ref: None,
    result_ref: None,
  };
  let guard = tree.control_lock().await;
  tree.register_run_locked(&guard, leaf_id, run, Some(task_spec_text)).await
}

fn resolved_backend(
  task: &ScheduledTaskConfig,
  cfg: &CharlieBotConfig,
  backend: Option<&str>,
) -> anyhow::Result<(String, Option<String>)> {
  let backend = backend
    .filter(|backend| !backend.is_empty())
    .or(task.backend.as_deref().filter(|backend| !backend.is_empty()))
    .or_else(|| cfg.backends.options.first().map(|option| option.id.as_str()))
    .ok_or_else(|| anyhow::anyhow!("scheduled task backend resolution requires a configured backends.options entry"))?;
  let option = cfg
    .get_backend_option(backend)
    .ok_or_else(|| anyhow::anyhow!("scheduled task '{}' backend '{}' is not configured", task.name, backend))?;
  Ok((option.id.clone(), (!option.model.is_empty()).then(|| option.model.clone())))
}

fn distinct_backend_conflict(task: &ScheduledTaskConfig, cfg: &CharlieBotConfig) -> anyhow::Result<Option<String>> {
  let Some(steps) = task.steps.as_ref() else { return Ok(None) };
  // Match Python's one-pass name index (`src/features/cron/cron_sequence.py:532-559`).
  let positions = steps.iter().enumerate().map(|(index, step)| (step.name.as_str(), index)).collect::<HashMap<_, _>>();
  for (position, step) in steps.iter().enumerate() {
    let Some(prior_name) = step.distinct_backend_from.as_deref() else { continue };
    let prior_position = positions
      .get(prior_name)
      .copied()
      .ok_or_else(|| anyhow::anyhow!("step '{}' names missing distinct backend source '{prior_name}'", step.name))?;
    let prior = &steps[prior_position];
    let (backend, model) = resolved_backend(task, cfg, step.backend.as_deref())?;
    let (prior_backend, prior_model) = resolved_backend(task, cfg, prior.backend.as_deref())?;
    let own = cfg.get_backend_option(&backend).expect("resolved backend option exists");
    let old = cfg.get_backend_option(&prior_backend).expect("resolved backend option exists");
    if own.backend_type == old.backend_type && model == prior_model {
      let names = if backend == prior_backend {
        format!("backend '{backend}'")
      } else {
        format!("backends '{prior_backend}' and '{backend}'")
      };
      return Ok(Some(format!(
        "steps '{}' and '{}' declare distinct_backend_from but both resolve to {names} (type {}, model {})",
        prior.name,
        step.name,
        own.backend_type,
        model.as_deref().unwrap_or("None")
      )));
    }
    let _ = position;
  }
  Ok(None)
}

fn step_prompt(
  steps: &[StepConfig],
  position: usize,
  by_position: &BTreeMap<usize, RunRecord>,
  tree: &dyn SequenceTree,
) -> anyhow::Result<String> {
  let prompt = steps[position].prompt.as_deref().unwrap_or_default().to_string();
  if position == 0 {
    return Ok(prompt);
  }
  let previous = by_position.get(&(position - 1));
  let result = previous.map(|run| run_result_text(tree, &run.session_id, &run.id)).transpose()?.unwrap_or_default();
  Ok(format!("{}\n\n## Result of the previous step ({})\n{}", prompt.trim_end(), steps[position - 1].name, result))
}

fn run_result_text(tree: &dyn SequenceTree, session_id: &str, run_id: &str) -> anyhow::Result<String> {
  let path = tree.run_dir(session_id, run_id).join(run_records::RUN_EVENTS_NAME);
  Ok(charliebot_tasks::review::read_worker_summary_file(&path)?.unwrap_or_default())
}

fn parse_firing_ref(owner_ref: &str) -> Option<(String, String)> {
  let rest = owner_ref.strip_prefix("cron:")?;
  let (name, firing) = rest.split_once(':')?;
  if name.is_empty() || firing.is_empty() {
    return None;
  }
  Some((name.to_string(), firing.to_string()))
}

fn firing_ref(task: &ScheduledTaskConfig, firing: &str) -> String {
  format!("cron:{}:{firing}", task.name)
}

fn load_bound_task(name: &str, cfg: &CharlieBotConfig) -> anyhow::Result<Option<ScheduledTaskConfig>> {
  let path = loader::cron_path(cfg, name);
  if !path.is_file() {
    return Ok(None);
  }
  let repo =
    std::env::var_os(REPO_ROOT_ENV).map(PathBuf::from).ok_or_else(|| anyhow::anyhow!("{REPO_ROOT_ENV} is not set"))?;
  loader::load_file(&path, name, &repo).map(Some).map_err(anyhow::Error::msg)
}

fn cron_home() -> PathBuf {
  home::charliebot_home_dir().expect("the server validated the profile home before registering cron")
}

fn names_for_binding(tasks: &[ScheduledTaskConfig], session_id: &str) -> Vec<String> {
  tasks.iter().filter(|task| task.session_id.as_deref() == Some(session_id)).map(|task| task.name.clone()).collect()
}

fn bound_configs_for_listing(home: &Path) -> Vec<ScheduledTaskConfig> {
  let mut paths = match std::fs::read_dir(loader::cron_dir_for_home(home)) {
    Ok(entries) => entries.filter_map(Result::ok).map(|entry| entry.path()).collect::<Vec<_>>(),
    Err(_) => return Vec::new(),
  };
  paths.sort();
  paths
    .into_iter()
    .filter(|path| path.extension().and_then(|extension| extension.to_str()) == Some("yaml"))
    .filter_map(|path| {
      let name = path.file_stem()?.to_str()?.to_string();
      let mut body = loader::read_task_file(&path).ok()?;
      body.insert("name".to_string(), Value::String(name));
      serde_json::from_value(Value::Object(body)).ok()
    })
    .collect()
}

fn write_task_key(home: &Path, name: &str, key: &str, value: Value) -> anyhow::Result<()> {
  let path = loader::cron_path_for_home(home, name);
  let mut data = loader::read_task_file(&path).map_err(|error| anyhow::anyhow!(error.message_without_path()))?;
  data.insert(key.to_string(), value);
  loader::write_task_file(&path, &Value::Object(data))?;
  Ok(())
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use charliebot_core::event_types;
  use charliebot_registry::sequence::SequenceBinding;
  use charliebot_store::metadata::{TaskProfile, TaskSpec, UtcDateTime};
  use charliebot_store::{CreateTask, TaskCaller};
  use serde_json::Map;
  use serde_json::{Value, json};
  use tokio::sync::watch;

  use super::{CronBinding, last_saturday_1am_utc, register_step_run, row_schedule_fields};
  use crate::config::{ScheduledTaskConfig, StepConfig};
  use crate::test_support::{feature_context, manager};

  #[tokio::test]
  async fn boundary_receiver_wakes_for_progress_and_terminal_notice() {
    let leaf_id = "synthetic-boundary-leaf";
    let owner_ref = "cron:synthetic-task:synthetic-firing";
    let mut receiver = crate::boundary::subscribe_boundary(leaf_id, owner_ref);

    crate::boundary::notify_boundary(leaf_id, owner_ref, false, false);
    receiver.changed().await.unwrap();
    assert_eq!(receiver.borrow_and_update().generation, 1);
    assert!(!receiver.borrow().done);

    crate::boundary::notify_boundary(leaf_id, owner_ref, true, false);
    receiver.changed().await.unwrap();
    assert_eq!(receiver.borrow_and_update().generation, 2);
    assert!(receiver.borrow().done);
    crate::boundary::remove_boundary(leaf_id, owner_ref);
  }

  #[test]
  fn sidebar_schedule_rows_reuse_the_id_and_fingerprint_memo_until_next_fire() {
    let session_ids = (0..4_317).map(|index| format!("synthetic-session-{index:04}")).collect::<Vec<_>>();
    let tasks = (0..13)
      .map(|index| {
        serde_json::from_value::<ScheduledTaskConfig>(json!({
          "name": format!("synthetic-task-{index:02}"),
          "cron": "0 8 * * *",
          "timezone": "UTC",
          "session_id": session_ids[index],
        }))
        .unwrap()
      })
      .collect::<Vec<_>>();
    let now = UtcDateTime::parse("2026-01-01T00:00:00Z").unwrap();
    let mut memo = None;

    let first = row_schedule_fields(&mut memo, &tasks, 17, &session_ids, now);
    // Only a bound session owns fields; an unbound one renders its default without an entry.
    assert_eq!(first.len(), tasks.len());
    assert_eq!(first[&session_ids[0]]["schedule_task"], json!("synthetic-task-00"));
    assert!(!first.contains_key(&session_ids[tasks.len()]));
    let warm = row_schedule_fields(&mut memo, &tasks, 17, &session_ids, now);
    assert_eq!(warm, first);
    assert_eq!(memo.as_ref().unwrap().builds, 1);

    let at_fire = UtcDateTime::parse("2026-01-01T08:00:00Z").unwrap();
    row_schedule_fields(&mut memo, &tasks, 17, &session_ids, at_fire);
    assert_eq!(memo.as_ref().unwrap().builds, 2);
    row_schedule_fields(&mut memo, &tasks, 18, &session_ids, at_fire);
    assert_eq!(memo.as_ref().unwrap().builds, 3);
  }

  #[test]
  fn the_last_saturday_boundary_is_always_in_the_past() {
    // Saturday 00:14 PT: this week's 01:00 boundary is still ahead, so the last one is a week back.
    let before_one = chrono::DateTime::parse_from_rfc3339("2026-10-31T07:14:00Z").expect("the stamp parses").to_utc();
    assert_eq!(
      last_saturday_1am_utc(before_one).expect("the boundary builds").into_inner().unix_timestamp(),
      chrono::DateTime::parse_from_rfc3339("2026-10-24T08:00:00Z").expect("the stamp parses").timestamp()
    );
    // Saturday 02:14 PT: this week's boundary has passed and is the last one.
    let after_one = chrono::DateTime::parse_from_rfc3339("2026-10-31T09:14:00Z").expect("the stamp parses").to_utc();
    assert_eq!(
      last_saturday_1am_utc(after_one).expect("the boundary builds").into_inner().unix_timestamp(),
      chrono::DateTime::parse_from_rfc3339("2026-10-31T08:00:00Z").expect("the stamp parses").timestamp()
    );
  }

  fn binding(features: &Arc<charliebot_registry::context::FeatureContext>, names: &[&str]) -> CronBinding {
    features.registry.bind_feature_context(features).unwrap();
    let (sender, receiver) = watch::channel(None);
    sender.send_replace(Some(Arc::downgrade(&features.registry)));
    CronBinding {
      name: names[0].to_string(),
      names: names.iter().map(|name| (*name).to_string()).collect(),
      registry: receiver,
    }
  }

  fn write_cron(home: &std::path::Path, name: &str, value: Value) {
    let path = home.join("config.d/cron.d").join(format!("{name}.yaml"));
    std::fs::write(&path, charliebot_core::yaml::dump(&value).unwrap()).unwrap();
  }

  #[tokio::test]
  async fn stale_anchor_wake_recycles_chat_events_and_keeps_worker_threads() {
    // Python: tests/test_cron_auto_bind.py::test_bound_node_wake_preserves_old_worker_threads_and_prefixes_the_firing_report
    let (home, features) = feature_context();
    let node = manager(&features, "synthetic-stale-anchor-node").await.unwrap();
    features
      .store
      .persist_cc_session_id_at(
        &node.id,
        "synthetic-native-session",
        Some("synthetic"),
        UtcDateTime::parse("2020-01-01T00:00:00Z").unwrap(),
      )
      .await
      .unwrap();
    let events_path = features.store.chat_events_path(&node.id);
    std::fs::create_dir_all(events_path.parent().unwrap()).unwrap();
    let old_event = json!({"id":"synthetic-old-event","timestamp":"2020-01-01T00:00:00+00:00"});
    std::fs::write(&events_path, format!("{}\n", serde_json::to_string(&old_event).unwrap())).unwrap();
    let thread_file = features.store.session_dir(&node.id).join("threads/legacy-round/sentinel.txt");
    std::fs::create_dir_all(thread_file.parent().unwrap()).unwrap();
    std::fs::write(&thread_file, "retained").unwrap();
    let meta = features.store.get_session(&node.id).await.unwrap().unwrap();

    let prefix = binding(&features, &["synthetic-cron"])
      .on_wake(&meta, &[json!({"type": event_types::CHILD_REPORT})])
      .await
      .unwrap()
      .unwrap();

    assert_eq!(
      prefix,
      "[Auto-triggered scheduled task result for 'synthetic-cron']\nReview the worker/reviewer results below. Check: was the branch merged? Are there errors? Summarize the outcome.\n\n"
    );
    let fresh = features.store.get_session(&node.id).await.unwrap().unwrap();
    assert_eq!(fresh.cc_session_id, None);
    assert_eq!(fresh.cc_session_started_at, None);
    assert_eq!(fresh.archive_offset, 1);
    assert!(std::fs::read_to_string(&events_path).unwrap().is_empty());
    assert_eq!(std::fs::read_to_string(thread_file).unwrap(), "retained");
    let archive_dir = home.path().join("sessions").join(&node.id).join("data/archives");
    let archive = std::fs::read_dir(archive_dir).unwrap().next().unwrap().unwrap().path();
    assert_eq!(std::fs::read_to_string(archive).unwrap(), format!("{}\n", serde_json::to_string(&old_event).unwrap()));
  }

  #[tokio::test]
  async fn live_anchor_wake_keeps_its_conversation_and_has_no_prefix() {
    // Python: tests/test_cron_auto_bind.py::test_bound_node_wake_on_a_live_anchor_carries_no_prefix
    let (_home, features) = feature_context();
    let node = manager(&features, "synthetic-live-anchor-node").await.unwrap();
    features.store.persist_cc_session_id(&node.id, "synthetic-live-native", Some("synthetic")).await.unwrap();
    let events_path = features.store.chat_events_path(&node.id);
    std::fs::create_dir_all(events_path.parent().unwrap()).unwrap();
    let old_event = json!({"id":"synthetic-unrecycled-event","timestamp":"2020-01-01T00:00:00+00:00"});
    std::fs::write(&events_path, format!("{}\n", serde_json::to_string(&old_event).unwrap())).unwrap();
    let meta = features.store.get_session(&node.id).await.unwrap().unwrap();

    let prefix = binding(&features, &["synthetic-cron"])
      .on_wake(&meta, &[json!({"type": event_types::CHILD_REPORT})])
      .await
      .unwrap();

    assert_eq!(prefix, None);
    let fresh = features.store.get_session(&node.id).await.unwrap().unwrap();
    assert_eq!(fresh.cc_session_id.as_deref(), Some("synthetic-live-native"));
    assert_eq!(fresh.archive_offset, 0);
    assert_eq!(
      std::fs::read_to_string(events_path).unwrap(),
      format!("{}\n", serde_json::to_string(&old_event).unwrap())
    );
  }

  #[tokio::test]
  async fn recovered_step_runs_pin_each_configured_prompt_before_chaining() {
    let (_home, features) = feature_context();
    let parent = manager(&features, "synthetic-step-spec-parent").await.unwrap();
    let leaf = features
      .store
      .create_task(
        CreateTask {
          request_id: "synthetic-step-spec-leaf".to_string(),
          task_parent_id: Some(parent.id.clone()),
          profile: TaskProfile::Worker,
          task: Some(TaskSpec { goal: "synthetic scheduled steps".to_string(), ..TaskSpec::default() }),
          name: Some("Synthetic scheduled steps".to_string()),
          backend: Some("synthetic".to_string()),
          group: None,
          session_id: None,
        },
        TaskCaller::System,
        Map::new(),
      )
      .await
      .unwrap();
    let task = ScheduledTaskConfig {
      name: "synthetic-step-task".to_string(),
      cron: "0 3 * * *".to_string(),
      prompt_file: None,
      repo: None,
      backend: Some("synthetic".to_string()),
      timezone: "America/Los_Angeles".to_string(),
      enabled: true,
      project: None,
      allow_failure: false,
      session_id: Some(parent.id.clone()),
      mode: Some("worker".to_string()),
      prompt: None,
      handler: None,
      steps: Some(vec![
        StepConfig {
          name: "select".to_string(),
          prompt_file: None,
          prompt: Some("Select candidates.".to_string()),
          backend: None,
          distinct_backend_from: None,
        },
        StepConfig {
          name: "review".to_string(),
          prompt_file: None,
          prompt: Some("Review the diff.".to_string()),
          backend: None,
          distinct_backend_from: None,
        },
      ]),
    };
    charliebot_tasks::execution::pre_recovery_step(Arc::clone(&features)).await.unwrap();
    let adapter = features.startup.require::<charliebot_tasks::execution::TaskExecutionAdapter>().unwrap();
    let tree = charliebot_tasks::recovery::sequence_tree(&features, &adapter).unwrap();

    for (position, prompt) in ["Select candidates.", "Review the diff."].into_iter().enumerate() {
      let run = register_step_run(
        &tree,
        &leaf.id,
        &task,
        "2026-10-21T03:00:00-07:00",
        position,
        "synthetic",
        Some("synthetic-model"),
      )
      .await
      .unwrap();
      let task_spec_path = std::path::Path::new(run.task_spec_ref.as_deref().unwrap());
      assert_eq!(task_spec_path.file_name().and_then(|name| name.to_str()), Some("task_spec.md"));
      assert_eq!(std::fs::read_to_string(task_spec_path).unwrap(), prompt);
      let expected_hash = charliebot_store::control_events::sha256_hex(prompt);
      assert_eq!(run.task_spec_hash.as_deref(), Some(expected_hash.as_str()));
    }
  }

  #[tokio::test]
  async fn failed_history_recycle_keeps_the_cleared_anchor_and_wake_prefix() {
    let (_home, features) = feature_context();
    let node = manager(&features, "synthetic-recycle-failure-node").await.unwrap();
    features
      .store
      .persist_cc_session_id_at(
        &node.id,
        "synthetic-old-native",
        Some("synthetic"),
        UtcDateTime::parse("2020-01-01T00:00:00Z").unwrap(),
      )
      .await
      .unwrap();
    let events_path = features.store.chat_events_path(&node.id);
    std::fs::remove_file(&events_path).unwrap();
    std::fs::create_dir_all(&events_path).unwrap();
    let meta = features.store.get_session(&node.id).await.unwrap().unwrap();

    let prefix = binding(&features, &["synthetic-cron"])
      .on_wake(&meta, &[json!({"type": event_types::CHILD_REPORT})])
      .await
      .unwrap();

    assert!(prefix.is_some());
    let fresh = features.store.get_session(&node.id).await.unwrap().unwrap();
    assert_eq!(fresh.cc_session_id, None);
    assert_eq!(fresh.cc_session_started_at, None);
  }

  #[tokio::test]
  async fn archiving_a_bound_node_disables_every_task_and_preserves_other_yaml_fields() {
    // Python: tests/test_cron_auto_bind.py::test_archiving_bound_node_writes_enabled_false
    // Python: tests/test_archive_cron_restore.py::test_archive_disables_every_task_bound_to_the_node
    let (home, features) = feature_context();
    let node = manager(&features, "synthetic-archive-bound-node").await.unwrap();
    for name in ["synthetic-nightly", "synthetic-weekly"] {
      write_cron(
        home.path(),
        name,
        json!({
          "cron": "0 8 * * *",
          "handler": "synthetic_handler",
          "session_id": node.id,
          "enabled": true,
          "project": "synthetic-project",
        }),
      );
    }

    binding(&features, &["synthetic-nightly", "synthetic-weekly"]).on_archive().await.unwrap();

    for name in ["synthetic-nightly", "synthetic-weekly"] {
      let path = home.path().join("config.d/cron.d").join(format!("{name}.yaml"));
      let task: serde_yaml::Value = serde_yaml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
      assert_eq!(task["enabled"].as_bool(), Some(false));
      assert_eq!(task["session_id"].as_str(), Some(node.id.as_str()));
      assert_eq!(task["project"].as_str(), Some("synthetic-project"));
    }
  }
}
