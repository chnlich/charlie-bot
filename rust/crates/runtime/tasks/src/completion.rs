//! Task close, cancel, reopen, acknowledgement, and completion evidence.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use charliebot_core::event_types;
use charliebot_registry::caller::CallerIdentity;
use charliebot_registry::context::{FeatureContext, StartupStop, StateError};
use charliebot_registry::sequence::{RestoreOutcome, SequenceCompletion};
use charliebot_store::ChatEvent;
use charliebot_store::compact::CompactValue;
use charliebot_store::control_events::{self, ACTOR_AGENT, ACTOR_SYSTEM, ACTOR_USER};
use charliebot_store::metadata::{SessionMetadata, TaskProfile, TaskType};
use charliebot_store::run_records::{self, RunDisplayState, RunKind, RunOutcomeKind, RunRecord};
use charliebot_store::session_fork::ReplacedSourceArchive;
use charliebot_store::{ControlGuard, StoreError, StoreHookFuture, StoreResult};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::dispatch::{self, Dispatcher, RunFinishedCompletion};
use crate::git;
use crate::review_text;

/// Prefix for a run reference in a child report's result_refs.
pub const RUN_REF_PREFIX: &str = "run:";
const SPEC_REF_PREFIX: &str = "spec:";
const REVIEW_REF_PREFIX: &str = "review:";
const LANDING_REF_PREFIX: &str = "landed:";
type DeliveryRunOutcomes = (HashMap<String, RunRecord>, HashMap<String, Option<String>>);
const RESTORE_CHAIN_HOP_LIMIT: usize = 1000;
/// The operator-scope refusal an archive request without user credentials gets.
pub const ARCHIVE_OPERATOR_REFUSAL: &str = "archiving a task requires operator credentials";

struct CloseNowRequest<'a> {
  session_id: &'a str,
  request_id: &'a str,
  evidence: &'a CompletionEvidence,
  actor: &'a str,
  outcome: &'a str,
  exclude_run_ids: &'a HashSet<String>,
  exclude_input_ids: &'a HashSet<String>,
}

/// One close fact to append plus the parent delivery its caller owns.
struct CloseAppend<'a> {
  session_id: &'a str,
  request_id: &'a str,
  outcome: &'a str,
  summary: &'a str,
  result_refs: &'a [String],
  run_ids: &'a [String],
  actor: &'a str,
  report_to: Option<&'a str>,
  archived_with: Option<&'a str>,
  sequence_outcome: Option<&'a str>,
}

/// The target branch and commit offered as landing evidence.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LandingEvidence {
  pub branch: String,
  pub commit: String,
  #[serde(default)]
  pub repo_path: Option<String>,
}

/// Evidence supplied for an operator, automatic, or sequence completion.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionEvidence {
  #[serde(default)]
  pub summary: String,
  #[serde(default)]
  pub result_refs: Vec<String>,
  #[serde(default)]
  pub run_ids: Vec<String>,
  #[serde(default)]
  pub review_run_ids: Vec<String>,
  #[serde(default)]
  pub landing: Option<LandingEvidence>,
}

/// The sequence outcome written onto its worker child's close fact and report.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SequenceCloseOutcome {
  Completed,
  Blocked,
  Failed,
  Cancelled,
}

impl SequenceCloseOutcome {
  fn as_str(self) -> &'static str {
    match self {
      Self::Completed => "completed",
      Self::Blocked => "blocked",
      Self::Failed => "failed",
      Self::Cancelled => "cancelled",
    }
  }
}

/// The owner of close, cancel, reopen, acknowledgement, and close evidence for one server.
pub struct TaskCompletion {
  features: Arc<FeatureContext>,
}

impl TaskCompletion {
  /// Build the completion owner over the server's shared feature context.
  pub fn new(features: Arc<FeatureContext>) -> Self {
    Self { features }
  }

  fn store(&self) -> &charliebot_store::Store {
    &self.features.store
  }

  fn dispatcher(&self) -> StoreResult<Arc<Dispatcher>> {
    dispatch::dispatcher(&self.features).map_err(|error| StoreError::new(error.to_string()))
  }

  fn execution_blockers(&self, session_id: &str, exclude_run_ids: &HashSet<String>) -> StoreResult<Vec<String>> {
    let mut blockers = self
      .store()
      .run_blockers(session_id)?
      .into_iter()
      .filter(|blocker| !exclude_run_ids.contains(&blocker.run_id))
      .map(|blocker| blocker.reason)
      .collect::<Vec<_>>();
    for descendant in self.store().descendant_ids(session_id)? {
      if self.store().task_state(&descendant)? == "open" {
        blockers.push(format!("has open descendant task {descendant}"));
      }
    }
    Ok(blockers)
  }

  fn completion_blockers(
    &self,
    session_id: &str,
    exclude_run_ids: &HashSet<String>,
    exclude_input_ids: &HashSet<String>,
  ) -> StoreResult<Vec<String>> {
    let mut blockers = self.execution_blockers(session_id, exclude_run_ids)?;
    let pending = self
      .dispatcher()?
      .pending_inputs(session_id)?
      .into_iter()
      .filter(|event| event.get("id").and_then(CompactValue::as_str).is_none_or(|id| !exclude_input_ids.contains(id)))
      .collect::<Vec<_>>();
    if !pending.is_empty() {
      blockers.insert(0, dispatch::unprocessed_input_blocker(&pending));
    }
    Ok(blockers)
  }

  fn completion_blockers_until(
    &self,
    session_id: &str,
    exclude_run_ids: &HashSet<String>,
    exclude_input_ids: &HashSet<String>,
    should_stop: &mut impl FnMut() -> bool,
  ) -> StoreResult<Option<Vec<String>>> {
    if should_stop() {
      return Ok(None);
    }
    let mut blockers = self
      .store()
      .run_blockers(session_id)?
      .into_iter()
      .filter(|blocker| !exclude_run_ids.contains(&blocker.run_id))
      .map(|blocker| blocker.reason)
      .collect::<Vec<_>>();
    if should_stop() {
      return Ok(None);
    }
    let Some(descendants) = self.store().descendant_ids_until(session_id, should_stop)? else {
      return Ok(None);
    };
    for descendant in descendants {
      if should_stop() {
        return Ok(None);
      }
      if self.store().task_state(&descendant)? == "open" {
        blockers.push(format!("has open descendant task {descendant}"));
      }
    }
    if should_stop() {
      return Ok(None);
    }
    let pending = self.dispatcher()?.pending_inputs(session_id)?;
    if should_stop() {
      return Ok(None);
    }
    let pending = pending
      .into_iter()
      .filter(|event| event.get("id").and_then(CompactValue::as_str).is_none_or(|id| !exclude_input_ids.contains(id)))
      .collect::<Vec<_>>();
    if !pending.is_empty() {
      blockers.insert(0, dispatch::unprocessed_input_blocker(&pending));
    }
    Ok(Some(blockers))
  }

  fn cancellation_blockers(&self, session_id: &str) -> StoreResult<Vec<String>> {
    self.execution_blockers(session_id, &HashSet::new())
  }

  fn delivery_run_outcomes(&self, meta: &SessionMetadata) -> StoreResult<DeliveryRunOutcomes> {
    let mut records = HashMap::new();
    let mut outcomes = HashMap::new();
    let facts = self.store().node_facts(&meta.id)?;
    let task_facts = facts.task()?;
    for run in self.store().list_run_records_sync(&meta.id)? {
      outcomes.insert(run.id.clone(), task_facts.run_outcomes.get(&run.id).cloned());
      records.insert(run.id.clone(), run);
    }
    for child_id in self.store().child_ids(&meta.id)? {
      let facts = self.store().node_facts(&child_id)?;
      let facts = facts.task()?;
      for run in self.store().list_run_records_sync(&child_id)? {
        outcomes.insert(run.id.clone(), facts.run_outcomes.get(&run.id).cloned());
        records.insert(run.id.clone(), run);
      }
    }
    Ok((records, outcomes))
  }

  fn evidence_blockers(&self, meta: &SessionMetadata, evidence: &CompletionEvidence) -> StoreResult<Vec<String>> {
    self.evidence_blockers_with_success(meta, evidence, None)
  }

