//! The backend resolution and conflict check of a steps firing, which the sequence controller applies to each step.

use std::collections::HashMap;

use charliebot_core::config::CharlieBotConfig;

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
