//! The backend resolution and the next-step judgment of a steps firing.
//!
//! A step runs on a backend id, which selects through the shared resolver when its run is registered. The sequence
//! controller starts each step with these functions, and the scheduler reads the next-step judgment to decide whether a
//! round is still in flight. Neither caller is imported by this module.

use std::collections::{BTreeMap, HashMap};

use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_registry::context::FeatureContext;
use charliebot_store::run_records::{RunFacts, RunOutcomeKind, RunRecord};

use crate::config::{ScheduledTaskConfig, StepConfig, effective_backend};

/// The member a step's run is registered on, or the reason no member is selectable.
pub(crate) enum StepMember {
  Selected(BackendOption),
  /// The step's backend id has no selectable member; the message lists every member and why it is out.
  Exhausted(String),
}

/// The backend id a step runs on: its own, else the task's effective backend.
pub(crate) fn step_backend_id(
  task: &ScheduledTaskConfig,
  cfg: &CharlieBotConfig,
  backend: Option<&str>,
) -> anyhow::Result<String> {
  match backend.filter(|backend| !backend.is_empty()) {
    Some(backend) => Ok(backend.to_string()),
    None => effective_backend(task, cfg),
  }
}

/// The member a step's run is registered on. A model id selects itself through the same filters as a one-member pool.
/// A selected pool moves its rotation past the member, and `exclude` names the members the selection leaves out.
pub(crate) fn select_step_member(
  features: &FeatureContext,
  cfg: &CharlieBotConfig,
  backend: &str,
  exclude: &[String],
) -> anyhow::Result<StepMember> {
  let selector = charliebot_agents::model_pool::Selector::from_features(features, cfg)?;
  match selector.select(backend, exclude)? {
    Some(option) => Ok(StepMember::Selected(option)),
    None => Ok(StepMember::Exhausted(selector.exhausted_message(backend)?)),
  }
}

/// The members a step's selection leaves out: the member its `distinct_backend_from` source step ran on. A step with no
/// source, or whose source has no run yet, leaves out none.
pub(crate) fn distinct_exclusion(
  steps: &[StepConfig],
  position: usize,
  runs: &BTreeMap<usize, RunRecord>,
) -> Vec<String> {
  let Some(source) = steps[position].distinct_backend_from.as_deref() else { return Vec::new() };
  let Some(source_position) = steps.iter().position(|step| step.name == source) else { return Vec::new() };
  runs.get(&source_position).and_then(|run| run.backend.clone()).into_iter().collect()
}

/// The model a step's run records for `option`, when the option names one.
pub(crate) fn option_model(option: &BackendOption) -> Option<String> {
  (!option.model.is_empty()).then(|| option.model.clone())
}

fn option_of<'a>(cfg: &'a CharlieBotConfig, backend: &str) -> anyhow::Result<&'a BackendOption> {
  cfg.get_backend_option(backend).ok_or_else(|| anyhow::anyhow!("scheduled task backend '{backend}' is not configured"))
}

/// The first step whose `distinct_backend_from` pair resolves to the same concrete backend and model, as a report.
/// A pair with a pool step is not compared here: the pool step selects a member that leaves the source's member out.
pub(crate) fn distinct_backend_conflict(
  task: &ScheduledTaskConfig,
  cfg: &CharlieBotConfig,
) -> anyhow::Result<Option<String>> {
  let Some(steps) = task.steps.as_ref() else { return Ok(None) };
  // A duplicated step name resolves to its last position, from this one-pass index.
  let positions = steps.iter().enumerate().map(|(index, step)| (step.name.as_str(), index)).collect::<HashMap<_, _>>();
  for step in steps {
    let Some(prior_name) = step.distinct_backend_from.as_deref() else { continue };
    let prior_position = positions
      .get(prior_name)
      .copied()
      .ok_or_else(|| anyhow::anyhow!("step '{}' names missing distinct backend source '{prior_name}'", step.name))?;
    let prior = &steps[prior_position];
    let backend = step_backend_id(task, cfg, step.backend.as_deref())?;
    let prior_backend = step_backend_id(task, cfg, prior.backend.as_deref())?;
    if cfg.backends.pool(&backend).is_some() || cfg.backends.pool(&prior_backend).is_some() {
      continue;
    }
    let own = option_of(cfg, &backend)?;
    let old = option_of(cfg, &prior_backend)?;
    if own.backend_type == old.backend_type && option_model(own) == option_model(old) {
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
        option_model(own).as_deref().unwrap_or("None")
      )));
    }
  }
  Ok(None)
}

/// Whether a steps firing has a next step still to start, read from its persisted runs.
///
/// The answer is true when no step run is pending, the last step passed with exit 0, and a later step exists. This
/// mirrors the next-step branch of `reconcile_bound_firing`, which starts that step. A conflicting backend makes that
/// branch report and stop instead, so the answer is false for it.
pub(crate) fn steps_chain_awaits_next_step(
  task: &ScheduledTaskConfig,
  cfg: &CharlieBotConfig,
  runs: &[RunRecord],
  facts: &RunFacts,
) -> anyhow::Result<bool> {
  let Some(steps) = task.steps.as_ref().filter(|steps| !steps.is_empty()) else { return Ok(false) };
  let mut by_position = BTreeMap::new();
  for run in runs {
    if let Some(sequence) = run.sequence_ref.as_ref().filter(|sequence| sequence.kind == "cron_steps") {
      by_position.insert(sequence.position as usize, run);
    }
  }
  let Some((&last_position, last_run)) = by_position.last_key_value() else { return Ok(false) };
  if by_position.values().any(|run| !facts.has_terminal_fact(&run.id)) {
    return Ok(false);
  }
  let passed =
    matches!(facts.terminal_outcome(&last_run.id)?, Some(RunOutcomeKind::Success)) && last_run.exit_code == Some(0);
  Ok(passed && last_position + 1 < steps.len() && distinct_backend_conflict(task, cfg)?.is_none())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::test_support::feature_context;
  use charliebot_agents::backend_availability;

  #[test]
  fn a_model_and_its_singleton_pool_share_step_selection_and_cooling() {
    let (_home, features) = feature_context();
    let cfg = features.config.get().unwrap();
    let model = select_step_member(&features, &cfg, "synthetic", &[]).unwrap();
    let pool = select_step_member(&features, &cfg, "pool", &[]).unwrap();
    let selected_id = |member| match member {
      StepMember::Selected(option) => option.id,
      StepMember::Exhausted(message) => panic!("expected a selectable member, got {message}"),
    };
    assert_eq!(selected_id(model), selected_id(pool));

    backend_availability::backend_availability(&features)
      .unwrap()
      .record("synthetic", None, backend_availability::REASON_QUOTA_ERROR, "synthetic quota error")
      .unwrap();
    let model = select_step_member(&features, &cfg, "synthetic", &[]).unwrap();
    let pool = select_step_member(&features, &cfg, "pool", &[]).unwrap();
    let exhaustion = |member| match member {
      StepMember::Exhausted(message) => message,
      StepMember::Selected(option) => panic!("expected an exhausted selection, got {}", option.id),
    };
    assert!(exhaustion(model).contains("- synthetic: quota_error"));
    assert!(exhaustion(pool).contains("- synthetic: quota_error"));
  }
}