  /// Validate one completion claim against this task's own facts. `succeeding_run_ids` names Runs judged as
  /// successful: an own-run archive request's still-active caller Run, whose request is re-evaluated only after it
  /// succeeds.
  fn evidence_blockers_with_success(
    &self,
    meta: &SessionMetadata,
    evidence: &CompletionEvidence,
    succeeding_run_ids: Option<&HashSet<String>>,
  ) -> StoreResult<Vec<String>> {
    let mut blockers = Vec::new();
    if evidence.summary.trim().is_empty() {
      blockers.push("completion requires a non-empty summary".to_string());
    }
    if evidence.result_refs.is_empty() {
      blockers.push("completion requires result evidence (result_refs)".to_string());
    }
    let (records, mut outcomes) = self.delivery_run_outcomes(meta)?;
    for run_id in succeeding_run_ids.into_iter().flatten() {
      outcomes.insert(run_id.clone(), Some("success".to_string()));
    }
    let some_run_judged_success = succeeding_run_ids.is_some_and(|run_ids| !run_ids.is_empty());
    let facts = self.store().node_facts(&meta.id)?;
    let facts = facts.task()?;
    let mut claimed = HashSet::new();
    for run_id in &evidence.run_ids {
      let Some(run) = records.get(run_id) else {
        blockers.push(format!("run {run_id} is not a Run of task {} or its children", meta.id));
        continue;
      };
      claimed.insert(run_id.as_str());
      if outcomes.get(run_id).and_then(Option::as_deref) != Some("success") {
        let bare_exit = if run.exit_code == Some(0) { " (a bare exit code is not evidence)" } else { "" };
        blockers.push(format!("run {run_id} has no successful run_finished fact{bare_exit}"));
      }
    }
    if !evidence.run_ids.is_empty() && claimed.is_empty() {
      blockers.push("completion evidence names no delivery Run of this task".to_string());
    }
    if evidence.run_ids.is_empty() && !some_run_judged_success {
      if outcomes.values().any(|outcome| outcome.as_deref() == Some("success")) {
        blockers.push("completion requires delivery run evidence (run_ids)".to_string());
      } else if outcomes.values().any(|outcome| outcome.as_deref() == Some("failed")) {
        blockers.push(
          "completion requires delivery run evidence (run_ids); failed Runs are not completion evidence".to_string(),
        );
      }
    }
    for result_ref in &evidence.result_refs {
      blockers.extend(self.structured_ref_blockers(meta, result_ref, &records, facts, evidence));
    }

    let claimed_work_ids = evidence.run_ids.iter().map(String::as_str).collect::<HashSet<_>>();
    for review_id in &evidence.review_run_ids {
      let Some(review) = records.get(review_id).filter(|run| run.kind == RunKind::Review && run.session_id == meta.id)
      else {
        blockers.push(format!("run {review_id} is not a review Run of task {}", meta.id));
        continue;
      };
      if facts.run_outcomes.get(review_id).map(String::as_str) != Some("success") {
        blockers.push(format!("review run {review_id} has no successful run_finished fact"));
        continue;
      }
      if let Some(review_of) = review.review_of_run_id.as_deref() {
        if !claimed_work_ids.contains(review_of) {
          blockers.push(format!(
            "review run {review_id} reviews work run {review_of}, which this completion claim does not name"
          ));
        }
      } else if !claimed_work_ids.is_empty() {
        blockers.push(format!(
          "review run {review_id} is not chained to a work Run (review_of_run_id is unset; it cannot prove which work it reviewed)"
        ));
      }
    }
    if meta.task.as_ref().is_some_and(|task| task.task_type == Some(TaskType::Implement)) {
      let successful_reviews = evidence.review_run_ids.iter().any(|review_id| {
        records.get(review_id).is_some_and(|run| {
          run.kind == RunKind::Review
            && run.session_id == meta.id
            && facts.run_outcomes.get(review_id).map(String::as_str) == Some("success")
            && run.review_of_run_id.as_deref().is_none_or(|run_id| evidence.run_ids.contains(&run_id.to_string()))
        })
      });
      let has_review_ref = evidence.result_refs.iter().any(|result_ref| result_ref.starts_with(REVIEW_REF_PREFIX));
      if !successful_reviews && !has_review_ref {
        blockers.push("implement completion requires a successful review run of this task".to_string());
      }
      if meta.task.as_ref().is_some_and(|task| task.repo_path.is_some())
        && evidence.landing.is_none()
        && !evidence.result_refs.iter().any(|result_ref| result_ref.starts_with(LANDING_REF_PREFIX))
      {
        blockers.push("implement completion requires target-branch landing evidence".to_string());
      }
    }
    Ok(blockers)
  }

  fn structured_ref_blockers(
    &self,
    meta: &SessionMetadata,
    result_ref: &str,
    records: &HashMap<String, RunRecord>,
    facts: &charliebot_store::TaskFacts,
    evidence: &CompletionEvidence,
  ) -> Vec<String> {
    let mut blockers = Vec::new();
    if let Some(run_id) = result_ref.strip_prefix(RUN_REF_PREFIX) {
      if !records.contains_key(run_id) {
        blockers.push(format!("result ref {result_ref} does not name a Run of task {}", meta.id));
      }
    } else if let Some(spec_hash) = result_ref.strip_prefix(SPEC_REF_PREFIX) {
      let claimed_ids = if evidence.run_ids.is_empty() {
        records.keys().map(String::as_str).collect::<Vec<_>>()
      } else {
        evidence.run_ids.iter().map(String::as_str).collect::<Vec<_>>()
      };
      let pinned = claimed_ids
        .iter()
        .filter_map(|run_id| records.get(*run_id).and_then(|run| run.task_spec_hash.as_deref()))
        .collect::<HashSet<_>>();
      if !pinned.contains(spec_hash) {
        blockers.push(format!("result ref {result_ref} does not match the task spec pinned by this task's runs"));
      }
    } else if let Some(review_id) = result_ref.strip_prefix(REVIEW_REF_PREFIX) {
      match records.get(review_id) {
        Some(run) if run.kind == RunKind::Review && run.session_id == meta.id => {
          if facts.run_outcomes.get(review_id).map(String::as_str) != Some("success") {
            blockers.push(format!("result ref {result_ref} names a review without a successful run_finished fact"));
          }
        }
        _ => blockers.push(format!("result ref {result_ref} does not name a review Run of task {}", meta.id)),
      }
    } else if let Some(body) = result_ref.strip_prefix(LANDING_REF_PREFIX) {
      let (branch, commit) = body.split_once('@').unwrap_or((body, ""));
      if branch.is_empty() || commit.is_empty() {
        blockers.push(format!("result ref {result_ref} must be landed:<branch>@<commit>"));
      } else {
        let mut base_branches =
          records.values().filter_map(|run| run.base_branch.as_deref()).map(str::to_string).collect::<HashSet<_>>();
        if let Some(base_branch) = meta.task.as_ref().and_then(|task| task.base_branch.as_deref()) {
          base_branches.insert(base_branch.to_string());
        }
        let targets = base_branches
          .iter()
          .flat_map(|base| [base.clone(), review_text::review_landing_target(base)])
          .collect::<HashSet<_>>();
        if !base_branches.is_empty() && !targets.contains(branch) {
          let mut bases = base_branches.into_iter().collect::<Vec<_>>();
          bases.sort();
          blockers
            .push(format!("result ref {result_ref} lands on {branch}; this task's runs target {}", bases.join(", ")));
        }
      }
    }
    blockers
  }

  fn landing_claims(&self, evidence: &CompletionEvidence) -> Vec<(String, String, Option<String>)> {
    let mut claims = Vec::new();
    if let Some(landing) = &evidence.landing {
      claims.push((landing.branch.clone(), landing.commit.clone(), landing.repo_path.clone()));
    }
    for result_ref in &evidence.result_refs {
      if let Some(body) = result_ref.strip_prefix(LANDING_REF_PREFIX)
        && let Some((branch, commit)) =
          body.split_once('@').filter(|(branch, commit)| !branch.is_empty() && !commit.is_empty())
      {
        claims.push((branch.to_string(), commit.to_string(), None));
      }
    }
    claims
  }

  async fn landing_blockers(&self, meta: &SessionMetadata, evidence: &CompletionEvidence) -> StoreResult<Vec<String>> {
    let mut blockers = Vec::new();
    let claims = self.landing_claims(evidence);
    if claims.is_empty() {
      return Ok(blockers);
    }
    let mut fallback_repo = meta.task.as_ref().and_then(|task| task.repo_path.clone());
    if fallback_repo.is_none() {
      fallback_repo = self.store().list_run_records_sync(&meta.id)?.into_iter().find_map(|run| run.repo_path);
    }
    for (branch, commit, repo_override) in claims {
      let Some(repo) = repo_override.or_else(|| fallback_repo.clone()) else {
        blockers.push(format!(
          "landing {branch}@{commit} cannot be verified: no repository is recorded for this task or its runs"
        ));
        continue;
      };
      let (landed, reason) = git::git_verify_commit_landed(Path::new(&repo), &branch, &commit).await;
      if !landed {
        blockers.push(format!("landing evidence unverified in {repo}: {branch}@{commit}: {reason}"));
      }
    }
    Ok(blockers)
  }

  pub async fn verified_evidence_blockers(
    &self,
    meta: &SessionMetadata,
    evidence: &CompletionEvidence,
    succeeding_run_ids: Option<&HashSet<String>>,
  ) -> StoreResult<Vec<String>> {
    let mut blockers = self.evidence_blockers_with_success(meta, evidence, succeeding_run_ids)?;
    blockers.extend(self.landing_blockers(meta, evidence).await?);
    Ok(blockers)
  }

  // ------------------------------------------------------------------
  // Archive
  // ------------------------------------------------------------------

  /// Archive one task: an operator closes its subtree; an agent's request waits for its own Run.
  pub async fn archive_task(
    &self,
    session_id: &str,
    request_id: &str,
    evidence: &CompletionEvidence,
    caller: &CallerIdentity,
  ) -> StoreResult<(usize, Value)> {
    if request_id.is_empty() {
      return Err(StoreError::invalid("request_id is required for archive"));
    }
    match caller {
      CallerIdentity::Operator { .. } => {
        if let Some(replay) = self.replay_close_request(session_id, request_id)? {
          return Ok(replay);
        }
        let summary = if evidence.summary.is_empty() { "archived by the user" } else { &evidence.summary };
        let archived = self
          .archive_subtree(session_id, Some(request_id), summary, &evidence.result_refs, &evidence.run_ids, caller)
          .await?;
        Ok((200, json!({"archived": archived})))
      }
      CallerIdentity::Agent { claims } => {
        if claims.session_id != session_id {
          return Err(StoreError::forbidden("an agent may only archive its own task"));
        }
        let meta = self.store().load_task_meta(session_id).await?;
        if meta.profile != TaskProfile::Manager {
          return Err(StoreError::forbidden("own-task archive requests come from manager tasks only"));
        }
        let run = self
          .store()
          .read_run_sync(session_id, &claims.run_id)?
          .ok_or_else(|| StoreError::invalid(format!("caller run {} not found in task {session_id}", claims.run_id)))?;
        let events = self.store().load_all_facts(session_id)?;
        if run_records::terminal_outcome(&events, &run.id)?.is_some()
          || self.store().run_display_state(session_id, &run.id)? != RunDisplayState::Running
        {
          return Err(StoreError::conflict(vec![format!(
            "caller run {} is not active; it cannot request archive",
            claims.run_id
          )]));
        }
        if let Some(replay) = self.replay_close_request(session_id, request_id)? {
          return Ok(replay);
        }
        let blockers = self.own_run_close_blockers(session_id, &run, evidence).await?;
        if !blockers.is_empty() {
          return Err(StoreError::conflict(sorted_unique(blockers)));
        }
        self.save_close_request(session_id, request_id, &claims.run_id, evidence).await
      }
    }
  }

  /// Build an archive close fact with its fixed report recipient and subtree owner.
  #[allow(clippy::too_many_arguments)]
  pub fn archived_close_event(
    &self,
    session_id: &str,
    request_id: &str,
    summary: &str,
    report_to: Option<&str>,
    archived_with: Option<&str>,
    actor: &str,
    result_refs: &[String],
    run_ids: &[String],
  ) -> Value {
    let mut payload = vec![
      ("request_id".to_string(), json!(request_id)),
      ("outcome".to_string(), json!("archived")),
      ("summary".to_string(), json!(summary)),
      ("result_refs".to_string(), json!(result_refs)),
      ("run_ids".to_string(), json!(run_ids)),
      ("report_to".to_string(), report_to.map_or(Value::Null, |parent| json!(parent))),
    ];
    if let Some(archived_with) = archived_with {
      payload.push(("archived_with".to_string(), json!(archived_with)));
    }
    control_events::build_control_event(
      event_types::TASK_CLOSED,
      actor,
      session_id,
      Some(&control_events::stable_close_event_id(session_id, request_id)),
      None,
      payload,
    )
  }

