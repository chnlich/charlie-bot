//! `cron.d` file loading, prompt pointers and backend startup validation. The broken-file alert is in `alert`.

use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use charliebot_core::config::{CONFIG_D_DIR_NAME, CRON_D_DIR_NAME, CharlieBotConfig};
use charliebot_core::repo_root::RepoRoot;
use charliebot_registry::Registry;
use charliebot_registry::context::FeatureContext;
use serde_json::{Map, Value};

use crate::alert::fire_error_alert;
use crate::config::{ScheduledTaskConfig, ScheduledTaskError};

/// One sorted read of every visible cron file.
#[derive(Clone, Debug, Default)]
pub struct CronSnapshot {
  /// Valid task configs sorted by filename.
  pub tasks: Vec<ScheduledTaskConfig>,
  /// Broken file records sorted by filename.
  pub errors: Vec<ScheduledTaskError>,
  /// Unique generation of the YAML and prompt-file fingerprint that produced this snapshot.
  pub fingerprint: u64,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct SnapshotKey {
  directory: PathBuf,
  repo: PathBuf,
  login_hosts: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
struct SnapshotFingerprint {
  cron_files: Vec<(String, f64)>,
  prompt_files: Option<Vec<(PathBuf, f64)>>,
}

#[derive(Clone)]
struct CachedSnapshot {
  snapshot: CronSnapshot,
  fingerprint: SnapshotFingerprint,
  prompt_paths: Vec<PathBuf>,
}

static SNAPSHOTS: OnceLock<Mutex<HashMap<SnapshotKey, CachedSnapshot>>> = OnceLock::new();
static NEXT_SNAPSHOT_GENERATION: AtomicU64 = AtomicU64::new(1);
#[cfg(test)]
static CRON_FILE_READS: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();
#[cfg(test)]
static PROMPT_FILE_READS: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();

/// The cron task directory under one profile home.
pub fn cron_dir_for_home(home: &Path) -> PathBuf {
  home.join(CONFIG_D_DIR_NAME).join(CRON_D_DIR_NAME)
}

/// The cron task directory this profile owns.
pub fn cron_dir(cfg: &CharlieBotConfig) -> PathBuf {
  cron_dir_for_home(&cfg.charliebot_home)
}

/// One task's config file path under a profile home.
pub fn cron_path_for_home(home: &Path, name: &str) -> PathBuf {
  cron_dir_for_home(home).join(format!("{name}.yaml"))
}

/// One task's config file path in this profile.
pub fn cron_path(cfg: &CharlieBotConfig, name: &str) -> PathBuf {
  cron_path_for_home(&cfg.charliebot_home, name)
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

/// Why one cron task file could not be written.
#[derive(Debug)]
pub enum TaskFileWriteError {
  /// The value could not be encoded as YAML.
  Encode(serde_yaml::Error),
  /// The encoded text could not be written to the file.
  Write(std::io::Error),
}

impl std::fmt::Display for TaskFileWriteError {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Encode(error) => error.fmt(formatter),
      Self::Write(error) => error.fmt(formatter),
    }
  }
}

impl std::error::Error for TaskFileWriteError {
  fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
    match self {
      Self::Encode(error) => Some(error),
      Self::Write(error) => Some(error),
    }
  }
}

/// One cron task value, encoded as YAML and written to the file.
pub fn write_task_file(path: &Path, value: &Value) -> Result<(), TaskFileWriteError> {
  let contents = crate::yaml::dump(value).map_err(TaskFileWriteError::Encode)?;
  std::fs::write(path, contents).map_err(TaskFileWriteError::Write)
}

/// Why updating one key in a cron task file failed.
#[derive(Debug)]
pub enum TaskFileKeyError {
  /// The existing file could not be read or decoded as a mapping.
  Read { path: PathBuf, error: TaskFileReadError },
  /// The updated mapping could not be encoded or written.
  Write { path: PathBuf, error: TaskFileWriteError },
}

impl std::fmt::Display for TaskFileKeyError {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Read { path, error } => write!(formatter, "{}: {}", path.display(), error.message_without_path()),
      Self::Write { path, error } => write!(formatter, "{}: {error}", path.display()),
    }
  }
}

impl std::error::Error for TaskFileKeyError {
  fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
    match self {
      Self::Read { error, .. } => Some(error),
      Self::Write { error, .. } => Some(error),
    }
  }
}

/// Set one top-level key in a cron task's YAML file on disk while keeping all other keys.
pub fn write_task_key_at(path: &Path, key: &str, value: Value) -> Result<(), TaskFileKeyError> {
  let mut data = read_task_file(path).map_err(|error| TaskFileKeyError::Read { path: path.to_path_buf(), error })?;
  data.insert(key.to_string(), value);
  write_task_file(path, &Value::Object(data))
    .map_err(|error| TaskFileKeyError::Write { path: path.to_path_buf(), error })
}

