//! `cron.d` file loading, prompt pointers, broken-file alerts and backend startup validation.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use charliebot_core::config::CharlieBotConfig;
use charliebot_core::repo_root::RepoRoot;
use charliebot_registry::Registry;
use charliebot_registry::context::FeatureContext;
use serde_json::Value;

use super::config::{ScheduledTaskConfig, ScheduledTaskError};

/// One sorted read of every visible cron file.
#[derive(Clone, Debug, Default)]
pub struct CronSnapshot {
  /// Valid task configs sorted by filename.
  pub tasks: Vec<ScheduledTaskConfig>,
  /// Broken file records sorted by filename.
  pub errors: Vec<ScheduledTaskError>,
}

/// Return the file directory owned by this profile.
pub fn cron_dir(cfg: &CharlieBotConfig) -> PathBuf {
  cfg.config_d_dir().join("cron.d")
}

/// Return one task's config file path.
pub fn cron_path(cfg: &CharlieBotConfig, name: &str) -> PathBuf {
  cron_dir(cfg).join(format!("{name}.yaml"))
}

/// Validate a task name before forming a host file path.
pub fn valid_name(name: &str) -> bool {
  let mut chars = name.chars();
  chars.next().is_some_and(|first| first.is_ascii_alphanumeric())
    && chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
}