  /// Archive an open task subtree and report the target close to its parent.
  pub async fn archive_subtree(
    &self,
    session_id: &str,
    request_id: Option<&str>,
    summary: &str,
    result_refs: &[String],
    run_ids: &[String],
    caller: &CallerIdentity,
  ) -> StoreResult<Vec<String>> {
    if !caller.is_operator() {
      return Err(StoreError::forbidden(ARCHIVE_OPERATOR_REFUSAL));
    }
    let request_id = match request_id {
      None => format!("archive-{}", Uuid::new_v4()),
      Some("") => return Err(StoreError::invalid("request_id is required for archive")),
      Some(request_id) => request_id.to_string(),
    };
    let store = self.store();
    let (mut subtree, mut parent_id) = {
      let _guard = store.control_lock().await;
      store.invalidate_tree_index();
      let meta = store.load_task_meta(session_id).await?;
      if store.task_state(session_id)? != "open" {
        return Ok(Vec::new());
      }
      let subtree = self.subtree_ids(session_id)?;
      let blockers = self.archive_run_blockers(&subtree)?;
      if !blockers.is_empty() {
        return Err(StoreError::conflict(blockers));
      }
      (subtree, meta.task_parent_id.clone())
    };

    let mut epochs = HashMap::new();
    for sid in &subtree {
      epochs.insert(sid.clone(), store.prime_aggregator(sid).await?);
    }
    let mut parent_epoch = match parent_id.as_deref() {
      Some(parent_id) => Some(store.prime_aggregator(parent_id).await?),
      None => None,
    };

    let mut archived: Vec<String> = Vec::new();
    let mut announcements: Vec<(String, ChatEvent, u64)> = Vec::new();
    let mut report = ChatEvent::Null;
    let mut report_created = false;
    {
      let guard = store.control_lock().await;
      store.invalidate_tree_index();
      let target_meta = store.load_task_meta(session_id).await?;
      if store.task_state(session_id)? != "open" {
        return Ok(Vec::new());
      }
      subtree = self.subtree_ids(session_id)?;
      let blockers = self.archive_run_blockers(&subtree)?;
      if !blockers.is_empty() {
        return Err(StoreError::conflict(blockers));
      }
      parent_id.clone_from(&target_meta.task_parent_id);
      if let Some(parent) = parent_id.as_deref()
        && !epochs.contains_key(parent)
      {
        parent_epoch = Some(store.prime_aggregator(parent).await?);
      }
      for sid in &subtree {
        if store.task_state(sid)? != "open" {
          continue;
        }
        let is_target = sid == session_id;
        let epoch = match epochs.get(sid) {
          Some(epoch) => *epoch,
          None => {
            let epoch = store.prime_aggregator(sid).await?;
            epochs.insert(sid.clone(), epoch);
            epoch
          }
        };
        let (close_event, delivered_report, created) = self
          .append_closed(
            &guard,
            CloseAppend {
              session_id: sid,
              request_id: &request_id,
              outcome: "archived",
              summary: if is_target { summary } else { "" },
              result_refs: if is_target { result_refs } else { &[] },
              run_ids: if is_target { run_ids } else { &[] },
              actor: ACTOR_USER,
              report_to: if is_target { parent_id.as_deref() } else { None },
              archived_with: if is_target { None } else { Some(session_id) },
              sequence_outcome: None,
            },
          )
          .await?;
        archived.push(sid.clone());
        announcements.push((sid.clone(), close_event, epoch));
        if is_target {
          report = delivered_report;
          report_created = created;
        }
      }
      for sid in &archived {
        store.sequence_archive_on_archive(sid).await?;
      }
    }

    for (sid, event, epoch) in announcements {
      store.announce_appended_event(&sid, &event, epoch).await?;
    }
    if report_created && let (Some(parent_id), Some(epoch)) = (parent_id.as_deref(), parent_epoch) {
      store.announce_appended_event(parent_id, &report, epoch).await?;
    }
    if report_created
      && let Some(parent_id) = parent_id.as_deref()
      && let Err(error) = self.dispatcher()?.wake_parent(parent_id, &report).await
    {
      tracing::warn!(target: "charliebot_tasks", session_id, parent = parent_id, report = ?report.get("id"), error = %error, "archive_parent_wake_failed");
    }
    Ok(archived)
  }

  /// The target and every descendant, parent before child.
  fn subtree_ids(&self, session_id: &str) -> StoreResult<Vec<String>> {
    let mut subtree = vec![session_id.to_string()];
    subtree.extend(self.store().descendant_ids(session_id)?);
    Ok(subtree)
  }

  /// Every unfinished Run blocker across a subtree, before any close fact is written.
  fn archive_run_blockers(&self, subtree: &[String]) -> StoreResult<Vec<String>> {
    let mut blockers = Vec::new();
    for sid in subtree {
      for blocker in self.store().run_blockers(sid)? {
        blockers.push(format!("{sid}: {}", blocker.reason));
      }
    }
    blockers.sort();
    Ok(blockers)
  }

  /// Close an elone source with archive facts while its caller holds the tree lock.
  pub async fn archive_replaced_source_locked(
    &self,
    guard: &ControlGuard,
    session_id: &str,
    successor_id: &str,
  ) -> StoreResult<()> {
    if self.store().task_state(session_id)? != "open" {
      return Ok(());
    }
    let request_id = format!("elone-{}", Uuid::new_v4());
    let summary = format!("archived by elone successor {successor_id}");
    self
      .append_closed(
        guard,
        CloseAppend {
          session_id,
          request_id: &request_id,
          outcome: "archived",
          summary: &summary,
          result_refs: &[],
          run_ids: &[],
          actor: ACTOR_USER,
          report_to: None,
          archived_with: None,
          sequence_outcome: None,
        },
      )
      .await?;
    Ok(())
  }

  /// Archive a failed worker after its report lands, when no work remains.
  pub async fn archive_unsuccessful_worker(&self, session_id: &str, run_id: &str, outcome: &str) -> StoreResult<bool> {
    let store = self.store();
    let epoch = store.prime_aggregator(session_id).await?;
    let request_id = format!("unsuccessful:{run_id}");
    let close_event;
    {
      let guard = store.control_lock().await;
      let meta = store.load_task_meta(session_id).await?;
      if meta.profile != TaskProfile::Worker || store.task_state(session_id)? != "open" {
        return Ok(false);
      }
      if store.last_ended_run_id(session_id)?.as_deref() != Some(run_id) {
        return Ok(false);
      }
      if !self.execution_blockers(session_id, &HashSet::new())?.is_empty() {
        return Ok(false);
      }
      if self.pause_reason(session_id)?.is_some() {
        return Ok(false);
      }
      if store.load_all_facts_view_sync(session_id)?.iter().any(|event| {
        event.get("type").and_then(CompactValue::as_str) == Some(event_types::TASK_CLOSED)
          && event.get("request_id").and_then(CompactValue::as_str) == Some(request_id.as_str())
      }) {
        return Ok(false);
      }
      let summary = format!("run {run_id} ended {outcome}; archived by the server");
      let run_ids = [run_id.to_string()];
      let (event, _report, _created) = self
        .append_closed(
          &guard,
          CloseAppend {
            session_id,
            request_id: &request_id,
            outcome: "archived",
            summary: &summary,
            result_refs: &[],
            run_ids: &run_ids,
            actor: ACTOR_SYSTEM,
            report_to: None,
            archived_with: None,
            sequence_outcome: None,
          },
        )
        .await?;
      close_event = event;
      store.sequence_archive_on_archive(session_id).await?;
    }
    store.announce_appended_event(session_id, &close_event, epoch).await?;
    Ok(true)
  }

  fn replay_close_request(&self, session_id: &str, request_id: &str) -> StoreResult<Option<(usize, Value)>> {
    // Python's `_replay_close_request` scans `fact_history` (`task_completion.py:544-563`), which returns its shared
    // cached event dictionaries. Keep the same full-history scan without cloning every event for each replayed run.
    let events = self.store().load_all_facts_view_sync(session_id)?;
    for event in events.iter() {
      if event.get("type").and_then(CompactValue::as_str) == Some(event_types::TASK_CLOSE_REQUESTED)
        && event.get("request_id").and_then(CompactValue::as_str) == Some(request_id)
      {
        return Ok(Some((
          202,
          json!({"session_id": session_id, "request_id": request_id, "status": "pending_run_finish"}),
        )));
      }
    }
    for event in events.iter() {
      if event.get("type").and_then(CompactValue::as_str) == Some(event_types::TASK_CLOSED)
        && event.get("request_id").and_then(CompactValue::as_str) == Some(request_id)
      {
        return Ok(Some((
          200,
          json!({"session_id": session_id, "closed_event_id": event.get("id").map(CompactValue::to_value)}),
        )));
      }
    }
    Ok(None)
  }

  async fn own_run_close_blockers(
    &self,
    session_id: &str,
    run: &RunRecord,
    evidence: &CompletionEvidence,
  ) -> StoreResult<Vec<String>> {
    let blockers = {
      let _guard = self.store().control_lock().await;
      let meta = self.store().load_task_meta(session_id).await?;
      let excluded_runs = HashSet::from([run.id.clone()]);
      let excluded_inputs = run.input_event_ids.iter().cloned().collect::<HashSet<_>>();
      let mut blockers = self.completion_blockers(session_id, &excluded_runs, &excluded_inputs)?;
      blockers.extend(self.evidence_blockers_with_success(&meta, evidence, Some(&excluded_runs))?);
      blockers
    };
    let meta = self.store().load_task_meta(session_id).await?;
    let mut blockers = blockers;
    blockers.extend(self.landing_blockers(&meta, evidence).await?);
    Ok(blockers)
  }