/// Set one top-level key in the named cron task's YAML file under `home` while keeping all other keys.
pub fn write_task_key(home: &Path, name: &str, key: &str, value: Value) -> Result<(), TaskFileKeyError> {
  write_task_key_at(&cron_path_for_home(home, name), key, value)
}

/// The check a task name passes before it forms a host file path.
pub fn valid_name(name: &str) -> bool {
  let mut chars = name.chars();
  chars.next().is_some_and(|first| first.is_ascii_alphanumeric())
    && chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
}

/// The YAML and prompt snapshot, unchanged until one of its file mtimes moves. A reload that finds broken files
/// raises their alert.
pub fn load_snapshot(features: &Arc<FeatureContext>, cfg: &CharlieBotConfig) -> anyhow::Result<CronSnapshot> {
  let (snapshot, reloaded) = read_snapshot(features, cfg)?;
  if reloaded {
    fire_error_alert(features, cfg, &snapshot.errors);
  }
  Ok(snapshot)
}

/// The snapshot of the cron files, and whether it was reloaded, without raising the alert: a read that is not the
/// scheduler's tick takes its tasks from here.
pub(crate) fn read_snapshot(features: &FeatureContext, cfg: &CharlieBotConfig) -> anyhow::Result<(CronSnapshot, bool)> {
  let repo = repo_root(features);
  load_snapshot_for_paths(&cron_dir(cfg), repo.path(), &crate::config::browser_login_hosts(cfg))
}

/// One shared cron snapshot, reread when a cron YAML appears, disappears or changes mtime, or a prompt file
/// changes mtime or goes missing.
pub(crate) fn load_snapshot_for_paths(
  directory: &Path,
  repo: &Path,
  login_hosts: &[String],
) -> anyhow::Result<(CronSnapshot, bool)> {
  let key =
    SnapshotKey { directory: directory.to_path_buf(), repo: repo.to_path_buf(), login_hosts: login_hosts.to_vec() };
  let snapshots = SNAPSHOTS.get_or_init(|| Mutex::new(HashMap::new()));
  let mut snapshots = snapshots.lock().expect("cron snapshot mutex is not poisoned");
  if let Some(cached) = snapshots.get(&key) {
    let fingerprint = snapshot_fingerprint(directory, &cached.prompt_paths)?;
    if fingerprint.prompt_files.is_some() && fingerprint == cached.fingerprint {
      return Ok((cached.snapshot.clone(), false));
    }
  }
  let (mut snapshot, prompt_paths) = reload_snapshot(directory, repo, login_hosts)?;
  let fingerprint = snapshot_fingerprint(directory, &prompt_paths)?;
  snapshot.fingerprint = NEXT_SNAPSHOT_GENERATION.fetch_add(1, Ordering::Relaxed);
  snapshots.insert(key, CachedSnapshot { snapshot: snapshot.clone(), fingerprint, prompt_paths });
  Ok((snapshot, true))
}

fn reload_snapshot(
  directory: &Path,
  repo: &Path,
  login_hosts: &[String],
) -> anyhow::Result<(CronSnapshot, Vec<PathBuf>)> {
  let entries = match std::fs::read_dir(directory) {
    Ok(entries) => entries,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
      return Ok((CronSnapshot::default(), Vec::new()));
    }
    Err(error) => return Err(error.into()),
  };
  let mut paths = entries.map(|entry| entry.map(|entry| entry.path())).collect::<Result<Vec<_>, _>>()?;
  paths.sort();
  let mut snapshot = CronSnapshot::default();
  let mut prompt_paths = Vec::new();
  for path in paths {
    let file_name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
    if !path.is_file() || !file_name.ends_with(".yaml") || file_name.starts_with('.') {
      continue;
    }
    let name = path.file_stem().and_then(|name| name.to_str()).unwrap_or_default().to_string();
    if !valid_name(&name) {
      snapshot.errors.push(ScheduledTaskError {
        name,
        path: path.display().to_string(),
        error: "file name is not a valid cron task name".to_string(),
        enabled: read_enabled(&path),
      });
      continue;
    }
    match load_file_with_prompts(&path, &name, repo, login_hosts) {
      Ok((task, paths)) => {
        prompt_paths.extend(paths);
        snapshot.tasks.push(task);
      }
      Err((error, paths)) => {
        prompt_paths.extend(paths);
        tracing::error!(name, path = %path.display(), error = %error, "cron_task_load_failed");
        snapshot.errors.push(ScheduledTaskError {
          name,
          path: path.display().to_string(),
          error,
          enabled: read_enabled(&path),
        });
      }
    }
  }
  snapshot.tasks.sort_by(|left, right| left.name.cmp(&right.name));
  snapshot.errors.sort_by(|left, right| left.name.cmp(&right.name));
  prompt_paths.sort();
  prompt_paths.dedup();
  Ok((snapshot, prompt_paths))
}

