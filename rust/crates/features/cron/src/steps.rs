//! The backend resolution and the next-step judgment of a steps firing.
//!
//! The sequence controller starts each step with these functions, and the scheduler reads the next-step judgment to
//! decide whether a round is still in flight. Neither caller is imported by this module.

use std::collections::{BTreeMap, HashMap};

use charliebot_core::config::CharlieBotConfig;
use charliebot_store::run_records::{RunFacts, RunOutcomeKind, RunRecord};

use crate::config::ScheduledTaskConfig;

pub(crate) fn resolved_backend(
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

pub(crate) fn distinct_backend_conflict(
  task: &ScheduledTaskConfig,
  cfg: &CharlieBotConfig,
) -> anyhow::Result<Option<String>> {
  let Some(steps) = task.steps.as_ref() else { return Ok(None) };
  // A duplicated step name resolves to its last position, from this one-pass index.
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