  async fn save_close_request(
    &self,
    session_id: &str,
    request_id: &str,
    owner_run_id: &str,
    evidence: &CompletionEvidence,
  ) -> StoreResult<(usize, Value)> {
    let guard = self.store().control_lock().await;
    if let Some(replay) = self.replay_close_request(session_id, request_id)? {
      return Ok(replay);
    }
    let event = control_events::build_control_event(
      event_types::TASK_CLOSE_REQUESTED,
      ACTOR_SYSTEM,
      session_id,
      Some(&control_events::stable_close_request_event_id(session_id, request_id)),
      None,
      [
        ("request_id".to_string(), json!(request_id)),
        ("owner_run_id".to_string(), json!(owner_run_id)),
        ("summary".to_string(), json!(evidence.summary)),
        ("result_refs".to_string(), json!(evidence.result_refs)),
        ("run_ids".to_string(), json!(evidence.run_ids)),
      ],
    );
    self.store().append_control_fact(&guard, session_id, event).await?;
    drop(guard);
    tracing::info!(target: "charliebot_tasks", session_id, request_id, owner_run_id, "{}", event_types::TASK_CLOSE_REQUESTED);
    Ok((202, json!({"session_id": session_id, "request_id": request_id, "status": "pending_run_finish"})))
  }

  #[allow(clippy::too_many_arguments)]
  async fn close_now(
    &self,
    session_id: &str,
    request_id: &str,
    evidence: &CompletionEvidence,
    actor: &str,
    outcome: &str,
    exclude_run_ids: &HashSet<String>,
    exclude_input_ids: &HashSet<String>,
  ) -> StoreResult<(usize, Value)> {
    Ok(
      self
        .close_now_inner(
          CloseNowRequest { session_id, request_id, evidence, actor, outcome, exclude_run_ids, exclude_input_ids },
          None,
        )
        .await?
        .expect("a close without a startup stop completes"),
    )
  }

  async fn close_now_until(
    &self,
    request: CloseNowRequest<'_>,
    stop: &StartupStop,
  ) -> StoreResult<Option<(usize, Value)>> {
    self.close_now_inner(request, Some(stop)).await
  }

  async fn close_now_inner(
    &self,
    request: CloseNowRequest<'_>,
    stop: Option<&StartupStop>,
  ) -> StoreResult<Option<(usize, Value)>> {
    let CloseNowRequest { session_id, request_id, evidence, actor, outcome, exclude_run_ids, exclude_input_ids } =
      request;
    let succeeding_run_ids = (outcome == "archived").then(|| exclude_run_ids.clone());
    let succeeding = succeeding_run_ids.as_ref();
    let (meta, blockers) = {
      let _guard = self.store().control_lock().await;
      let meta = self.store().load_task_meta(session_id).await?;
      let blockers = match stop {
        Some(stop) => {
          let mut should_stop = || stop.is_cancelled();
          match self.completion_blockers_until(session_id, exclude_run_ids, exclude_input_ids, &mut should_stop)? {
            Some(blockers) => blockers,
            None => return Ok(None),
          }
        }
        None => self.completion_blockers(session_id, exclude_run_ids, exclude_input_ids)?,
      };
      (meta, blockers)
    };
    if !blockers.is_empty() {
      return Err(StoreError::conflict(sorted_unique(blockers)));
    }
    if stop.is_some_and(StartupStop::is_cancelled) {
      return Ok(None);
    }
    let evidence_blockers = self.verified_evidence_blockers(&meta, evidence, succeeding).await?;
    if !evidence_blockers.is_empty() {
      return Err(StoreError::conflict(sorted_unique(evidence_blockers)));
    }
    if stop.is_some_and(StartupStop::is_cancelled) {
      return Ok(None);
    }

    let child_epoch = self.store().prime_aggregator(session_id).await?;
    if stop.is_some_and(StartupStop::is_cancelled) {
      return Ok(None);
    }
    let parent_epoch = match meta.task_parent_id.as_deref() {
      Some(parent_id) => Some(self.store().prime_aggregator(parent_id).await?),
      None => None,
    };
    if stop.is_some_and(StartupStop::is_cancelled) {
      return Ok(None);
    }
    let (fresh_meta, close_event, report, report_created) = {
      let guard = self.store().control_lock().await;
      if stop.is_some_and(StartupStop::is_cancelled) {
        return Ok(None);
      }
      if self.store().task_state(session_id)? != "open" {
        return Err(StoreError::conflict(vec![format!("task {session_id} is no longer open")]));
      }
      let fresh_meta = self.store().load_task_meta(session_id).await?;
      let mut blockers = match stop {
        Some(stop) => {
          let mut should_stop = || stop.is_cancelled();
          match self.completion_blockers_until(session_id, exclude_run_ids, exclude_input_ids, &mut should_stop)? {
            Some(blockers) => blockers,
            None => return Ok(None),
          }
        }
        None => self.completion_blockers(session_id, exclude_run_ids, exclude_input_ids)?,
      };
      blockers.extend(self.evidence_blockers_with_success(&fresh_meta, evidence, succeeding)?);
      let blockers = sorted_unique(blockers);
      if !blockers.is_empty() {
        return Err(StoreError::conflict(blockers));
      }
      let (close_event, report, report_created) = self
        .append_closed(
          &guard,
          CloseAppend {
            session_id,
            request_id,
            outcome,
            summary: &evidence.summary,
            result_refs: &evidence.result_refs,
            run_ids: &evidence.run_ids,
            actor,
            report_to: fresh_meta.task_parent_id.as_deref(),
            archived_with: None,
            sequence_outcome: None,
          },
        )
        .await?;
      (fresh_meta, close_event, report, report_created)
    };
    self.store().announce_appended_event(session_id, &close_event, child_epoch).await?;
    if report_created && let (Some(parent_id), Some(epoch)) = (fresh_meta.task_parent_id.as_deref(), parent_epoch) {
      self.store().announce_appended_event(parent_id, &report, epoch).await?;
    }
    if report_created
      && let Some(parent_id) = fresh_meta.task_parent_id.as_deref()
      && let Err(error) = self.dispatcher()?.wake_parent(parent_id, &report).await
    {
      tracing::warn!(target: "charliebot_tasks", session_id, parent = parent_id, report = ?report.get("id"), error = %error, "close_parent_wake_failed");
    }
    Ok(Some((
      200,
      json!({"session_id": session_id, "closed_event_id": close_event.get("id").map(CompactValue::to_value)}),
    )))
  }

  /// Append one `task_closed` fact, then deliver its parent report. The caller holds the control lock.
  async fn append_closed(
    &self,
    guard: &ControlGuard,
    append: CloseAppend<'_>,
  ) -> StoreResult<(ChatEvent, ChatEvent, bool)> {
    let CloseAppend {
      session_id,
      request_id,
      outcome,
      summary,
      result_refs,
      run_ids,
      actor,
      report_to,
      archived_with,
      sequence_outcome,
    } = append;
    let close_event = match outcome {
      "archived" => self.archived_close_event(
        session_id,
        request_id,
        summary,
        report_to,
        archived_with,
        actor,
        result_refs,
        run_ids,
      ),
      "completed" | "cancelled" => {
        let mut payload = vec![
          ("request_id".to_string(), json!(request_id)),
          ("outcome".to_string(), json!(outcome)),
          ("summary".to_string(), json!(summary)),
          ("result_refs".to_string(), json!(result_refs)),
          ("run_ids".to_string(), json!(run_ids)),
          ("report_to".to_string(), report_to.map_or(Value::Null, |parent| json!(parent))),
        ];
        if let Some(sequence_outcome) = sequence_outcome {
          payload.push(("sequence_outcome".to_string(), json!(sequence_outcome)));
        }
        control_events::build_control_event(
          event_types::TASK_CLOSED,
          actor,
          session_id,
          Some(&control_events::stable_close_event_id(session_id, request_id)),
          None,
          payload,
        )
      }
      other => return Err(StoreError::new(format!("unknown task close outcome: {other}"))),
    };
    let close_event = self.store().append_control_fact(guard, session_id, close_event).await?;
    let report_outcome = sequence_outcome.unwrap_or(outcome);
    let (report, report_created) = self
      .dispatcher()?
      .deliver_child_report_locked_as_actor(
        guard,
        session_id,
        close_event.clone(),
        report_outcome,
        summary,
        Some(result_refs),
        report_to,
        ACTOR_SYSTEM,
      )
      .await?;
    self.store().invalidate_tree_index();
    Ok((close_event, report, report_created))
  }

  /// Cancel an open task, stopping non-terminal runs when the target is a worker.
  pub async fn cancel_task(
    &self,
    session_id: &str,
    request_id: &str,
    reason: &str,
    caller: &CallerIdentity,
  ) -> StoreResult<Value> {
    let (actor, target_meta) = match caller {
      CallerIdentity::Operator { .. } => (ACTOR_USER, None),
      CallerIdentity::Agent { claims } => {
        let target = self.store().load_task_meta(session_id).await?;
        if target.task_parent_id.as_deref() != Some(claims.session_id.as_str()) {
          return Err(StoreError::forbidden("an agent may only cancel a direct child of its own task"));
        }
        (ACTOR_AGENT, Some(target))
      }
    };
    if request_id.is_empty() {
      return Err(StoreError::invalid("request_id is required for cancellation"));
    }
    if reason.trim().is_empty() {
      return Err(StoreError::invalid("cancellation requires a reason"));
    }
    let meta = match target_meta {
      Some(meta) => meta,
      None => self.store().load_task_meta(session_id).await?,
    };
    if meta.profile == TaskProfile::Worker {
      return self.cancel_worker_task(session_id, request_id, reason, actor, &meta).await;
    }
    if let Some((_, replay)) = self.replay_close_request(session_id, request_id)? {
      return with_stopped_runs(replay, Vec::new());
    }
    let child_epoch = self.store().prime_aggregator(session_id).await?;
    let parent_epoch = match meta.task_parent_id.as_deref() {
      Some(parent_id) => Some(self.store().prime_aggregator(parent_id).await?),
      None => None,
    };
    let (fresh_meta, close_event, report, report_created) = {
      let guard = self.store().control_lock().await;
      let state = self.store().task_state(session_id)?;
      if state != "open" {
        return Err(StoreError::conflict(vec![format!(
          "task {session_id} is {state}; only an open task can be cancelled"
        )]));
      }
      if let Some((_, replay)) = self.replay_close_request(session_id, request_id)? {
        return with_stopped_runs(replay, Vec::new());
      }
      let fresh_meta = self.store().load_task_meta(session_id).await?;
      let blockers = sorted_unique(self.cancellation_blockers(session_id)?);
      if !blockers.is_empty() {
        return Err(StoreError::conflict(blockers));
      }
      let (close_event, report, report_created) = self
        .append_closed(
          &guard,
          CloseAppend {
            session_id,
            request_id,
            outcome: "cancelled",
            summary: reason,
            result_refs: &[],
            run_ids: &[],
            actor,
            report_to: fresh_meta.task_parent_id.as_deref(),
            archived_with: None,
            sequence_outcome: None,
          },
        )
        .await?;
      (fresh_meta, close_event, report, report_created)
    };
    self.store().announce_appended_event(session_id, &close_event, child_epoch).await?;
    if report_created && let (Some(parent_id), Some(epoch)) = (fresh_meta.task_parent_id.as_deref(), parent_epoch) {
      self.store().announce_appended_event(parent_id, &report, epoch).await?;
    }
    if report_created
      && let Some(parent_id) = fresh_meta.task_parent_id.as_deref()
      && let Err(error) = self.dispatcher()?.wake_parent(parent_id, &report).await
    {
      tracing::warn!(target: "charliebot_tasks", session_id, parent = parent_id, report = ?report.get("id"), error = %error, "cancel_parent_wake_failed");
    }
    Ok(
      json!({"session_id": session_id, "closed_event_id": close_event.get("id").map(CompactValue::to_value), "stopped_runs": []}),
    )
  }