/// One cron file's task, loaded and validated with its prompt pointer strings preserved.
pub fn load_file(path: &Path, name: &str, repo: &Path, login_hosts: &[String]) -> Result<ScheduledTaskConfig, String> {
  load_file_with_prompts(path, name, repo, login_hosts).map(|(task, _)| task).map_err(|(error, _)| error)
}

fn load_file_with_prompts(
  path: &Path,
  name: &str,
  repo: &Path,
  login_hosts: &[String],
) -> Result<(ScheduledTaskConfig, Vec<PathBuf>), (String, Vec<PathBuf>)> {
  #[cfg(test)]
  if let Some(directory) = path.parent() {
    *CRON_FILE_READS
      .get_or_init(|| Mutex::new(HashMap::new()))
      .lock()
      .expect("cron file read counter mutex is not poisoned")
      .entry(directory.to_path_buf())
      .or_default() += 1;
  }
  let body = read_task_file(path).map_err(|error| (error.to_string(), Vec::new()))?;
  let body = Value::Object(body);
  let prompts = prompt_paths(&body, repo);
  validate_body(name, body, repo, login_hosts).map(|task| (task, prompts.clone())).map_err(|error| (error, prompts))
}

fn prompt_paths(body: &Value, repo: &Path) -> Vec<PathBuf> {
  let entries = std::iter::once(body).chain(body.get("steps").and_then(Value::as_array).into_iter().flatten());
  entries
    .filter_map(|entry| entry.get("prompt_file").and_then(Value::as_str))
    .filter(|pointer| !pointer.is_empty())
    .filter_map(|pointer| resolve_path(pointer, repo).ok())
    .collect()
}

fn snapshot_fingerprint(directory: &Path, prompt_paths: &[PathBuf]) -> anyhow::Result<SnapshotFingerprint> {
  let mut cron_files = Vec::new();
  match std::fs::read_dir(directory) {
    Ok(entries) => {
      for entry in entries {
        let path = entry?.path();
        let name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
        if !path.is_file() || !name.ends_with(".yaml") || name.starts_with('.') {
          continue;
        }
        let modified =
          path.metadata().map(|metadata| modified_seconds(metadata.mtime(), metadata.mtime_nsec())).unwrap_or(0.0);
        cron_files.push((name.to_string(), modified));
      }
    }
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
    Err(error) => return Err(error.into()),
  }
  cron_files.sort_by(|left, right| left.0.cmp(&right.0));
  let mut prompt_files = Vec::new();
  for path in prompt_paths {
    let metadata = match path.metadata() {
      Ok(metadata) => metadata,
      Err(_) => return Ok(SnapshotFingerprint { cron_files, prompt_files: None }),
    };
    prompt_files.push((path.clone(), modified_seconds(metadata.mtime(), metadata.mtime_nsec())));
  }
  prompt_files.sort_by(|left, right| left.0.cmp(&right.0));
  Ok(SnapshotFingerprint { cron_files, prompt_files: Some(prompt_files) })
}

fn modified_seconds(seconds: i64, nanoseconds: i64) -> f64 {
  seconds as f64 + nanoseconds as f64 / 1_000_000_000.0
}

#[cfg(test)]
fn cron_file_read_count(directory: &Path) -> usize {
  CRON_FILE_READS
    .get_or_init(|| Mutex::new(HashMap::new()))
    .lock()
    .expect("cron file read counter mutex is not poisoned")
    .get(directory)
    .copied()
    .unwrap_or_default()
}

#[cfg(test)]
fn prompt_file_read_count(path: &Path) -> usize {
  PROMPT_FILE_READS
    .get_or_init(|| Mutex::new(HashMap::new()))
    .lock()
    .expect("prompt file read counter mutex is not poisoned")
    .get(path)
    .copied()
    .unwrap_or_default()
}

