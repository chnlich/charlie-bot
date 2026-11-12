//! Scheduled-task request and file models.

use charliebot_compat::request_body::{BodyModel, Field, FieldType};
use charliebot_core::config::CharlieBotConfig;
use serde::{Deserialize, Serialize};

/// The default time zone of a scheduled task and of a create request: `charliebot_core::pacific::ZONE`, the house zone.
pub const DEFAULT_TIMEZONE: &str = charliebot_core::pacific::ZONE;

/// A named step in an ordered scheduled-worker chain.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StepConfig {
  pub name: String,
  #[serde(default)]
  pub prompt_file: Option<String>,
  #[serde(default)]
  pub prompt: Option<String>,
  #[serde(default)]
  pub backend: Option<String>,
  #[serde(default)]
  pub distinct_backend_from: Option<String>,
}

/// How a bound task's round reaches its session: admitted to the bound manager (`Master`), or started as the
/// task's own worker run (`Worker`). An absent `mode` starts worker work.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskMode {
  Master,
  Worker,
}

/// A complete scheduled task after prompt pointers have been resolved.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduledTaskConfig {
  pub name: String,
  pub cron: String,
  #[serde(default)]
  pub prompt_file: Option<String>,
  #[serde(default)]
  pub repo: Option<String>,
  #[serde(default)]
  pub backend: Option<String>,
  #[serde(default = "default_timezone")]
  pub timezone: String,
  #[serde(default = "default_true")]
  pub enabled: bool,
  #[serde(default)]
  pub project: Option<String>,
  #[serde(default)]
  pub allow_failure: bool,
  #[serde(default)]
  pub session_id: Option<String>,
  #[serde(default)]
  pub mode: Option<TaskMode>,
  #[serde(default)]
  pub prompt: Option<String>,
  #[serde(default)]
  pub handler: Option<String>,
  #[serde(default)]
  pub steps: Option<Vec<StepConfig>>,
}

/// A cron file that could not be loaded.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ScheduledTaskError {
  pub name: String,
  pub path: String,
  pub error: String,
  pub enabled: Option<bool>,
}

/// The source, mode and sequence rules shared with the YAML loader, each returning its message when broken.
pub fn validate_task(task: &ScheduledTaskConfig) -> Result<(), String> {
  let source_count = usize::from(task.prompt.as_ref().is_some_and(|value| !value.is_empty()))
    + usize::from(task.steps.as_ref().is_some_and(|value| !value.is_empty()))
    + usize::from(task.handler.as_ref().is_some_and(|value| !value.is_empty()));
  if source_count != 1 {
    return Err("task must have exactly one of 'prompt', 'prompt_file', 'steps', or 'handler'".to_string());
  }
  if task.mode.is_some() && task.session_id.as_deref().unwrap_or_default().is_empty() {
    return Err("'mode' requires 'session_id' (mode selects how a bound task fires)".to_string());
  }
  if task.mode == Some(TaskMode::Master) {
    if task.prompt.as_deref().unwrap_or_default().is_empty() {
      return Err("mode 'master' requires a prompt source ('prompt' or 'prompt_file')".to_string());
    }
    if task.steps.is_some() || task.handler.as_ref().is_some_and(|value| !value.is_empty()) {
      return Err("mode 'master' forbids 'steps' and 'handler'; the manager wake is a prompt".to_string());
    }
  }
  if task.steps.as_ref().is_some_and(Vec::is_empty) {
    return Err("steps must be a non-empty list".to_string());
  }
  if let Some(steps) = task.steps.as_ref() {
    for (index, step) in steps.iter().enumerate() {
      if step.name.is_empty() {
        return Err("step name must have at least 1 character".to_string());
      }
      if steps[..index].iter().any(|previous| previous.name == step.name) {
        return Err(format!("duplicate step name '{}'", step.name));
      }
      if step.prompt.as_deref().unwrap_or_default().is_empty() {
        return Err(format!(
          "step '{}' has no prompt body; the loader resolves each step's 'prompt_file' before validation",
          step.name
        ));
      }
      let Some(source_name) = step.distinct_backend_from.as_deref() else { continue };
      let source_position = steps.iter().position(|candidate| candidate.name == source_name);
      let Some(source_position) = source_position else {
        return Err(format!(
          "step '{}' declares distinct_backend_from '{}', which is not a step of this task",
          step.name, source_name
        ));
      };
      if source_position >= index {
        return Err(format!(
          "step '{}' declares distinct_backend_from '{}', which must name a step listed earlier in the same task",
          step.name, source_name
        ));
      }
    }
  }
  Ok(())
}

/// The backend a scheduled task fires on: its own `backend` (an options id or a pool id, returned as written), else the
/// `backends.calls.new_session` target, else the first configured option.
pub(crate) fn effective_backend(task: &ScheduledTaskConfig, cfg: &CharlieBotConfig) -> anyhow::Result<String> {
  if let Some(backend) = task.backend.as_deref().filter(|backend| !backend.is_empty()) {
    cfg.backends.check_backend_id(backend).map_err(anyhow::Error::msg)?;
    return Ok(backend.to_string());
  }
  let backend = cfg
    .backends
    .new_session_backend()
    .ok_or_else(|| anyhow::anyhow!("scheduled task backend resolution requires a configured backends.options entry"))?;
  cfg.backends.check_backend_id(&backend).map_err(anyhow::Error::msg)?;
  Ok(backend)
}

fn default_timezone() -> String {
  DEFAULT_TIMEZONE.to_string()
}

fn default_true() -> bool {
  true
}

const CREATE_FIELDS: &[Field] = &[
  Field::required("name", FieldType::String),
  Field::required("cron", FieldType::String),
  Field::optional("prompt_file", FieldType::NullableString),
  Field::optional("repo", FieldType::NullableString),
  Field::optional("backend", FieldType::NullableString),
  Field::optional("timezone", FieldType::String),
  Field::optional("enabled", FieldType::Boolean),
  Field::optional("project", FieldType::NullableString),
  Field::optional("allow_failure", FieldType::Boolean),
  Field::optional("session_id", FieldType::NullableString),
  Field::optional("mode", FieldType::NullableLiteral(&["master", "worker"])),
];

/// The HTTP create-field model; unknown extra keys are ignored.
pub const CREATE_MODEL: BodyModel = BodyModel::required(CREATE_FIELDS);

/// The patch body model; omitted and nullable values remain distinguishable in the validated object.
pub const UPDATE_MODEL: BodyModel = BodyModel::required(&[
  Field::optional("cron", FieldType::NullableString),
  Field::optional("prompt_file", FieldType::NullableString),
  Field::optional("repo", FieldType::NullableString),
  Field::optional("backend", FieldType::NullableString),
  Field::optional("timezone", FieldType::NullableString),
  Field::optional("project", FieldType::NullableString),
  Field::optional("enabled", FieldType::Boolean),
  Field::optional("project", FieldType::NullableString),
  Field::optional("allow_failure", FieldType::Boolean),
  Field::optional("session_id", FieldType::NullableString),
]);

/// The task file model for one resolved mapping, named by the file stem and checked by [`validate_task`].
pub fn task_from_value(name: &str, value: serde_json::Value) -> Result<ScheduledTaskConfig, String> {
  let mut object = value.as_object().cloned().ok_or_else(|| "cron config must be a mapping".to_string())?;
  object.insert("name".to_string(), serde_json::Value::String(name.to_string()));
  let body = serde_json::Value::Object(object);
  let parsed = serde_json::from_value::<ScheduledTaskConfig>(body).map_err(|error| error.to_string())?;
  validate_task(&parsed)?;
  Ok(parsed)
}
