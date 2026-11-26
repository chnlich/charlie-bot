//! Scheduled-task request and file models.

use charliebot_compat::request_body::{BodyModel, Field, FieldType};
use charliebot_core::config::{CharlieBotConfig, HOST_AUTH_SECTION_KEY, HostAuthConfig};
use serde::{Deserialize, Serialize};

/// The default time zone of a scheduled task and of a create request: `charliebot_core::pacific::ZONE`, the house zone.
pub const DEFAULT_TIMEZONE: &str = charliebot_core::pacific::ZONE;

/// A named step in an ordered scheduled-worker chain.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StepConfig {
  /// The step's name, unique within its task. A `distinct_backend_from` names an earlier step by this name. A chain
  /// that stops reports the step by this name.
  pub name: String,
  /// The file with the step's prompt. A relative path resolves against the repository root, and `~` expands to the home
  /// directory. The loader reads the file on every load into `prompt`.
  #[serde(default)]
  pub prompt_file: Option<String>,
  /// The step's prompt text, which the loader fills from `prompt_file`. Change that file, not this key. A host file
  /// cannot hold `prompt` inline.
  #[serde(default)]
  pub prompt: Option<String>,
  /// The backend id that runs this step: a `backends.options` id or a `backends.pools` id. The startup check and
  /// `--check-config` refuse any other id. Absent or empty, the step runs on the bound session's backend.
  #[serde(default)]
  pub backend: Option<String>,
  /// The name of an earlier step in the same task. Both steps must name a backend. When this step's backend is a pool,
  /// the pool picks a member that the named step did not run on. When both backends are single backends of the same
  /// type and model, the chain stops before this step runs.
  #[serde(default)]
  pub distinct_backend_from: Option<String>,
}

/// The input that a fire of a prompt task gives its bound session. `Master` sends the task's own prompt. `Worker`, and
/// an absent `mode`, send the task's name and its category template, and the session decides the round.
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
  /// The task's name is its cron file's name without the extension. The file must not carry a `name` key; the loader
  /// refuses one.
  pub name: String,
  /// The schedule as five space-separated fields: minute, hour, day of month, month and day of week. The task fires at
  /// each minute that the fields match, read in `timezone`.
  pub cron: String,
  /// The file with the task's prompt. A relative path resolves against the repository root, and `~` expands to the home
  /// directory. The loader reads the file on every load into `prompt`.
  #[serde(default)]
  pub prompt_file: Option<String>,
  /// The git repository that each run of the task works in, as a worktree of it. A `worker` task's session gets the
  /// round template for a repository when this key is set, and the repo-less template when it is absent.
  #[serde(default)]
  pub repo: Option<String>,
  /// The time zone that `cron` is read in: an IANA name such as `America/New_York`, or `local` for the host's zone,
  /// read from `/etc/localtime` when the file loads. Absent, it is `DEFAULT_TIMEZONE`.
  #[serde(default = "default_timezone")]
  pub timezone: String,
  /// Set `false` to stop the task's fires. The task stays in the cron list with its schedule. Absent, the task is
  /// enabled.
  #[serde(default = "default_true")]
  pub enabled: bool,
  /// Set `true` to mark a failed last run as needing review. The task's session row in the sidebar then shows that run
  /// in amber with "review needed", instead of red. The fire and its failure record do not change. Absent, `false`.
  #[serde(default)]
  pub allow_failure: bool,
  /// The session that the task's fires reach. Absent or empty, the scheduler creates a session for the task on a later
  /// tick and writes its id into this file. To bind the task to a session that exists, write that session's id here.
  #[serde(default)]
  pub session_id: Option<String>,
  /// `master` sends the task's own prompt to its session. `worker` sends the task's name and the category template, and
  /// the session decides the round. Absent, the task works as `worker`. `master` forbids `steps` and `handler`.
  #[serde(default)]
  pub mode: Option<TaskMode>,
  /// The prompt text. The loader fills it from the file that `prompt_file` names, so change that file, not this key. A
  /// host file cannot hold `prompt` inline.
  #[serde(default)]
  pub prompt: Option<String>,
  /// The name of a scheduled handler that a feature registers, such as `disk_sweep`. Each fire runs the handler in
  /// place of a prompt. A name that no feature registers fails each fire with "Unknown handler".
  #[serde(default)]
  pub handler: Option<String>,
  /// An ordered list of steps. The steps run one after another in list order, each with its own prompt. The list must
  /// not be empty, and a task with steps has no `prompt` or `handler`.
  #[serde(default)]
  pub steps: Option<Vec<StepConfig>>,
  /// The host whose ssh login a round of this task runs on: the round starts through the improve launcher on this host,
  /// with no worktree on this host. Absent, the round runs here.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub improve_host: Option<String>,
}