/// The task one in-memory create or update body describes, resolved against the repo before it is persisted.
pub fn validate_body(
  name: &str,
  mut body: Value,
  repo: &Path,
  login_hosts: &[String],
) -> Result<ScheduledTaskConfig, String> {
  let object = body.as_object_mut().ok_or_else(|| "cron config must be a mapping".to_string())?;
  if object.contains_key("name") {
    return Err("the body must not carry a 'name' key; the file name is the job name".to_string());
  }
  if object.contains_key("prompt") {
    return Err("a cron.d host file must not carry an inline 'prompt'; it holds the path to the prompt source under 'prompt_file', and the loader reads that file on every load".to_string());
  }
  if object.get("session_id").and_then(Value::as_str).is_some_and(str::is_empty) {
    object.remove("session_id");
  }
  resolve_pointer(object, "prompt_file", repo, name)?;
  if let Some(steps) = object.get_mut("steps").and_then(Value::as_array_mut) {
    for step in steps {
      if let Some(step) = step.as_object_mut() {
        if step.contains_key("prompt") {
          return Err("a cron.d host file must not carry an inline 'prompt'; a step holds the path to its prompt source under 'prompt_file', and the loader reads that file on every load".to_string());
        }
        let step_name = step.get("name").and_then(Value::as_str).unwrap_or(name).to_string();
        resolve_pointer(step, "prompt_file", repo, &step_name)?;
      }
    }
  }
  if body.get("timezone").and_then(Value::as_str) == Some("local") {
    body["timezone"] = Value::String(detect_local_timezone());
  }
  if let Some(repo) = body.get("repo").and_then(Value::as_str) {
    body["repo"] = Value::String(expand_home(repo)?);
  }
  let task = crate::config::task_from_value(name, body)?;
  crate::config::validate_improve_host(&task, login_hosts)?;
  Ok(task)
}

fn resolve_pointer(
  entry: &mut serde_json::Map<String, Value>,
  key: &str,
  repo: &Path,
  name: &str,
) -> Result<(), String> {
  let Some(pointer) = entry.get(key) else { return Ok(()) };
  let Some(pointer) = pointer.as_str() else { return Ok(()) };
  if pointer.is_empty() {
    return Ok(());
  }
  let path = resolve_path(pointer, repo)?;
  #[cfg(test)]
  {
    *PROMPT_FILE_READS
      .get_or_init(|| Mutex::new(HashMap::new()))
      .lock()
      .expect("prompt file read counter mutex is not poisoned")
      .entry(path.clone())
      .or_default() += 1;
  }
  let prompt = std::fs::read_to_string(&path)
    .map_err(|error| format!("cron entry {name:?} prompt_file unreadable: {} ({error})", path.display()))?;
  entry.insert("prompt".to_string(), Value::String(prompt));
  Ok(())
}

/// The file a `prompt_file` pointer names: `~` expands to the home, and a relative pointer resolves against `repo`.
pub(crate) fn resolve_path(pointer: &str, repo: &Path) -> Result<PathBuf, String> {
  if pointer == "~" || pointer.starts_with("~/") {
    return Ok(PathBuf::from(expand_home(pointer)?));
  }
  let path = PathBuf::from(pointer);
  if path.is_absolute() { Ok(path) } else { Ok(repo.join(path)) }
}

fn expand_home(path: &str) -> Result<String, String> {
  if path == "~" || path.starts_with("~/") {
    let home = std::env::var_os("HOME").ok_or_else(|| "HOME is not set while expanding a cron path".to_string())?;
    let suffix = path.strip_prefix('~').expect("the path starts with a tilde");
    return Ok(PathBuf::from(home).join(suffix.trim_start_matches('/')).display().to_string());
  }
  Ok(path.to_string())
}

fn detect_local_timezone() -> String {
  let result = (|| {
    let target = std::fs::canonicalize("/etc/localtime").map_err(|error| error.to_string())?;
    let text = target.display().to_string();
    let name = text
      .rsplit_once("/zoneinfo/")
      .map(|(_, name)| name)
      .ok_or_else(|| format!("no '/zoneinfo/' segment in {text:?}"))?;
    name.parse::<chrono_tz::Tz>().map_err(|error| error.to_string())?;
    Ok::<String, String>(name.to_string())
  })();
  result.unwrap_or_else(|error| {
    tracing::warn!(error, fallback = "UTC", "local_timezone_resolve_failed");
    "UTC".to_string()
  })
}

fn read_enabled(path: &Path) -> Option<bool> {
  match read_task_file(path) {
    Ok(mapping) => mapping.get("enabled").and_then(Value::as_bool),
    Err(TaskFileReadError::Read { error, .. }) => {
      tracing::debug!(path = %path.display(), error = %error, "cron_file_enabled_unreadable");
      None
    }
    Err(TaskFileReadError::Decode(error)) => {
      tracing::debug!(path = %path.display(), error = %error, "cron_file_enabled_unreadable");
      None
    }
    Err(TaskFileReadError::Convert(_) | TaskFileReadError::NotMapping) => None,
  }
}

fn repo_root(features: &FeatureContext) -> Arc<RepoRoot> {
  features.startup.require::<RepoRoot>().expect("the server inserts the checkout before registering request handlers")
}