/// Reload every visible YAML config, retaining an error per broken file.
pub fn load_snapshot(features: &Arc<FeatureContext>, cfg: &CharlieBotConfig) -> anyhow::Result<CronSnapshot> {
  let repo = repo_root(features);
  let directory = cron_dir(cfg);
  let entries = match std::fs::read_dir(&directory) {
    Ok(entries) => entries,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
      let snapshot = CronSnapshot::default();
      fire_error_alert(features, cfg, &snapshot.errors);
      return Ok(snapshot);
    }
    Err(error) => return Err(error.into()),
  };
  let mut paths = entries.map(|entry| entry.map(|entry| entry.path())).collect::<Result<Vec<_>, _>>()?;
  paths.sort();
  let mut snapshot = CronSnapshot::default();
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
    match load_file(&path, &name, repo.path()) {
      Ok(task) => snapshot.tasks.push(task),
      Err(error) => {
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
  fire_error_alert(features, cfg, &snapshot.errors);
  Ok(snapshot)
}

/// Load and validate one cron file while preserving prompt pointer strings.
pub fn load_file(path: &Path, name: &str, repo: &Path) -> Result<ScheduledTaskConfig, String> {
  let raw = std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
  let yaml = serde_yaml::from_str::<serde_yaml::Value>(&raw).map_err(|error| error.to_string())?;
  let body = serde_json::to_value(yaml).map_err(|error| error.to_string())?;
  validate_body(name, body, repo)
}

/// Resolve one in-memory create or update body before persisting it.
pub fn validate_body(name: &str, mut body: Value, repo: &Path) -> Result<ScheduledTaskConfig, String> {
  let object = body.as_object_mut().ok_or_else(|| "cron config must be a mapping".to_string())?;
  if object.contains_key("name") {
    return Err("the body must not carry a 'name' key; the file name is the job name".to_string());
  }
  if object.contains_key("prompt") {
    return Err("a cron.d host file must not carry an inline 'prompt'; it holds the path to the prompt source under 'prompt_file', and the loader reads that file on every load".to_string());
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
  super::config::task_from_value(name, body)
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
  let prompt = std::fs::read_to_string(&path)
    .map_err(|error| format!("cron entry {name:?} prompt_file unreadable: {} ({error})", path.display()))?;
  entry.insert("prompt".to_string(), Value::String(prompt));
  Ok(())
}

fn resolve_path(pointer: &str, repo: &Path) -> Result<PathBuf, String> {
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
  let raw = match std::fs::read_to_string(path) {
    Ok(raw) => raw,
    Err(error) => {
      tracing::debug!(path = %path.display(), error = %error, "cron_file_enabled_unreadable");
      return None;
    }
  };
  let value = match serde_yaml::from_str::<serde_yaml::Value>(&raw) {
    Ok(value) => value,
    Err(error) => {
      tracing::debug!(path = %path.display(), error = %error, "cron_file_enabled_unreadable");
      return None;
    }
  };
  value.get("enabled")?.as_bool()
}

fn repo_root(features: &FeatureContext) -> Arc<RepoRoot> {
  features.startup.require::<RepoRoot>().expect("the server inserts the checkout before registering request handlers")
}

fn fire_error_alert(features: &Arc<FeatureContext>, cfg: &CharlieBotConfig, errors: &[ScheduledTaskError]) {
  let names = errors.iter().map(|error| error.name.clone()).collect::<Vec<_>>();
  let path = cfg.charliebot_home.join("state/cron_alert_fingerprint.json");
  let old = match std::fs::read_to_string(&path) {
    Ok(raw) => match serde_json::from_str::<Vec<String>>(&raw) {
      Ok(names) => names.into_iter().collect::<std::collections::HashSet<_>>(),
      Err(error) => {
        tracing::warn!(path = %path.display(), error = %error, "cron_alert_state_unparseable");
        std::collections::HashSet::new()
      }
    },
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => std::collections::HashSet::new(),
    Err(error) => {
      tracing::warn!(path = %path.display(), error = %error, "cron_alert_state_unreadable");
      std::collections::HashSet::new()
    }
  };
  if old == names.iter().cloned().collect() {
    return;
  }
  if tokio::runtime::Handle::try_current().is_err() {
    tracing::info!(names = ?names, "cron_alert_skipped_no_event_loop");
    return;
  }
  let message = alert_message(&names);
  let credentials = Arc::clone(&features.credentials);
  let chat_id = cfg.telegram.chat_id.clone();
  tokio::spawn(async move {
    match send_telegram(&credentials, chat_id.as_deref(), &message).await {
      Ok(()) => tracing::info!("cron_alert_sent"),
      Err(error) => tracing::error!(error = %error, "cron_alert_send_failed"),
    }
  });
  if let Err(error) = std::fs::create_dir_all(path.parent().expect("state path has a parent")).and_then(|()| {
    charliebot_core::fsutil::atomic_write_text(
      &path,
      &format!("{}\n", serde_json::to_string(&names).expect("cron names serialize")),
    )
  }) {
    tracing::error!(path = %path.display(), error = %error, "cron_alert_state_write_failed");
  }
}

fn alert_message(names: &[String]) -> String {
  if names.is_empty() {
    "✅ all cron load failures resolved".to_string()
  } else {
    format!("⚠️ cron tasks failed to load: {}", names.join(", "))
  }
}

async fn send_telegram(
  credentials: &charliebot_core::hot_reload::HotReload<charliebot_core::credentials::Credentials>,
  chat_id: Option<&str>,
  message: &str,
) -> anyhow::Result<()> {
  let credentials = credentials.get()?;
  let token = credentials.require("telegram", "bot_token")?;
  let chat_id = chat_id.ok_or_else(|| anyhow::anyhow!("telegram.chat_id is not configured"))?;
  let response = charliebot_compat::http_client::shared_client()?
    .post(format!("https://api.telegram.org/bot{token}/sendMessage"))
    .json(&serde_json::json!({"chat_id": chat_id, "text": message.chars().take(4096).collect::<String>(), "parse_mode": "Markdown"}))
    .send()
    .await?;
  if response.status().is_success() {
    return Ok(());
  }
  let status = response.status();
  let body = response.text().await.unwrap_or_default();
  anyhow::bail!("Telegram API returned {status}: {}", body.chars().take(200).collect::<String>())
}

/// Check all valid task and step backend references before serving.
pub fn backend_refs_check(_registry: &Registry, cfg: &CharlieBotConfig) -> Result<(), String> {
  let mut problems = Vec::new();
  let known = cfg.backends.options.iter().map(|option| option.id.as_str()).collect::<std::collections::HashSet<_>>();
  let directory = cron_dir(cfg);
  let entries = match std::fs::read_dir(&directory) {
    Ok(entries) => entries,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
    Err(error) => return Err(format!("{}: {error}", directory.display())),
  };
  let mut paths = entries
    .map(|entry| entry.map(|entry| entry.path()))
    .collect::<Result<Vec<_>, _>>()
    .map_err(|error| format!("{}: {error}", directory.display()))?;
  paths.sort();
  for path in paths {
    if path.extension().and_then(|ext| ext.to_str()) != Some("yaml")
      || path.file_name().is_some_and(|name| name.to_string_lossy().starts_with('.'))
    {
      continue;
    }
    let Some(stem) = path.file_stem().and_then(|name| name.to_str()) else { continue };
    let repo = std::env::var_os(charliebot_core::repo_root::REPO_ROOT_ENV)
      .map(PathBuf::from)
      .ok_or_else(|| format!("{} is not set", charliebot_core::repo_root::REPO_ROOT_ENV))?;
    let Ok(task) = load_file(&path, stem, &repo) else { continue };
    for (entry, backend) in std::iter::once(("backend".to_string(), task.backend.as_deref())).chain(
      task.steps.iter().flatten().map(|step| (format!("steps '{}' backend", step.name), step.backend.as_deref())),
    ) {
      if let Some(backend) = backend.filter(|backend| !backend.is_empty())
        && !known.contains(backend)
      {
        problems.push(format!("{}: {entry} names unknown backend '{backend}'", path.display()));
      }
    }
  }
  if problems.is_empty() {
    return Ok(());
  }
  Err(format!(
    "backend references must name a backends.options id (id rule: BackendsConfig in src/infra/config.py):\n{}",
    problems.into_iter().map(|problem| format!("  {problem}")).collect::<Vec<_>>().join("\n")
  ))
}

#[cfg(test)]
mod tests {
  use std::path::Path;

  use charliebot_core::config::CharlieBotConfig;
  use serde_json::{Value, json};

  use super::{alert_message, fire_error_alert, load_snapshot, validate_body};
  use crate::cron::test_support::feature_context;

  fn write_task(cfg: &CharlieBotConfig, name: &str, body: &Value) {
    let directory = cfg.config_d_dir().join("cron.d");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join(format!("{name}.yaml")), charliebot_core::yaml::dump(body).unwrap()).unwrap();
  }

  fn write_prompt(directory: &Path, name: &str, body: &str) -> std::path::PathBuf {
    std::fs::create_dir_all(directory).unwrap();
    let path = directory.join(name);
    std::fs::write(&path, body).unwrap();
    path
  }

  #[test]
  fn prompt_pointers_resolve_and_bad_task_sources_are_isolated() {
    // Python: tests/test_cron_defaults.py::test_loader_loads_prompt_file_pointer
    // Python: tests/test_cron_defaults.py::test_loader_rejects_task_without_prompt_file
    // Python: tests/test_cron_backend.py::test_load_cron_file_loads_prompt_file
    let (home, features) = feature_context();
    let cfg = features.config.get().unwrap();
    let prompt = write_prompt(home.path(), "prompt.md", "Synthetic body\n");
    write_task(&cfg, "valid", &json!({"cron":"0 8 * * *", "prompt_file":prompt}));
    write_task(&cfg, "inline", &json!({"cron":"0 8 * * *", "prompt":"inline body"}));
    write_task(&cfg, "missing", &json!({"cron":"0 8 * * *"}));

    let snapshot = load_snapshot(&features, &cfg).unwrap();

    assert_eq!(snapshot.tasks.len(), 1);
    assert_eq!(snapshot.tasks[0].prompt.as_deref(), Some("Synthetic body\n"));
    assert_eq!(snapshot.tasks[0].prompt_file.as_deref(), Some(prompt.to_str().unwrap()));
    assert_eq!(snapshot.errors.iter().map(|error| error.name.as_str()).collect::<Vec<_>>(), ["inline", "missing"]);
    assert!(snapshot.errors[0].error.contains("inline 'prompt'"));
    assert!(snapshot.errors[1].error.contains("prompt_file"));
  }

  #[test]
  fn step_source_and_distinct_backend_validation_matches_the_loader() {
    // Python: tests/test_cron_steps.py::test_load_cron_file_rejects_duplicate_step_names
    // Python: tests/test_cron_steps.py::test_load_cron_file_rejects_empty_steps
    // Python: tests/test_cron_steps.py::test_load_cron_file_rejects_step_prompt_source_violation
    // Python: tests/test_cron_steps.py::test_load_cron_file_rejects_distinct_backend_violations
    // Python: tests/test_cron_steps.py::test_load_cron_file_accepts_distinct_written_backends
    // Python: tests/test_cron_steps.py::test_load_cron_file_accepts_unset_backends_with_distinct_backend_from
    let home = tempfile::tempdir().unwrap();
    let first = write_prompt(home.path(), "first.md", "First prompt");
    let second = write_prompt(home.path(), "second.md", "Second prompt");
    let pointer = |name: &str, prompt: &Path| json!({"name":name,"prompt_file":prompt});
    let task = |steps: Value| json!({"cron":"0 3 * * *", "steps":steps});

    let duplicate = task(json!([pointer("selector", &first), pointer("selector", &second)]));
    assert!(validate_body("chained", duplicate, home.path()).unwrap_err().contains("duplicate step name 'selector'"));
    let empty = task(json!([]));
    assert!(validate_body("chained", empty, home.path()).unwrap_err().contains("exactly one of"));
    let inline = task(json!([{"name":"selector","prompt":"inline"}]));
    assert!(validate_body("chained", inline, home.path()).unwrap_err().contains("inline 'prompt'"));
    let missing = task(json!([{"name":"selector"}]));
    assert!(validate_body("chained", missing, home.path()).unwrap_err().contains("step 'selector' has no prompt body"));

    let distinct = task(json!([
      {"name":"selector","prompt_file":first,"backend":"synthetic-a"},
      {"name":"reviewer","prompt_file":second,"backend":"synthetic-b","distinct_backend_from":"selector"}
    ]));
    let loaded = validate_body("chained", distinct, home.path()).unwrap();
    assert_eq!(loaded.steps.as_ref().unwrap()[1].distinct_backend_from.as_deref(), Some("selector"));
    let unset = task(json!([
      {"name":"selector","prompt_file":first},
      {"name":"reviewer","prompt_file":second,"distinct_backend_from":"selector"}
    ]));
    assert!(validate_body("chained", unset, home.path()).is_ok());

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
      task(json!([
        {"name":"selector","prompt_file":first,"backend":"synthetic-a"},
        {"name":"reviewer","prompt_file":second,"backend":"synthetic-a","distinct_backend_from":"selector"}
      ])),
      json!({"cron":"0 3 * * *","backend":"synthetic-a","steps":[
        {"name":"selector","prompt_file":first},
        {"name":"reviewer","prompt_file":second,"distinct_backend_from":"selector"}
      ]}),
    ] {
      assert!(validate_body("chained", invalid, home.path()).is_err());
    }
  }

  #[test]
  fn startup_registration_does_not_seed_cron_files_and_seed_dump_is_stable() {
    // Python: tests/test_cron_defaults.py::test_startup_never_writes_cron
    // Python: tests/test_cron_defaults.py::test_seed_idempotence
    let (home, features) = feature_context();
    let cfg = features.config.get().unwrap();
    assert_eq!(std::fs::read_dir(cfg.config_d_dir().join("cron.d")).unwrap().count(), 0);
    let value = json!({"cron":"27 6 * * *","timezone":"local","steps":[
      {"name":"selector","prompt_file":"prompts/cron/selector.md"},
      {"name":"reviewer","prompt_file":"prompts/cron/reviewer.md","distinct_backend_from":"selector"}
    ]});
    let bytes = charliebot_core::yaml::dump(&value).unwrap();
    let path = cfg.config_d_dir().join("cron.d/synthetic-default.yaml");
    std::fs::write(&path, &bytes).unwrap();
    std::fs::write(&path, charliebot_core::yaml::dump(&value).unwrap()).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), bytes.as_bytes());
    assert!(home.path().join("config.d/cron.d/synthetic-default.yaml").exists());
  }

  #[tokio::test]
  async fn alert_fingerprint_tracks_transitions_and_telegram_failures_stay_local() {
    // Python: tests/test_cron_alerts.py::test_alert_fires_once_on_transition_recovers_once_and_repeats_nothing
    // Python: tests/test_cron_alerts.py::test_telegram_failure_is_log_only
    let (home, features) = feature_context();
    let cfg = features.config.get().unwrap();
    let error = |name: &str| super::super::config::ScheduledTaskError {
      name: name.to_string(),
      path: home.path().join(format!("config.d/cron.d/{name}.yaml")).display().to_string(),
      error: "synthetic load error".to_string(),
      enabled: Some(true),
    };
    let state = cfg.charliebot_home.join("state/cron_alert_fingerprint.json");

    fire_error_alert(&features, &cfg, &[error("alpha"), error("beta")]);
    assert_eq!(std::fs::read_to_string(&state).unwrap(), "[\"alpha\",\"beta\"]\n");
    fire_error_alert(&features, &cfg, &[error("alpha"), error("beta")]);
    assert_eq!(std::fs::read_to_string(&state).unwrap(), "[\"alpha\",\"beta\"]\n");
    fire_error_alert(&features, &cfg, &[]);
    assert_eq!(std::fs::read_to_string(&state).unwrap(), "[]\n");
    tokio::task::yield_now().await;
    assert_eq!(alert_message(&["alpha".to_string(), "beta".to_string()]), "⚠️ cron tasks failed to load: alpha, beta");
    assert_eq!(alert_message(&[]), "✅ all cron load failures resolved");
  }

  #[test]
  fn alert_without_an_event_loop_does_not_persist_a_fingerprint() {
    // Python: tests/test_cron_alerts.py::test_no_event_loop_skips_send_without_persisting
    let (_home, features) = feature_context();
    let cfg = features.config.get().unwrap();
    let error = super::super::config::ScheduledTaskError {
      name: "synthetic".to_string(),
      path: "synthetic.yaml".to_string(),
      error: "synthetic load error".to_string(),
      enabled: Some(true),
    };
    fire_error_alert(&features, &cfg, &[error]);
    assert!(!cfg.charliebot_home.join("state/cron_alert_fingerprint.json").exists());
  }
}