impl ScheduledTaskConfig {
  /// Whether the task's one source is a prompt: a prompt task's fire reaches its bound session as a trigger input.
  pub(crate) fn is_prompt(&self) -> bool {
    self.prompt.as_deref().is_some_and(|prompt| !prompt.is_empty())
  }

  /// Whether the task's runs take the bound session's backend. A handler task takes none, so its fires run whatever
  /// the session names.
  pub(crate) fn uses_session_backend(&self) -> bool {
    self.handler.as_deref().is_none_or(str::is_empty)
  }
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
      if step.backend.as_deref().is_none_or(str::is_empty) {
        return Err(format!("step '{}' declares distinct_backend_from, so it must name its own backend", step.name));
      }
      if steps[source_position].backend.as_deref().is_none_or(str::is_empty) {
        return Err(format!(
          "step '{}' is the distinct_backend_from source of step '{}', so it must name its own backend",
          source_name, step.name
        ));
      }
    }
  }
  Ok(())
}

/// The hosts that `host_auth.browser_logins` names, in config order. A config without the host-auth section names none.
pub fn browser_login_hosts(cfg: &CharlieBotConfig) -> Vec<String> {
  let Some(value) = cfg.section_value(HOST_AUTH_SECTION_KEY) else { return Vec::new() };
  value
    .downcast_ref::<HostAuthConfig>()
    .expect("host_auth is parsed as HostAuthConfig")
    .browser_logins
    .iter()
    .filter_map(|login| login.host.clone())
    .collect()
}

/// The load-time check of a task's `improve_host`: the value names a host in `login_hosts`, and the key sits only on a
/// task with a prompt that is not `mode: master`, steps or a handler. Those tasks start no round the launcher carries.
pub fn validate_improve_host(task: &ScheduledTaskConfig, login_hosts: &[String]) -> Result<(), String> {
  let Some(host) = task.improve_host.as_deref() else {
    return Ok(());
  };
  let has_prompt = task.prompt.as_deref().is_some_and(|prompt| !prompt.is_empty());
  if !has_prompt || task.mode == Some(TaskMode::Master) || task.steps.is_some() || task.handler.is_some() {
    return Err("improve_host needs a task with a 'prompt' that is not mode 'master', steps or a handler".to_string());
  }
  if !login_hosts.iter().any(|login_host| login_host == host) {
    return Err(format!("improve_host '{host}' names no host in host_auth.browser_logins"));
  }
  Ok(())
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
  Field::optional("timezone", FieldType::String),
  Field::optional("enabled", FieldType::Boolean),
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
  Field::optional("timezone", FieldType::NullableString),
  Field::optional("enabled", FieldType::Boolean),
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

#[cfg(test)]
mod tests {
  use super::*;

  /// A task in `mode: master` with no `session_id` is valid: the scheduler's
  /// auto-bind creates the task's session on a later tick and writes the id
  /// back into the host file, and an agent caller of the create API binds its
  /// own new session at create time.
  #[test]
  fn validation_accepts_mode_without_session_id() {
    let task = ScheduledTaskConfig {
      name: "proactive-agent".to_string(),
      cron: "0 7 * * *".to_string(),
      prompt_file: Some("prompts/cron/proactive_agent/prompt.md".to_string()),
      repo: None,
      timezone: DEFAULT_TIMEZONE.to_string(),
      enabled: true,
      allow_failure: false,
      session_id: None,
      mode: Some(TaskMode::Master),
      prompt: Some("Synthetic prompt\n".to_string()),
      handler: None,
      steps: None,
      improve_host: None,
    };
    validate_task(&task).expect("master mode without session_id validates");

    let worker_task = ScheduledTaskConfig { mode: Some(TaskMode::Worker), ..task.clone() };
    validate_task(&worker_task).expect("worker mode without session_id validates");

    let empty_session_task = ScheduledTaskConfig { session_id: Some(String::new()), ..task };
    validate_task(&empty_session_task).expect("an empty session_id counts as unbound");
  }
}