/// The cron-file startup check, run before the server serves and by `--check-config`. Every cron file goes through the
/// loader the server's snapshot uses, so a file the server refuses fails here, named by its path and its error. A
/// valid task's step backend must name a backends.options id or a backends.pools id. A distinct-step conflict is
/// checked when the firing reaches that step, so one task cannot prevent the server from starting.
pub fn backend_refs_check(_registry: &Registry, cfg: &CharlieBotConfig) -> Result<(), String> {
  let repo = std::env::var_os(charliebot_core::repo_root::REPO_ROOT_ENV)
    .map(PathBuf::from)
    .ok_or_else(|| format!("{} is not set", charliebot_core::repo_root::REPO_ROOT_ENV))?;
  cron_files_problems(cfg, &repo)
}

/// The problems of the cron files under `cfg`'s home, read for the repository at `repo`; `Ok` when there are none.
fn cron_files_problems(cfg: &CharlieBotConfig, repo: &Path) -> Result<(), String> {
  let login_hosts = crate::config::browser_login_hosts(cfg);
  let (snapshot, _) =
    load_snapshot_for_paths(&cron_dir(cfg), repo, &login_hosts).map_err(|error| format!("{error:#}"))?;
  let mut problems: Vec<String> =
    snapshot.errors.iter().map(|error| format!("{}: {}", error.path, error.error)).collect();
  for task in &snapshot.tasks {
    for step in task.steps.iter().flatten() {
      if let Some(backend) = step.backend.as_deref().filter(|backend| !backend.is_empty())
        && let Err(error) = cfg.backends.check_backend_id(backend)
      {
        problems.push(format!("{}: steps '{}' backend: {error}", cron_path(cfg, &task.name).display(), step.name));
      }
    }
  }
  if problems.is_empty() {
    return Ok(());
  }
  Err(format!(
    "cron task files must load, and backend references must name a backends.options id or a backends.pools id \
     (id rule: charliebot_core::config::BackendsConfig):\n{}",
    problems.into_iter().map(|problem| format!("  {problem}")).collect::<Vec<_>>().join("\n")
  ))
}

#[cfg(test)]
mod tests {
  use std::path::Path;

  use charliebot_core::config::{CharlieBotConfig, HostAuthConfig};
  use serde_json::{Value, json};

  use super::{
    TaskFileKeyError, TaskFileReadError, TaskFileWriteError, cron_files_problems, load_snapshot, read_task_file,
    validate_body, write_task_file, write_task_key, write_task_key_at,
  };
  use crate::test_support::{feature_context, feature_context_with_login_hosts};

