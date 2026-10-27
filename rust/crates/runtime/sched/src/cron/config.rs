//! Scheduled-task request and file models (`src/features/cron/config.py`).

use charliebot_compat::fastapi_body::{BodyModel, Field, FieldType};
use serde::{Deserialize, Serialize};

/// The Python default for cron task and create-request time zones.
pub const DEFAULT_TIMEZONE: &str = "America/Los_Angeles";

const STEP_FIELDS: &[Field] = &[
  Field::required("name", FieldType::String).min_length(1),
  Field::optional("prompt_file", FieldType::NullableString),
  Field::optional("prompt", FieldType::NullableString),
  Field::optional("backend", FieldType::NullableString),
  Field::optional("distinct_backend_from", FieldType::NullableString),
];

/// The nested step body model used by FastAPI request validation.
pub const STEP_MODEL: BodyModel = BodyModel::optional(STEP_FIELDS).forbid_extra();

const TASK_FIELDS: &[Field] = &[
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
  Field::optional("prompt", FieldType::NullableString),
  Field::optional("handler", FieldType::NullableString),
  Field::optional("steps", FieldType::NullableModelList { class_name: "StepConfig", model: &STEP_MODEL }),
];

/// The YAML task model: forbids unknown fields and applies the model-level source and mode rules.
pub const TASK_MODEL: BodyModel = BodyModel::optional(TASK_FIELDS).forbid_extra().checked(check_task_model);

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

/// Shared fields of the API create model and the strict cron file model.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ScheduledTaskFields {
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
  pub mode: Option<String>,
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
  pub mode: Option<String>,
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

/// Validate the source, mode and sequence invariants shared with the YAML loader.
pub fn validate_task(task: &ScheduledTaskConfig) -> Result<(), String> {
  if task.mode.as_deref().is_some_and(|mode| !matches!(mode, "master" | "worker")) {
    return Err("mode must be 'master' or 'worker'".to_string());
  }
  let source_count = usize::from(task.prompt.as_ref().is_some_and(|value| !value.is_empty()))
    + usize::from(task.steps.as_ref().is_some_and(|value| !value.is_empty()))
    + usize::from(task.handler.as_ref().is_some_and(|value| !value.is_empty()));
  if source_count != 1 {
    return Err("task must have exactly one of 'prompt', 'prompt_file', 'steps', or 'handler'".to_string());
  }
  if task.mode.is_some() && task.session_id.as_deref().unwrap_or_default().is_empty() {
    return Err("'mode' requires 'session_id' (mode selects how a bound task fires)".to_string());
  }
  if task.mode.as_deref() == Some("master") {
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
      let own = step.backend.as_deref().or(task.backend.as_deref());
      let prior = steps[source_position].backend.as_deref().or(task.backend.as_deref());
      if own.is_some() && own == prior {
        return Err(format!(
          "step '{}' and its distinct_backend_from source '{}' both declare backend '{}'; the two steps must name different backends",
          step.name,
          steps[source_position].name,
          own.unwrap_or_default()
        ));
      }
    }
  }
  Ok(())
}

fn check_task_model(values: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
  serde_json::from_value::<ScheduledTaskConfig>(serde_json::Value::Object(values.clone()))
    .ok()
    .and_then(|task| validate_task(&task).err())
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

/// The HTTP create-field model, with Pydantic's default extra-key ignore behavior.
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

/// Build a task file model from a resolved mapping and file stem.
pub fn task_from_value(name: &str, value: serde_json::Value) -> Result<ScheduledTaskConfig, String> {
  let mut object = value.as_object().cloned().ok_or_else(|| "cron config must be a mapping".to_string())?;
  object.insert("name".to_string(), serde_json::Value::String(name.to_string()));
  let body = serde_json::Value::Object(object);
  let parsed = serde_json::from_value::<ScheduledTaskConfig>(body).map_err(|error| error.to_string())?;
  validate_task(&parsed)?;
  Ok(parsed)
}