  async fn cancel_worker_task(
    &self,
    session_id: &str,
    request_id: &str,
    reason: &str,
    actor: &str,
    target_meta: &SessionMetadata,
  ) -> StoreResult<Value> {
    let stop_request_id = format!("cancel:{request_id}");
    let child_epoch = self.store().prime_aggregator(session_id).await?;
    let parent_epoch = match target_meta.task_parent_id.as_deref() {
      Some(parent_id) => Some(self.store().prime_aggregator(parent_id).await?),
      None => None,
    };
    let (close_event, report, report_created, closed_event_id, stop_runs) = {
      let guard = self.store().control_lock().await;
      let fresh_meta = self.store().load_task_meta(session_id).await?;
      if let Some((status, replay)) = self.replay_close_request(session_id, request_id)? {
        if status == 202 {
          return with_stopped_runs(replay, Vec::new());
        }
        let closed_event_id = replay
          .get("closed_event_id")
          .cloned()
          .ok_or_else(|| StoreError::new("closed worker replay has no closed_event_id"))?;
        let stop_runs = self.cancellation_stop_runs(session_id, &stop_request_id)?;
        (None, ChatEvent::Null, false, closed_event_id, stop_runs)
      } else {
        let state = self.store().task_state(session_id)?;
        if state != "open" {
          return Err(StoreError::conflict(vec![format!(
            "task {session_id} is {state}; only an open task can be cancelled"
          )]));
        }
        let mut open_descendants = Vec::new();
        for descendant in self.store().descendant_ids(session_id)? {
          if self.store().task_state(&descendant)? == "open" {
            open_descendants.push(format!("has open descendant task {descendant}"));
          }
        }
        if !open_descendants.is_empty() {
          return Err(StoreError::conflict(open_descendants));
        }
        let events = self.store().load_all_facts(session_id)?;
        let run_records = self.store().list_run_records_sync(session_id)?;
        let mut sequence_blockers = Vec::new();
        let mut stoppable_runs = Vec::new();
        for run in run_records {
          if run_records::terminal_outcome(&events, &run.id)?.is_some() {
            continue;
          }
          if run.sequence_ref.is_some() {
            sequence_blockers.push(format!("run {} is owned by a sequence controller", run.id));
          }
          stoppable_runs.push(run);
        }
        if !sequence_blockers.is_empty() {
          return Err(StoreError::conflict(sequence_blockers));
        }
        let (close_event, report, report_created) = self
          .append_closed(
            &guard,
            CloseAppend {
              session_id,
              request_id,
              outcome: "cancelled",
              summary: reason,
              result_refs: &[],
              run_ids: &[],
              actor,
              report_to: fresh_meta.task_parent_id.as_deref(),
              archived_with: None,
              sequence_outcome: None,
            },
          )
          .await?;
        for run in &stoppable_runs {
          if !self.store().append_stop_request_locked(&guard, session_id, &run.id, &stop_request_id).await? {
            return Err(StoreError::new(format!(
              "run {} became terminal while the worker cancel lock was held",
              run.id
            )));
          }
        }
        let stop_runs = self.cancellation_stop_runs(session_id, &stop_request_id)?;
        let closed_event_id = close_event
          .get("id")
          .map(CompactValue::to_value)
          .ok_or_else(|| StoreError::new("worker close event has no id"))?;
        (Some(close_event), report, report_created, closed_event_id, stop_runs)
      }
    };

    let mut identity_error = None;
    for run in &stop_runs {
      if run.pid.is_none() {
        continue;
      }
      match self.store().reconcile_stop_request(session_id, &run.id).await {
        Ok(_) => {}
        Err(error @ StoreError::Conflict(_)) => {
          if identity_error.is_none() {
            identity_error = Some(error);
          } else {
            tracing::warn!(
              target: "charliebot_tasks",
              session_id,
              run_id = %run.id,
              error = %error,
              "worker_cancel_stop_identity_conflict"
            );
          }
        }
        Err(error) => return Err(error),
      }
    }

    if let Some(close_event) = close_event {
      self.store().announce_appended_event(session_id, &close_event, child_epoch).await?;
      if report_created && let (Some(parent_id), Some(epoch)) = (target_meta.task_parent_id.as_deref(), parent_epoch) {
        self.store().announce_appended_event(parent_id, &report, epoch).await?;
      }
      if report_created
        && let Some(parent_id) = target_meta.task_parent_id.as_deref()
        && let Err(error) = self.dispatcher()?.wake_parent(parent_id, &report).await
      {
        tracing::warn!(
          target: "charliebot_tasks",
          session_id,
          parent = parent_id,
          report = ?report.get("id"),
          error = %error,
          "cancel_parent_wake_failed"
        );
      }
    }
    if let Some(error) = identity_error {
      return Err(error);
    }
    let stopped_runs = self.cancellation_stop_results(session_id, &stop_request_id)?;
    Ok(json!({"session_id": session_id, "closed_event_id": closed_event_id, "stopped_runs": stopped_runs}))
  }

  fn cancellation_stop_runs(&self, session_id: &str, request_id: &str) -> StoreResult<Vec<RunRecord>> {
    let events = self.store().load_all_facts(session_id)?;
    let mut records = self
      .store()
      .list_run_records_sync(session_id)?
      .into_iter()
      .map(|run| (run.id.clone(), run))
      .collect::<HashMap<_, _>>();
    let mut stopped_runs = Vec::new();
    let mut seen = HashSet::new();
    for event in events {
      if event.get("type").and_then(CompactValue::as_str) != Some(event_types::RUN_STOP_REQUESTED)
        || event.get("request_id").and_then(CompactValue::as_str) != Some(request_id)
      {
        continue;
      }
      let run_id = event
        .get("run_id")
        .and_then(CompactValue::as_str)
        .ok_or_else(|| StoreError::new("run_stop_requested fact has no string run_id"))?;
      if !seen.insert(run_id.to_string()) {
        continue;
      }
      let run = records
        .remove(run_id)
        .ok_or_else(|| StoreError::not_found(format!("run {run_id} named by cancellation stop was not found")))?;
      stopped_runs.push(run);
    }
    Ok(stopped_runs)
  }

  fn cancellation_stop_results(&self, session_id: &str, request_id: &str) -> StoreResult<Vec<Value>> {
    let events = self.store().load_all_facts(session_id)?;
    self
      .cancellation_stop_runs(session_id, request_id)?
      .into_iter()
      .map(|run| {
        let outcome = if run.pid.is_none() {
          json!("never_launched")
        } else {
          match run_records::terminal_outcome(&events, &run.id)? {
            Some(RunOutcomeKind::Interrupted) => json!("interrupted"),
            None => Value::Null,
            Some(outcome) => {
              return Err(StoreError::new(format!(
                "cancelled run {} has unexpected terminal outcome {outcome:?}",
                run.id
              )));
            }
          }
        };
        Ok(json!({"run_id": run.id, "outcome": outcome}))
      })
      .collect()
  }

  /// Restore one archived task node and its archived ancestors.
  pub async fn restore_task(
    &self,
    session_id: &str,
    request_id: &str,
    reason: &str,
    caller: &CallerIdentity,
  ) -> StoreResult<Value> {
    if !caller.is_operator() {
      return Err(StoreError::forbidden("task restore requires operator credentials"));
    }
    if request_id.is_empty() {
      return Err(StoreError::invalid("request_id is required for restore"));
    }
    let restored = self.restore_chain(session_id, request_id, reason).await?;
    Ok(json!({"session_id": session_id, "restored": restored}))
  }

  /// Restore the target and each archived ancestor, topmost first.
  pub async fn restore_chain(&self, session_id: &str, request_id: &str, reason: &str) -> StoreResult<Vec<String>> {
    let (restored, announcements) = {
      let guard = self.store().control_lock().await;
      self.restore_chain_locked(&guard, session_id, request_id, reason).await?
    };
    for (node_id, event, epoch) in announcements {
      self.store().announce_appended_event(&node_id, &event, epoch).await?;
    }
    Ok(restored)
  }