  fn write_task(cfg: &CharlieBotConfig, name: &str, body: &Value) {
    let directory = cfg.config_d_dir().join("cron.d");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join(format!("{name}.yaml")), crate::yaml::dump(body).unwrap()).unwrap();
  }

  fn write_prompt(directory: &Path, name: &str, body: &str) -> std::path::PathBuf {
    std::fs::create_dir_all(directory).unwrap();
    let path = directory.join(name);
    std::fs::write(&path, body).unwrap();
    path
  }

  #[test]
  fn task_file_read_reports_an_unreadable_file_a_decode_failure_a_conversion_failure_and_a_non_mapping() {
    let dir = tempfile::tempdir().unwrap();

    let missing = dir.path().join("missing.yaml");
    let error = read_task_file(&missing).unwrap_err();
    assert!(matches!(error, TaskFileReadError::Read { .. }));
    let io = std::fs::read_to_string(&missing).unwrap_err();
    // The loaders that name the file fold the path into the message; the API calls message_without_path.
    assert_eq!(error.to_string(), format!("{}: {io}", missing.display()));
    assert_eq!(error.message_without_path(), io.to_string());

    let invalid = dir.path().join("invalid.yaml");
    std::fs::write(&invalid, "cron: [unclosed\n").unwrap();
    let error = read_task_file(&invalid).unwrap_err();
    assert!(matches!(error, TaskFileReadError::Decode(_)));
    let yaml = serde_yaml::from_str::<serde_yaml::Value>("cron: [unclosed\n").unwrap_err();
    assert_eq!(error.to_string(), yaml.to_string());
    assert_eq!(error.message_without_path(), yaml.to_string());

    let sequence = dir.path().join("sequence.yaml");
    std::fs::write(&sequence, "- cron\n- 0 8 * * *\n").unwrap();
    let error = read_task_file(&sequence).unwrap_err();
    assert!(matches!(error, TaskFileReadError::NotMapping));
    assert_eq!(error.to_string(), "cron config must be a mapping");
    assert_eq!(error.message_without_path(), "cron config must be a mapping");

    let null_key = dir.path().join("null-key.yaml");
    std::fs::write(&null_key, "cron: 0 8 * * *\nnull: null key\n").unwrap();
    let error = read_task_file(&null_key).unwrap_err();
    assert!(matches!(error, TaskFileReadError::Convert(_)));
    let yaml = serde_yaml::from_str::<serde_yaml::Value>("cron: 0 8 * * *\nnull: null key\n").unwrap();
    let json = serde_json::to_value(yaml).unwrap_err();
    assert_eq!(error.to_string(), json.to_string());
    assert_eq!(error.message_without_path(), json.to_string());
  }

  #[test]
  fn task_file_write_keeps_the_loader_bytes_and_reports_a_write_failure_without_a_path() {
    let dir = tempfile::tempdir().unwrap();
    let body = json!({"cron":"0 8 * * *","prompt_file":"prompts/synthetic.md"});
    let path = dir.path().join("synthetic.yaml");
    write_task_file(&path, &body).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), crate::yaml::dump(&body).unwrap());

    let absent = dir.path().join("absent/synthetic.yaml");
    let error = write_task_file(&absent, &body).unwrap_err();
    assert!(matches!(error, TaskFileWriteError::Write(_)));
    let io = std::fs::write(&absent, "").unwrap_err();
    assert_eq!(error.to_string(), io.to_string());
    // The API, the seed, and the task-key writer fold the path into a failed write.
    assert_eq!(format!("{}: {error}", absent.display()), format!("{}: {io}", absent.display()));

    let encode =
      TaskFileWriteError::Encode(serde_yaml::from_str::<serde_yaml::Value>("cron: [unclosed\n").unwrap_err());
    let yaml = serde_yaml::from_str::<serde_yaml::Value>("cron: [unclosed\n").unwrap_err();
    assert_eq!(encode.to_string(), yaml.to_string());
  }

  #[test]
  fn task_key_writer_updates_one_key_in_place_and_folds_the_path_into_errors() {
    let dir = tempfile::tempdir().unwrap();
    let cron_dir = dir.path().join("config.d/cron.d");
    std::fs::create_dir_all(&cron_dir).unwrap();
    let path = cron_dir.join("synthetic.yaml");
    std::fs::write(&path, "cron: 0 8 * * *\nhandler: synthetic_handler\nenabled: true\n").unwrap();

    write_task_key(dir.path(), "synthetic", "enabled", json!(false)).unwrap();
    write_task_key(dir.path(), "synthetic", "session_id", json!("s-1")).unwrap();
    let updated = std::fs::read_to_string(&path).unwrap();
    assert_eq!(updated, "cron: 0 8 * * *\nhandler: synthetic_handler\nenabled: false\nsession_id: s-1\n");

    let missing = cron_dir.join("missing.yaml");
    let read_err = write_task_key_at(&missing, "enabled", json!(false)).unwrap_err();
    assert!(matches!(read_err, TaskFileKeyError::Read { .. }));
    assert!(read_err.to_string().starts_with(&format!("{}: ", missing.display())), "{read_err}");

    let invalid = cron_dir.join("invalid.yaml");
    std::fs::write(&invalid, "cron: [unclosed\n").unwrap();
    let decode_err = write_task_key_at(&invalid, "enabled", json!(false)).unwrap_err();
    assert!(matches!(decode_err, TaskFileKeyError::Read { .. }));
    assert!(decode_err.to_string().starts_with(&format!("{}: ", invalid.display())), "{decode_err}");
  }

  #[test]
  fn prompt_pointers_resolve_and_bad_task_sources_are_isolated() {
    let (home, features) = feature_context();
    let cfg = features.config.get().unwrap();
    let prompt = write_prompt(home.path(), "prompt.md", "Synthetic body\n");
    write_task(&cfg, "valid", &json!({"cron":"0 8 * * *", "prompt_file":prompt}));
    write_task(&cfg, "inline", &json!({"cron":"0 8 * * *", "prompt":"inline body"}));
    write_task(&cfg, "missing", &json!({"cron":"0 8 * * *"}));
    write_task(&cfg, "bad-mode", &json!({"cron":"0 8 * * *", "prompt_file":prompt, "mode":"boss"}));

    let snapshot = load_snapshot(&features, &cfg).unwrap();

    assert_eq!(snapshot.tasks.len(), 1);
    assert_eq!(snapshot.tasks[0].prompt.as_deref(), Some("Synthetic body\n"));
    assert_eq!(snapshot.tasks[0].prompt_file.as_deref(), Some(prompt.to_str().unwrap()));
    assert_eq!(
      snapshot.errors.iter().map(|error| error.name.as_str()).collect::<Vec<_>>(),
      ["bad-mode", "inline", "missing"]
    );
    let error_of =
      |name: &str| snapshot.errors.iter().find(|error| error.name == name).expect("the task's error").error.as_str();
    assert!(error_of("inline").contains("inline 'prompt'"));
    assert!(error_of("missing").contains("prompt_file"));
    assert!(error_of("bad-mode").contains("boss"), "the unknown mode is refused: {}", error_of("bad-mode"));
  }

  #[test]
  fn improve_host_is_checked_per_file_against_the_login_hosts_and_refused_off_a_prompted_worker_task() {
    let (home, features) = feature_context_with_login_hosts(&["remote-test-host"]);
    let cfg = features.config.get().unwrap();
    let prompt = write_prompt(home.path(), "prompt.md", "Synthetic body\n");
    write_task(&cfg, "keyed", &json!({"cron":"0 8 * * *", "prompt_file":prompt, "improve_host":"remote-test-host"}));
    write_task(&cfg, "other-host", &json!({"cron":"0 8 * * *", "prompt_file":prompt, "improve_host":"host9"}));
    write_task(
      &cfg,
      "master",
      &json!({"cron":"0 8 * * *", "prompt_file":prompt, "mode":"master", "improve_host":"remote-test-host"}),
    );
    write_task(
      &cfg,
      "handler",
      &json!({"cron":"0 8 * * *", "handler":"synthetic_handler", "improve_host":"remote-test-host"}),
    );
    write_task(
      &cfg,
      "steps",
      &json!({"cron":"0 8 * * *", "steps":[{"name":"first", "prompt_file":prompt}], "improve_host":"remote-test-host"}),
    );

    let snapshot = load_snapshot(&features, &cfg).unwrap();

    assert_eq!(snapshot.tasks.iter().map(|task| task.name.as_str()).collect::<Vec<_>>(), ["keyed"]);
    assert_eq!(snapshot.tasks[0].improve_host.as_deref(), Some("remote-test-host"));
    let logins = &cfg
      .section_value("host_auth")
      .and_then(|value| value.downcast_ref::<HostAuthConfig>())
      .expect("host_auth parses as HostAuthConfig")
      .browser_logins;
    assert_eq!(logins[0].host.as_deref(), Some("remote-test-host"));
    assert_eq!(logins[0].input_prompt.as_deref(), Some("synthetic-code"), "the Phase 1 input_prompt survives the load");
    assert_eq!(logins[0].lifetime_sec, Some(86400), "the Phase 1 lifetime_sec survives the load");
    assert_eq!(
      snapshot.errors.iter().map(|error| error.name.as_str()).collect::<Vec<_>>(),
      ["handler", "master", "other-host", "steps"]
    );
    let error_of =
      |name: &str| snapshot.errors.iter().find(|error| error.name == name).expect("the task's error").error.as_str();
    assert!(error_of("other-host").contains("improve_host 'host9' names no host in host_auth.browser_logins"));
    assert!(error_of("master").contains("not mode 'master'"), "{}", error_of("master"));
    assert!(error_of("handler").contains("not mode 'master'"), "{}", error_of("handler"));
    assert!(error_of("steps").contains("not mode 'master'"), "{}", error_of("steps"));
  }

  #[test]
  fn unchanged_cron_snapshot_reuses_yaml_and_prompt_reads_until_a_fingerprint_moves() {
    let (home, features) = feature_context();
    let cfg = features.config.get().unwrap();
    let directory = cfg.config_d_dir().join("cron.d");
    let prompt = write_prompt(home.path(), "synthetic-prompt.md", "Synthetic prompt\n");
    write_task(&cfg, "synthetic-task", &json!({"cron":"0 8 * * *", "prompt_file":prompt}));

    let first = load_snapshot(&features, &cfg).unwrap();
    let reads_after_first = super::cron_file_read_count(&directory);
    let prompt_reads_after_first = super::prompt_file_read_count(&prompt);
    let second = load_snapshot(&features, &cfg).unwrap();
    assert_eq!(second.fingerprint, first.fingerprint);
    assert_eq!(super::cron_file_read_count(&directory), reads_after_first);
    assert_eq!(super::prompt_file_read_count(&prompt), prompt_reads_after_first);

    write_task(&cfg, "synthetic-task", &json!({"cron":"0 8 * * *", "prompt_file":prompt, "enabled":false}));
    let changed = load_snapshot(&features, &cfg).unwrap();
    assert_ne!(changed.fingerprint, first.fingerprint);
    assert_eq!(super::cron_file_read_count(&directory), reads_after_first + 1);
    assert_eq!(super::prompt_file_read_count(&prompt), prompt_reads_after_first + 1);
    drop(home);
  }

  #[test]
  fn a_task_file_with_a_task_level_backend_or_project_fails_to_load() {
    let home = tempfile::tempdir().unwrap();
    let prompt = write_prompt(home.path(), "prompt.md", "Prompt");
    for key in ["backend", "project"] {
      let mut body = json!({"cron":"0 3 * * *", "prompt_file":prompt});
      body[key] = json!("synthetic");
      let error = validate_body("keyed", body, home.path(), &[]).unwrap_err();
      assert!(error.contains(&format!("unknown field `{key}`")), "{error}");
    }
  }

  #[test]
  fn the_cron_startup_check_fails_a_file_the_server_refuses_naming_the_file_and_the_field() {
    let (home, features) = feature_context();
    let cfg = features.config.get().unwrap();
    let prompt = write_prompt(home.path(), "prompt.md", "Prompt");
    write_task(&cfg, "keyed", &json!({"cron":"0 3 * * *", "prompt_file":prompt, "backend":"synthetic"}));
    let error = cron_files_problems(&cfg, home.path()).unwrap_err();
    assert!(error.contains("keyed.yaml"), "{error}");
    assert!(error.contains("unknown field `backend`"), "{error}");
  }

  #[test]
  fn step_source_and_distinct_backend_validation_matches_the_loader() {
    let home = tempfile::tempdir().unwrap();
    let first = write_prompt(home.path(), "first.md", "First prompt");
    let second = write_prompt(home.path(), "second.md", "Second prompt");
    let pointer = |name: &str, prompt: &Path| json!({"name":name,"prompt_file":prompt});
    let task = |steps: Value| json!({"cron":"0 3 * * *", "steps":steps});

    let duplicate = task(json!([pointer("selector", &first), pointer("selector", &second)]));
    assert!(
      validate_body("chained", duplicate, home.path(), &[]).unwrap_err().contains("duplicate step name 'selector'")
    );
    let empty = task(json!([]));
    assert!(validate_body("chained", empty, home.path(), &[]).unwrap_err().contains("exactly one of"));
    let inline = task(json!([{"name":"selector","prompt":"inline"}]));
    assert!(validate_body("chained", inline, home.path(), &[]).unwrap_err().contains("inline 'prompt'"));
    let missing = task(json!([{"name":"selector"}]));
    assert!(
      validate_body("chained", missing, home.path(), &[]).unwrap_err().contains("step 'selector' has no prompt body")
    );

    let distinct = task(json!([
      {"name":"selector","prompt_file":first,"backend":"synthetic-a"},
      {"name":"reviewer","prompt_file":second,"backend":"synthetic-b","distinct_backend_from":"selector"}
    ]));
    let loaded = validate_body("chained", distinct, home.path(), &[]).unwrap();
    assert_eq!(loaded.steps.as_ref().unwrap()[1].distinct_backend_from.as_deref(), Some("selector"));
    // Both steps of a pair name their own backend: a file that leaves either out fails to load.
    let source_unset = task(json!([
      {"name":"selector","prompt_file":first},
      {"name":"reviewer","prompt_file":second,"backend":"synthetic-b","distinct_backend_from":"selector"}
    ]));
    assert!(
      validate_body("chained", source_unset, home.path(), &[]).unwrap_err().contains("must name its own backend")
    );
    let reviewer_unset = task(json!([
      {"name":"selector","prompt_file":first,"backend":"synthetic-a"},
      {"name":"reviewer","prompt_file":second,"distinct_backend_from":"selector"}
    ]));
    assert!(
      validate_body("chained", reviewer_unset, home.path(), &[]).unwrap_err().contains("must name its own backend")
    );

    // Identical backend ids load here: whether they conflict depends on the config, which backend_refs_check reads.
    for invalid in [
      task(json!([
        {"name":"selector","prompt_file":first},
        {"name":"reviewer","prompt_file":second,"distinct_backend_from":"missing"}
      ])),
      task(json!([
        {"name":"reviewer","prompt_file":first,"distinct_backend_from":"selector"},
        {"name":"selector","prompt_file":second}
      ])),
      task(json!([
        {"name":"selector","prompt_file":first,"distinct_backend_from":"selector"},
        {"name":"reviewer","prompt_file":second}
      ])),
    ] {
      assert!(validate_body("chained", invalid, home.path(), &[]).is_err());
    }
  }

  #[test]
  fn startup_registration_does_not_seed_cron_files_and_seed_dump_is_stable() {
    let (home, features) = feature_context();
    let cfg = features.config.get().unwrap();
    assert_eq!(std::fs::read_dir(cfg.config_d_dir().join("cron.d")).unwrap().count(), 0);
    let value = json!({"cron":"27 6 * * *","timezone":"local","steps":[
      {"name":"selector","prompt_file":"prompts/cron/selector.md"},
      {"name":"reviewer","prompt_file":"prompts/cron/reviewer.md","distinct_backend_from":"selector"}
    ]});
    let bytes = crate::yaml::dump(&value).unwrap();
    let path = cfg.config_d_dir().join("cron.d/synthetic-default.yaml");
    std::fs::write(&path, &bytes).unwrap();
    std::fs::write(&path, crate::yaml::dump(&value).unwrap()).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), bytes.as_bytes());
    assert!(home.path().join("config.d/cron.d/synthetic-default.yaml").exists());
  }
}
