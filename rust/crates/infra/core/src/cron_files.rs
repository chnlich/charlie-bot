//! The cron task files under `cron.d`: which directory entries count as one, and how one file decodes into the mapping
//! its readers take. The scheduler loader and the improve hold read both use these, so one rule decides both.

use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

/// Whether a `cron.d` directory entry is a cron task file: a regular file whose name ends in `.yaml` and does not start
/// with a dot. Metadata errors other than a vanished entry are returned so readers can fail closed.
pub fn is_task_file(path: &Path) -> io::Result<bool> {
  let file_name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
  if !file_name.ends_with(".yaml") || file_name.starts_with('.') {
    return Ok(false);
  }
  match path.metadata() {
    Ok(metadata) => Ok(metadata.is_file()),
    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
    Err(error) => Err(error),
  }
}

/// One cron task file, decoded as the JSON object its readers and writers take.
pub type TaskFile = Map<String, Value>;

/// Why one cron task file could not be read as a mapping.
#[derive(Debug)]
pub enum TaskFileReadError {
  /// The file could not be opened or read.
  Read { path: PathBuf, error: std::io::Error },
  /// The file's text is not YAML.
  Decode(serde_yaml::Error),
  /// The YAML document cannot become a JSON object, such as one with a non-string key.
  Convert(serde_json::Error),
  /// The YAML document is not a mapping.
  NotMapping,
}

impl TaskFileReadError {
  /// The message with no file path, as a reader that already names the file uses.
  pub fn message_without_path(&self) -> String {
    match self {
      Self::Read { error, .. } => error.to_string(),
      Self::Decode(error) => error.to_string(),
      Self::Convert(error) => error.to_string(),
      Self::NotMapping => "cron config must be a mapping".to_string(),
    }
  }
}

impl std::fmt::Display for TaskFileReadError {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Read { path, error } => write!(formatter, "{}: {error}", path.display()),
      Self::Decode(error) => error.fmt(formatter),
      Self::Convert(error) => error.fmt(formatter),
      Self::NotMapping => formatter.write_str("cron config must be a mapping"),
    }
  }
}

impl std::error::Error for TaskFileReadError {
  fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
    match self {
      Self::Read { error, .. } => Some(error),
      Self::Decode(error) => Some(error),
      Self::Convert(error) => Some(error),
      Self::NotMapping => None,
    }
  }
}

/// One cron task file, read as YAML and required to be a mapping.
pub fn read_task_file(path: &Path) -> Result<TaskFile, TaskFileReadError> {
  let raw =
    std::fs::read_to_string(path).map_err(|error| TaskFileReadError::Read { path: path.to_path_buf(), error })?;
  let yaml = serde_yaml::from_str::<serde_yaml::Value>(&raw).map_err(TaskFileReadError::Decode)?;
  match serde_json::to_value(yaml).map_err(TaskFileReadError::Convert)? {
    Value::Object(mapping) => Ok(mapping),
    _ => Err(TaskFileReadError::NotMapping),
  }
}