  /// Restore an archived chain while the caller holds the control lock.
  pub async fn restore_chain_locked(
    &self,
    guard: &ControlGuard,
    session_id: &str,
    request_id: &str,
    reason: &str,
  ) -> StoreResult<RestoreOutcome> {
    let mut chain = Vec::new();
    let mut current = session_id.to_string();
    let mut seen = HashSet::new();
    loop {
      let meta = self.store().load_task_meta(&current).await?;
      if self.store().task_state(&current)? == "open" {
        break;
      }
      chain.push(current.clone());
      if meta.task_parent_id.is_none() || meta.task_parent_id.as_ref().is_some_and(|parent| seen.contains(parent)) {
        break;
      }
      seen.insert(current);
      current = meta.task_parent_id.expect("parent id is present after the prior check");
      if chain.len() > RESTORE_CHAIN_HOP_LIMIT {
        return Err(StoreError::conflict(vec![format!(
          "ancestor chain of {session_id} exceeds {RESTORE_CHAIN_HOP_LIMIT} hops"
        )]));
      }
    }
    if chain.is_empty() {
      return Ok((Vec::new(), Vec::new()));
    }
    let mut restored = Vec::new();
    let mut announcements = Vec::new();
    for node_id in chain.into_iter().rev() {
      let history = self.store().load_all_facts_view_sync(&node_id)?;
      let mut latest_lifecycle = None;
      let mut last_close = None;
      for event in history.iter().rev() {
        let event_type = event.get("type").and_then(CompactValue::as_str);
        if latest_lifecycle.is_none()
          && matches!(event_type, Some(event_types::TASK_CLOSED | event_types::TASK_REOPENED))
        {
          latest_lifecycle = Some(event);
        }
        if last_close.is_none() && event_type == Some(event_types::TASK_CLOSED) {
          last_close = Some(event);
        }
        if latest_lifecycle.is_some() && last_close.is_some() {
          break;
        }
      }
      if latest_lifecycle.is_some_and(|event| {
        event.get("type").and_then(CompactValue::as_str) == Some(event_types::TASK_REOPENED)
          && event.get("request_id").and_then(CompactValue::as_str) == Some(request_id)
      }) {
        restored.push(node_id);
        continue;
      }
      let close_id = last_close.and_then(|event| event.get("id")).and_then(CompactValue::as_str);
      let event_scope = close_id.map_or_else(|| request_id.to_string(), |close_id| format!("{request_id}:{close_id}"));
      let event = control_events::build_control_event(
        event_types::TASK_REOPENED,
        ACTOR_USER,
        &node_id,
        Some(&control_events::stable_reopen_event_id(&node_id, &event_scope)),
        None,
        [
          ("request_id".to_string(), json!(request_id)),
          ("closed_event_id".to_string(), close_id.map_or(Value::Null, |id| json!(id))),
          ("reason".to_string(), json!(reason)),
        ],
      );
      let epoch = self.store().prime_aggregator(&node_id).await?;
      let event = self.store().append_control_fact(guard, &node_id, event).await?;
      restored.push(node_id.clone());
      announcements.push((node_id, event, epoch));
    }
    self.store().invalidate_tree_index();
    Ok((restored, announcements))
  }

  /// Acknowledge the exact pending inputs an operator handled out of band.
  pub async fn acknowledge_inputs(
    &self,
    session_id: &str,
    request_id: &str,
    input_ids: &[String],
    note: &str,
    caller: &CallerIdentity,
  ) -> StoreResult<Value> {
    if !caller.is_operator() {
      return Err(StoreError::forbidden("acknowledging task input requires operator credentials"));
    }
    if request_id.is_empty() {
      return Err(StoreError::invalid("request_id is required for input acknowledgement"));
    }
    if input_ids.is_empty() {
      return Err(StoreError::invalid("input_ids is required and must name at least one input"));
    }
    if input_ids.iter().collect::<HashSet<_>>().len() != input_ids.len() {
      return Err(StoreError::invalid("input_ids must not repeat an id"));
    }
    if let Some(replay) = self.replay_input_ack(session_id, request_id)? {
      return Ok(replay);
    }
    let epoch = self.store().prime_aggregator(session_id).await?;
    let (event, response) = {
      let guard = self.store().control_lock().await;
      self.store().load_task_meta(session_id).await?;
      if let Some(replay) = self.replay_input_ack(session_id, request_id)? {
        return Ok(replay);
      }
      let pending = self.dispatcher()?.pending_inputs(session_id)?;
      let acknowledged = self.store().facts_of(session_id)?.confirmed_input_ids;
      let pending_ids =
        pending.iter().filter_map(|event| event.get("id").and_then(CompactValue::as_str)).collect::<HashSet<_>>();
      let unknown = input_ids
        .iter()
        .filter(|input_id| !pending_ids.contains(input_id.as_str()) && !acknowledged.contains(input_id.as_str()))
        .cloned()
        .collect::<Vec<_>>();
      if !unknown.is_empty() {
        return Err(StoreError::conflict(vec![dispatch::inputs_not_pending_conflict(session_id, &unknown)]));
      }
      let acked_now =
        input_ids.iter().filter(|input_id| pending_ids.contains(input_id.as_str())).cloned().collect::<Vec<_>>();
      if acked_now.is_empty() {
        return Ok(json!({
          "session_id": session_id,
          "acknowledged_event_id": null,
          "input_ids": [],
          "already_acknowledged": input_ids,
        }));
      }
      let event = control_events::build_control_event(
        event_types::TASK_INPUT_ACKNOWLEDGED,
        ACTOR_USER,
        session_id,
        Some(&control_events::stable_input_ack_event_id(session_id, request_id)),
        None,
        [
          ("request_id".to_string(), json!(request_id)),
          ("input_ids".to_string(), json!(acked_now)),
          ("note".to_string(), json!(note)),
        ],
      );
      let event = self.store().append_control_fact(&guard, session_id, event).await?;
      self.store().invalidate_tree_index();
      let already_acknowledged = input_ids.iter().filter(|id| !acked_now.contains(id)).cloned().collect::<Vec<_>>();
      let response = json!({
        "session_id": session_id,
        "acknowledged_event_id": event.get("id").map(CompactValue::to_value),
        "input_ids": acked_now,
        "already_acknowledged": already_acknowledged,
      });
      (event, response)
    };
    self.store().announce_appended_event(session_id, &event, epoch).await?;
    Ok(response)
  }

  fn replay_input_ack(&self, session_id: &str, request_id: &str) -> StoreResult<Option<Value>> {
    let event_id = control_events::stable_input_ack_event_id(session_id, request_id);
    for event in self.store().load_all_facts(session_id)? {
      if event.get("type").and_then(CompactValue::as_str) == Some(event_types::TASK_INPUT_ACKNOWLEDGED)
        && event.get("id").and_then(CompactValue::as_str) == Some(event_id.as_str())
      {
        return Ok(Some(json!({
          "session_id": session_id,
          "acknowledged_event_id": event_id,
          "input_ids": event.get("input_ids").map(CompactValue::to_value).unwrap_or(Value::Null),
          "already_acknowledged": [],
        })));
      }
    }
    Ok(None)
  }

  /// Close an ended sequence's worker child and report the sequence outcome.
  pub async fn close_sequence_child(
    &self,
    session_id: &str,
    request_id: &str,
    outcome: SequenceCloseOutcome,
    evidence: &CompletionEvidence,
  ) -> StoreResult<(usize, Value)> {
    if request_id.is_empty() {
      return Err(StoreError::invalid("request_id is required for sequence close"));
    }
    if let Some(replay) = self.replay_close_request(session_id, request_id)? {
      return Ok(replay);
    }
    let sequence_outcome = outcome.as_str();
    let completing = outcome == SequenceCloseOutcome::Completed && !evidence.run_ids.is_empty();
    let pre_meta = self.store().load_task_meta(session_id).await?;
    let blockers = {
      let _guard = self.store().control_lock().await;
      if self.store().task_state(session_id)? != "open" {
        return Err(StoreError::conflict(vec![format!("task {session_id} is no longer open")]));
      }
      self.cancellation_blockers(session_id)?
    };
    if !blockers.is_empty() {
      return Err(StoreError::conflict(sorted_unique(blockers)));
    }
    if completing {
      let blockers = self.verified_evidence_blockers(&pre_meta, evidence, None).await?;
      if !blockers.is_empty() {
        return Err(StoreError::conflict(sorted_unique(blockers)));
      }
    }
    let child_epoch = self.store().prime_aggregator(session_id).await?;
    let parent_epoch = match pre_meta.task_parent_id.as_deref() {
      Some(parent_id) => Some(self.store().prime_aggregator(parent_id).await?),
      None => None,
    };
    let (fresh_meta, close_event, report, report_created) = {
      let guard = self.store().control_lock().await;
      if let Some(replay) = self.replay_close_request(session_id, request_id)? {
        return Ok(replay);
      }
      if self.store().task_state(session_id)? != "open" {
        return Err(StoreError::conflict(vec![format!("task {session_id} is no longer open")]));
      }
      let fresh_meta = self.store().load_task_meta(session_id).await?;
      let mut blockers = self.cancellation_blockers(session_id)?;
      if completing {
        blockers.extend(self.evidence_blockers(&fresh_meta, evidence)?);
      }
      let blockers = sorted_unique(blockers);
      if !blockers.is_empty() {
        return Err(StoreError::conflict(blockers));
      }
      let lifecycle_outcome = if completing { "completed" } else { "cancelled" };
      let (close_event, report, report_created) = self
        .append_closed(
          &guard,
          CloseAppend {
            session_id,
            request_id,
            outcome: lifecycle_outcome,
            summary: &evidence.summary,
            result_refs: &evidence.result_refs,
            run_ids: &evidence.run_ids,
            actor: ACTOR_SYSTEM,
            report_to: fresh_meta.task_parent_id.as_deref(),
            archived_with: None,
            sequence_outcome: Some(sequence_outcome),
          },
        )
        .await?;
      (fresh_meta, close_event, report, report_created)
    };
    self.store().announce_appended_event(session_id, &close_event, child_epoch).await?;
    if report_created && let (Some(parent_id), Some(epoch)) = (fresh_meta.task_parent_id.as_deref(), parent_epoch) {
      self.store().announce_appended_event(parent_id, &report, epoch).await?;
    }
    if report_created
      && let Some(parent_id) = fresh_meta.task_parent_id.as_deref()
      && let Err(error) = self.dispatcher()?.wake_parent(parent_id, &report).await
    {
      tracing::warn!(target: "charliebot_tasks", session_id, parent = parent_id, report = ?report.get("id"), error = %error, "sequence_close_parent_wake_failed");
    }
    Ok((200, json!({"session_id": session_id, "closed_event_id": close_event.get("id").map(CompactValue::to_value)})))
  }

  /// Recheck saved own-run close requests after the matching run succeeds.
  pub async fn recheck_close_requests(&self, session_id: &str, run_id: &str) -> StoreResult<Vec<String>> {
    Ok(
      self
        .recheck_close_requests_inner(session_id, run_id, None)
        .await?
        .expect("a close-request recheck without a startup stop completes"),
    )
  }

  /// Recheck saved own-run close requests during startup, returning `None` when recovery is stopped.
  pub async fn recheck_close_requests_until(
    &self,
    session_id: &str,
    run_id: &str,
    stop: &StartupStop,
  ) -> StoreResult<Option<Vec<String>>> {
    self.recheck_close_requests_inner(session_id, run_id, Some(stop)).await
  }

  async fn recheck_close_requests_inner(
    &self,
    session_id: &str,
    run_id: &str,
    stop: Option<&StartupStop>,
  ) -> StoreResult<Option<Vec<String>>> {
    if stop.is_some_and(StartupStop::is_cancelled) {
      return Ok(None);
    }
    let facts = self.store().node_facts(session_id)?;
    let task = facts.task()?;
    if task.run_outcomes.get(run_id).map(String::as_str) != Some("success") {
      return Ok(Some(Vec::new()));
    }
    let close_requests = task.close_requests.clone();
    drop(facts);
    let mut remaining = Vec::new();
    for request in close_requests {
      if stop.is_some_and(StartupStop::is_cancelled) {
        return Ok(None);
      }
      if request.get("owner_run_id").and_then(CompactValue::as_str) != Some(run_id) {
        continue;
      }
      let Some(request_id) = request.get("request_id").and_then(CompactValue::as_str).filter(|value| !value.is_empty())
      else {
        continue;
      };
      if self.replay_close_request(session_id, request_id)?.is_some_and(|(status, _)| status == 200) {
        continue;
      }
      let request_value = request.to_value();
      let evidence = evidence_from_close_request(&request_value)?;
      let run = self.store().read_run_sync(session_id, run_id)?;
      let claimed = run.map(|run| run.input_event_ids.into_iter().collect()).unwrap_or_default();
      let exclude_run_ids = HashSet::from([run_id.to_string()]);
      let result = match stop {
        Some(stop) => match self
          .close_now_until(
            CloseNowRequest {
              session_id,
              request_id,
              evidence: &evidence,
              actor: ACTOR_SYSTEM,
              outcome: "archived",
              exclude_run_ids: &exclude_run_ids,
              exclude_input_ids: &claimed,
            },
            stop,
          )
          .await?
        {
          Some(result) => Ok(result),
          None => return Ok(None),
        },
        None => {
          self.close_now(session_id, request_id, &evidence, ACTOR_SYSTEM, "archived", &exclude_run_ids, &claimed).await
        }
      };
      match result {
        Ok(_) => {}
        Err(StoreError::Conflict(blockers)) => {
          remaining.extend(blockers.iter().cloned());
          tracing::info!(target: "charliebot_tasks", session_id, request_id, blockers = ?blockers, "task_close_request_still_blocked");
          if self.store().node_facts(session_id)?.task()?.task_state == "open" {
            self.notify_close_request_blocked(session_id, request_id, &blockers).await?;
          }
        }
        Err(error) => return Err(error),
      }
    }
    Ok(Some(remaining))
  }

  async fn notify_close_request_blocked(
    &self,
    session_id: &str,
    request_id: &str,
    blockers: &[String],
  ) -> StoreResult<()> {
    let lines = blockers.iter().map(|blocker| format!("- {blocker}")).collect::<Vec<_>>().join("\n");
    let content = format!(
      "Closure request {request_id} did not close this task. Blockers:\n{lines}\nResolve them, then send a new closure request."
    );
    let notice_id = control_events::stable_close_blocked_notice_id(session_id, request_id);
    match self
      .dispatcher()?
      .admit_input(session_id, event_types::SCHEDULED_TRIGGER, &content, ACTOR_SYSTEM, json!({"input_id": notice_id}))
      .await
    {
      Ok(_) => self.dispatcher()?.dispatch_pending(session_id).await.map(|_| ()),
      Err(StoreError::Archived(message)) => {
        tracing::info!(target: "charliebot_tasks", session_id, error = %message, "close_blocked_notice_skipped_archived");
        Ok(())
      }
      Err(error) => Err(error),
    }
  }

  /// Build the delivery pause reason from pending triggers and unprocessed input.
  pub fn pause_reason(&self, session_id: &str) -> StoreResult<Option<String>> {
    let triggers_dir = self.store().session_dir(session_id).join("triggers");
    let (pending_triggers, _next_fire) = charliebot_store::trigger_files::pending_trigger_state(triggers_dir);
    if pending_triggers > 0 {
      return Ok(Some(format!("has {pending_triggers} pending trigger(s)")));
    }
    let pending = self.dispatcher()?.pending_inputs(session_id)?;
    if !pending.is_empty() {
      return Ok(Some(dispatch::unprocessed_input_blocker(&pending)));
    }
    Ok(None)
  }

  /// Apply successful-run close requests and ordinary worker delivery follow-ups.
  pub async fn after_run_finished(&self, session_id: &str, run_id: &str) -> StoreResult<()> {
    let meta = self.store().load_task_meta(session_id).await?;
    self.recheck_close_requests(session_id, run_id).await?;
    if self.store().node_facts(session_id)?.task()?.run_outcomes.get(run_id).map(String::as_str) != Some("success") {
      return Ok(());
    }
    let Some(run) = self.store().read_run_sync(session_id, run_id)? else {
      return Ok(());
    };
    if meta.profile != TaskProfile::Worker || run.kind != RunKind::Work {
      return Ok(());
    }
    if meta.task.as_ref().is_some_and(|task| task.task_type == Some(TaskType::Implement)) {
      tracing::info!(target: "charliebot_tasks", session_id, run_id, "worker_delivery_awaits_review");
      return Ok(());
    }
    if meta.task.as_ref().is_some_and(|task| task.task_type == Some(TaskType::ColdRead)) {
      tracing::info!(target: "charliebot_tasks", session_id, run_id, "cold_read_delivery_awaits_report");
      return Ok(());
    }
    if let Some(reason) = self.pause_reason(session_id)? {
      tracing::info!(target: "charliebot_tasks", session_id, run_id, reason, "worker_delivery_paused");
      return Ok(());
    }
    match self.evaluate_automatic_completion(session_id, run_id, None, None, None, None).await {
      Ok(_) => Ok(()),
      Err(StoreError::Conflict(blockers)) => {
        tracing::info!(target: "charliebot_tasks", session_id, run_id, blockers = ?blockers, "automatic_worker_completion_blocked");
        Ok(())
      }
      Err(error) => Err(error),
    }
  }

  /// Close a successful worker run with its default or caller-supplied evidence.
  pub async fn evaluate_automatic_completion(
    &self,
    session_id: &str,
    run_id: &str,
    summary: Option<&str>,
    result_refs: Option<&[String]>,
    request_id: Option<&str>,
    evidence: Option<&CompletionEvidence>,
  ) -> StoreResult<(usize, Value)> {
    let meta = self.store().load_task_meta(session_id).await?;
    if meta.profile != TaskProfile::Worker {
      return Err(StoreError::forbidden("automatic completion applies to worker tasks"));
    }
    let facts = self.store().node_facts(session_id)?;
    let facts = facts.task()?;
    if facts.run_outcomes.get(run_id).map(String::as_str) != Some("success") {
      return Err(StoreError::conflict(vec![format!(
        "run {run_id} has no successful run_finished fact; failed or interrupted evidence keeps the task open"
      )]));
    }
    let run = self
      .store()
      .read_run_sync(session_id, run_id)?
      .ok_or_else(|| StoreError::not_found(run_records::run_not_found_in_task_text(run_id, session_id)))?;
    let effective_evidence = if let Some(evidence) = evidence {
      evidence.clone()
    } else {
      let mut refs = result_refs.map(<[String]>::to_vec).unwrap_or_else(|| vec![format!("{RUN_REF_PREFIX}{run_id}")]);
      if let Some(spec_hash) = run.task_spec_hash {
        refs.push(format!("{SPEC_REF_PREFIX}{spec_hash}"));
      }
      CompletionEvidence {
        summary: summary
          .filter(|summary| !summary.is_empty())
          .map_or_else(|| format!("Run {run_id} completed"), str::to_string),
        result_refs: refs,
        run_ids: vec![run_id.to_string()],
        ..CompletionEvidence::default()
      }
    };
    let effective_request_id =
      request_id.filter(|request_id| !request_id.is_empty()).map_or_else(|| format!("auto:{run_id}"), str::to_string);
    if let Some(replay) = self.replay_close_request(session_id, &effective_request_id)? {
      return Ok(replay);
    }
    self
      .close_now(
        session_id,
        &effective_request_id,
        &effective_evidence,
        ACTOR_SYSTEM,
        "completed",
        &HashSet::new(),
        &HashSet::new(),
      )
      .await
  }
}

fn with_stopped_runs(mut response: Value, stopped_runs: Vec<Value>) -> StoreResult<Value> {
  let object = response.as_object_mut().ok_or_else(|| StoreError::new("cancel response is not a JSON object"))?;
  object.insert("stopped_runs".to_string(), json!(stopped_runs));
  Ok(response)
}

impl RunFinishedCompletion for TaskCompletion {
  fn after_run_finished<'a>(&'a self, session_id: &'a str, run_id: &'a str) -> BoxFuture<'a, StoreResult<()>> {
    Box::pin(async move { TaskCompletion::after_run_finished(self, session_id, run_id).await })
  }
}

impl ReplacedSourceArchive for TaskCompletion {
  fn archive_replaced_source_locked<'a>(
    &'a self,
    _store: &'a charliebot_store::Store,
    guard: &'a ControlGuard,
    source_id: &'a str,
    successor_id: &'a str,
  ) -> StoreHookFuture<'a, ()> {
    Box::pin(async move { TaskCompletion::archive_replaced_source_locked(self, guard, source_id, successor_id).await })
  }
}

impl SequenceCompletion for TaskCompletion {
  fn evaluate_automatic_completion<'a>(
    &'a self,
    session_id: &'a str,
    run_id: &'a str,
    summary: Option<&'a str>,
    result_refs: Option<&'a [String]>,
    request_id: Option<&'a str>,
  ) -> BoxFuture<'a, StoreResult<(usize, Value)>> {
    Box::pin(async move {
      TaskCompletion::evaluate_automatic_completion(self, session_id, run_id, summary, result_refs, request_id, None)
        .await
    })
  }

  fn close_sequence_child<'a>(
    &'a self,
    session_id: &'a str,
    request_id: &'a str,
    outcome: &'a str,
    evidence: &'a Value,
  ) -> BoxFuture<'a, StoreResult<(usize, Value)>> {
    Box::pin(async move {
      let outcome = match outcome {
        "completed" => SequenceCloseOutcome::Completed,
        "blocked" => SequenceCloseOutcome::Blocked,
        "failed" => SequenceCloseOutcome::Failed,
        "cancelled" => SequenceCloseOutcome::Cancelled,
        _ => return Err(StoreError::invalid(format!("unknown sequence close outcome {outcome:?}"))),
      };
      let evidence: CompletionEvidence = serde_json::from_value(evidence.clone())
        .map_err(|error| StoreError::invalid(format!("invalid sequence close evidence: {error}")))?;
      TaskCompletion::close_sequence_child(self, session_id, request_id, outcome, &evidence).await
    })
  }

  fn restore_chain<'a>(
    &'a self,
    session_id: &'a str,
    request_id: &'a str,
    reason: &'a str,
  ) -> BoxFuture<'a, StoreResult<Vec<String>>> {
    Box::pin(async move { TaskCompletion::restore_chain(self, session_id, request_id, reason).await })
  }

  fn restore_chain_locked<'a>(
    &'a self,
    guard: &'a ControlGuard,
    session_id: &'a str,
    request_id: &'a str,
    reason: &'a str,
  ) -> BoxFuture<'a, StoreResult<RestoreOutcome>> {
    Box::pin(async move { TaskCompletion::restore_chain_locked(self, guard, session_id, request_id, reason).await })
  }

  fn archive_unsuccessful_worker<'a>(
    &'a self,
    session_id: &'a str,
    run_id: &'a str,
    outcome: &'a str,
  ) -> BoxFuture<'a, StoreResult<bool>> {
    Box::pin(async move { TaskCompletion::archive_unsuccessful_worker(self, session_id, run_id, outcome).await })
  }
}

/// Return the completion owner installed in startup state.
pub fn task_completion(features: &FeatureContext) -> Result<Arc<TaskCompletion>, StateError> {
  features.startup.require::<TaskCompletion>()
}

/// Recheck saved close requests for one successfully finished run.
pub async fn recheck_close_requests(
  features: &FeatureContext,
  session_id: &str,
  run_id: &str,
) -> StoreResult<Vec<String>> {
  task_completion(features)
    .map_err(|error| StoreError::new(error.to_string()))?
    .recheck_close_requests(session_id, run_id)
    .await
}

/// Apply completion follow-ups for one finished run.
pub async fn after_run_finished(features: &FeatureContext, session_id: &str, run_id: &str) -> StoreResult<()> {
  task_completion(features)
    .map_err(|error| StoreError::new(error.to_string()))?
    .after_run_finished(session_id, run_id)
    .await
}

/// Return the pending-trigger or pending-input pause reason for one task.
pub fn pause_reason(features: &FeatureContext, session_id: &str) -> StoreResult<Option<String>> {
  task_completion(features).map_err(|error| StoreError::new(error.to_string()))?.pause_reason(session_id)
}

fn evidence_from_close_request(request: &Value) -> StoreResult<CompletionEvidence> {
  let string_field = |name: &str| request.get(name).and_then(Value::as_str).map(str::to_string).unwrap_or_default();
  let strings_field = |name: &str| -> StoreResult<Vec<String>> {
    match request.get(name) {
      None | Some(Value::Null) => Ok(Vec::new()),
      Some(Value::Array(values)) => values
        .iter()
        .map(|value| {
          value
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| StoreError::new(format!("saved close request {name} must contain strings")))
        })
        .collect(),
      Some(_) => Err(StoreError::new(format!("saved close request {name} must be an array"))),
    }
  };
  Ok(CompletionEvidence {
    summary: string_field("summary"),
    result_refs: strings_field("result_refs")?,
    run_ids: strings_field("run_ids")?,
    ..CompletionEvidence::default()
  })
}

fn sorted_unique(mut values: Vec<String>) -> Vec<String> {
  values.sort();
  values.dedup();
  values
}

#[cfg(test)]
mod tests {
  use std::collections::HashSet;

  use super::TaskCompletion;
  use charliebot_store::metadata::{SessionMetadata, TaskProfile};
  use charliebot_store::run_records::{RunKind, RunRecord};
  use charliebot_store::{CreateTask, HistoryWork, TaskCaller};
  use serde_json::{Map, json};

  use crate::test_fixtures;

  fn run_record(session_id: &str, run_id: String) -> RunRecord {
    RunRecord {
      id: run_id,
      session_id: session_id.to_string(),
      kind: RunKind::Work,
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

  async fn add_runs(features: &charliebot_registry::context::FeatureContext, meta: &SessionMetadata, count: usize) {
    let guard = features.store.control_lock().await;
    for index in 0..count {
      let run = run_record(&meta.id, format!("{}-synthetic-run-{index:03}", meta.id));
      features.store.register_run_locked(&guard, &meta.id, run, None).await.unwrap();
    }
  }

  #[tokio::test]
  async fn delivery_outcomes_reuse_sealed_facts_for_many_runs() {
    let home = tempfile::tempdir().unwrap();
    let features = test_fixtures::claude_features(&home.path().join("home"), &test_fixtures::claude_config(None));
    let parent = test_fixtures::manager_session(&features, "delivery-parent", "fable", None, "claude-parent").await;
    let child = features
      .store
      .create_task(
        CreateTask {
          request_id: "delivery-child".to_string(),
          task_parent_id: Some(parent.id.clone()),
          profile: TaskProfile::Manager,
          task: None,
          name: Some("Synthetic child".to_string()),
          backend: Some("fable".to_string()),
          group: None,
          session_id: None,
        },
        TaskCaller::System,
        Map::new(),
      )
      .await
      .unwrap();
    add_runs(&features, &parent, 10).await;
    add_runs(&features, &child, 5).await;
    for index in 0..4 {
      features
        .store
        .append_chat_event(&parent.id, json!({"type":"user","content":format!("parent-{index}")}))
        .await
        .unwrap();
    }
    for index in 0..2 {
      features
        .store
        .append_chat_event(&child.id, json!({"type":"user","content":format!("child-{index}")}))
        .await
        .unwrap();
    }
    features.store.load_chat_events_shared_sync(&parent.id).unwrap();
    features.store.load_chat_events_shared_sync(&child.id).unwrap();
    // The seal keeps only facts that were folded at eviction, and a task-state read no longer folds them.
    features.store.node_facts(&parent.id).unwrap();
    features.store.node_facts(&child.id).unwrap();
    features.store.evict_recovery_history(&parent.id);
    features.store.evict_recovery_history(&child.id);
    let before: HistoryWork = features.store.history_work();

    let outcomes = TaskCompletion::new(std::sync::Arc::clone(&features)).delivery_run_outcomes(&parent).unwrap();

    let after = features.store.history_work();
    assert_eq!(outcomes.0.len(), 15);
    // Node-state folds from the tree index also count in `full_folds`; the outcomes read only node facts.
    let facts_folds =
      (after.full_folds - before.full_folds) - (after.node_state_full_folds - before.node_state_full_folds);
    let facts_events =
      (after.folded_events - before.folded_events) - (after.node_state_folded_events - before.node_state_folded_events);
    assert_eq!(facts_folds, 0, "the parent and child facts remain sealed after event-cache eviction");
    assert_eq!(facts_events, 0, "sealed facts avoid refolding either history");
  }

  #[tokio::test]
  async fn restore_chain_loads_each_closed_node_history_once_without_owned_copies() {
    let home = tempfile::tempdir().unwrap();
    let features = test_fixtures::claude_features(&home.path().join("home"), &test_fixtures::claude_config(None));
    let parent = test_fixtures::manager_session(&features, "restore-parent", "fable", None, "claude-parent").await;
    let child = features
      .store
      .create_task(
        CreateTask {
          request_id: "restore-child".to_string(),
          task_parent_id: Some(parent.id.clone()),
          profile: TaskProfile::Manager,
          task: None,
          name: Some("Synthetic restore child".to_string()),
          backend: Some("fable".to_string()),
          group: None,
          session_id: None,
        },
        TaskCaller::System,
        Map::new(),
      )
      .await
      .unwrap();
    for session_id in [&parent.id, &child.id] {
      for index in 0..2 {
        features
          .store
          .append_chat_event(session_id, json!({"type":"user","content":format!("history-{index}")}))
          .await
          .unwrap();
      }
      features
        .store
        .append_chat_event(
          session_id,
          json!({"id":format!("{session_id}-close"),"type":"task_closed","outcome":"completed"}),
        )
        .await
        .unwrap();
      features.store.node_state(session_id).unwrap();
      features.store.evict_recovery_history(session_id);
    }
    let before: HistoryWork = features.store.history_work();

    let completion = TaskCompletion::new(std::sync::Arc::clone(&features));
    let guard = features.store.control_lock().await;
    let (restored, _announcements) =
      completion.restore_chain_locked(&guard, &child.id, "restore-count", "synthetic restore").await.unwrap();
    drop(guard);

    let after = features.store.history_work();
    assert_eq!(restored, [parent.id.clone(), child.id.clone()]);
    assert_eq!(after.file_reads - before.file_reads, 2, "one history read for each node in the closed chain");
    assert_eq!(after.event_history_views - before.event_history_views, 2);
    assert_eq!(after.owned_event_clones - before.owned_event_clones, 0, "the restore scan borrows the shared history");
  }

  #[tokio::test]
  async fn completion_blockers_check_stop_during_tree_index_build() {
    let home = tempfile::tempdir().unwrap();
    let features = test_fixtures::claude_features(&home.path().join("home"), &test_fixtures::claude_config(None));
    let root = test_fixtures::manager_session(&features, "stop-root", "fable", None, "claude-parent").await;
    let child = features
      .store
      .create_task(
        CreateTask {
          request_id: "stop-child".to_string(),
          task_parent_id: Some(root.id.clone()),
          profile: TaskProfile::Manager,
          task: None,
          name: Some("Synthetic child".to_string()),
          backend: Some("fable".to_string()),
          group: None,
          session_id: None,
        },
        TaskCaller::System,
        Map::new(),
      )
      .await
      .unwrap();
    features
      .store
      .create_task(
        CreateTask {
          request_id: "stop-leaf".to_string(),
          task_parent_id: Some(child.id),
          profile: TaskProfile::Worker,
          task: None,
          name: Some("Synthetic leaf".to_string()),
          backend: Some("fable".to_string()),
          group: None,
          session_id: None,
        },
        TaskCaller::System,
        Map::new(),
      )
      .await
      .unwrap();
    features.store.invalidate_tree_index();
    let builds_before = features.store.task_tree_index_build_count();
    let completion = TaskCompletion::new(std::sync::Arc::clone(&features));
    let mut checks = 0;

    let blockers = completion
      .completion_blockers_until(&root.id, &HashSet::new(), &HashSet::new(), &mut || {
        checks += 1;
        checks == 5
      })
      .unwrap();

    assert!(blockers.is_none(), "a stopped blocker scan returns no partial blocker list");
    assert_eq!(checks, 5, "completion checks the stop during task-index directory scanning");
    assert_eq!(features.store.task_tree_index_build_count(), builds_before, "the partial task index is discarded");
  }
}
