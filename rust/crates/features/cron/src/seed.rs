//! The repo-owned default cron tasks, seeded into the host's per-job cron files. The setup command (`charliebot setup`)
//! runs [`setup_step`] after the home provisioning, in registration order.

use std::path::{Path, PathBuf};

use charliebot_core::config::CharlieBotConfig;
use charliebot_core::home_init;
use charliebot_core::repo_root::CONFIGS_DIR_NAME;
use serde_json::Value;

use crate::loader;

/// The repo's `configs/cron.default.yaml` key that lists the default cron tasks. The seed reads the list through it,
/// and so does the release trial's stand-in reader in `app/cli`.
pub const SCHEDULED_TASKS_KEY: &str = "scheduled_tasks";

/// One seeded entry's report: the job name and whether its host file was `created`, already `exists`, or in a dry
/// run `would-create`.
#[derive(Debug)]
pub struct SeedReport {
  pub name: String,
  pub status: &'static str,
}

/// The repo-owned default cron tasks written into per-job host files by name.
///
/// Reads the repo's `configs/cron.default.yaml` and the host `config.d/cron.d/` directory. A default entry with no
/// host file `cron.d/<name>.yaml` is created from its body minus `name`, keeping the `prompt_file` pointer. An
/// existing host file is left unchanged, and host-only files are never rewritten or dropped.
///
/// With `dry_run` the same validation runs and nothing is written; a would-be seed reports `would-create`, so the
/// `./scripts/setup.sh -n` preview fails exactly where the real run fails. `loader::validate_body` checks every
/// body first, each step's `prompt_file` included, and a failed check writes nothing.
///
/// Creates `config.d/cron.d/` when absent, and each new host file goes through [`loader::write_task_file`], the
/// writer the cron API uses.
///
/// The callers are [`setup_step`], which `charliebot setup` runs, and the server's startup, which runs this function
/// after `init_charliebot_home` on every start. The server also writes cron files when the scheduler auto-binds an
/// unbound task's session id and when an API caller creates or updates a task.
pub fn seed_default_cron_tasks(
  cfg: &CharlieBotConfig,
  repo_root: &Path,
  dry_run: bool,
) -> Result<Vec<SeedReport>, String> {
  let defaults_data = read_defaults(repo_root)?;
  let default_entries = defaults_data.get(SCHEDULED_TASKS_KEY).and_then(Value::as_array).cloned().unwrap_or_default();

  // Validate every default entry on a resolved copy before touching the host
  // directory, so a bad repo default fails loudly without any partial write.
  let login_hosts = crate::config::browser_login_hosts(cfg);
  for entry in &default_entries {
    let (name, body) = split_entry(entry)?;
    loader::validate_body(&name, body, repo_root, &login_hosts)
      .map_err(|error| format!("default cron entry {name:?} failed the seed check: {error}"))?;
  }

  let cron_d_dir = loader::cron_dir(cfg);
  if !dry_run {
    std::fs::create_dir_all(&cron_d_dir).map_err(|error| format!("{}: {error}", cron_d_dir.display()))?;
  }

  let mut report = Vec::new();
  for entry in &default_entries {
    let (name, _) = split_entry(entry)?;
    let path = loader::cron_path(cfg, &name);
    if path.exists() {
      report.push(SeedReport { name, status: home_init::SEED_EXISTS });
      continue;
    }
    let (_, body) = split_entry(entry)?;
    // A dry run on a home without config.yaml reads no backend ids: the layout writes that file only in the real run,
    // and the real run checks the ids of the file it just wrote.
    if !dry_run || cfg.config_file().exists() {
      let missing = missing_step_backend_ids(cfg, &body);
      if !missing.is_empty() {
        return Err(format!(
          "default cron entry {name:?} names backend ids that the host config does not hold, so its file is \
           not seeded: {}",
          missing.join(", ")
        ));
      }
    }
    if dry_run {
      report.push(SeedReport { name, status: home_init::SEED_WOULD_CREATE });
      continue;
    }
    // Persist the pointer unchanged: the pointed file owns the prompt body,
    // and this host file carries only its path.
    loader::write_task_file(&path, &body).map_err(|error| format!("{}: {error}", path.display()))?;
    report.push(SeedReport { name, status: home_init::SEED_CREATED });
  }
  Ok(report)
}

/// The setup action: seed the default cron tasks, then list the effective scheduled tasks.
pub fn setup_step(cfg: &CharlieBotConfig, repo_root: &Path, dry_run: bool) -> Result<(), String> {
  println!("==> Seeding default cron tasks");
  // Per-task created/exists for repo-default cron entries, keyed on whether the
  // per-job host file config.d/cron.d/<name>.yaml exists. The dry-run runs the
  // same validation as the real run and writes nothing, so the preview fails
  // where the real run would; the one exception is the backend-id check on a
  // home without config.yaml (see seed_default_cron_tasks).
  for item in seed_default_cron_tasks(cfg, repo_root, dry_run)? {
    println!("  cron {}: {}", item.name, item.status);
  }

  // Effective scheduled task list: name / cron / resolved timezone. A task's backend belongs to its bound session,
  // which setup may not have yet.
  println!("  effective scheduled tasks:");
  let (snapshot, _) =
    loader::load_snapshot_for_paths(&loader::cron_dir(cfg), repo_root, &crate::config::browser_login_hosts(cfg))
      .map_err(|error| error.to_string())?;
  if snapshot.tasks.is_empty() {
    println!("    (none)");
  }
  for task in &snapshot.tasks {
    println!("    - {} | cron={} | tz={}", task.name, task.cron, task.timezone);
  }
  Ok(())
}

/// Disables every cron task of a home: seeds the repo's default tasks as the server seeds them at its start, then sets
/// `enabled: false` on each task file of the home's cron directory, seeded and copied files alike. A release trial runs
/// it before its server starts, so the trial's home fires no scheduled task. Returns the disabled task names in file
/// order.
pub fn disable_cron_tasks(cfg: &CharlieBotConfig, repo_root: &Path) -> Result<Vec<String>, String> {
  seed_default_cron_tasks(cfg, repo_root, false)?;
  let directory = loader::cron_dir(cfg);
  let mut paths = std::fs::read_dir(&directory)
    .and_then(|entries| entries.map(|entry| entry.map(|entry| entry.path())).collect::<Result<Vec<_>, _>>())
    .map_err(|error| format!("{}: {error}", directory.display()))?;
  paths.sort();
  let mut names = Vec::new();
  for path in paths {
    let file_name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
    if !path.is_file() || !file_name.ends_with(".yaml") || file_name.starts_with('.') {
      continue;
    }
    loader::write_task_key_at(&path, "enabled", Value::Bool(false)).map_err(|error| error.to_string())?;
    names.push(path.file_stem().and_then(|stem| stem.to_str()).unwrap_or_default().to_string());
  }
  Ok(names)
}

/// The backend ids that the steps of a default entry name and the host config does not hold, in step order.
fn missing_step_backend_ids(cfg: &CharlieBotConfig, body: &Value) -> Vec<String> {
  body
    .get("steps")
    .and_then(Value::as_array)
    .into_iter()
    .flatten()
    .filter_map(|step| step.get("backend").and_then(Value::as_str))
    .filter(|backend| cfg.backends.check_backend_id(backend).is_err())
    .map(str::to_string)
    .collect()
}

/// One default entry's name and the body without it, or the error that names an entry that is not a mapping or
/// carries no usable `name`.
fn split_entry(entry: &Value) -> Result<(String, Value), String> {
  let Some(object) = entry.as_object() else {
    return Err(format!("invalid default cron entry (not a mapping): {entry:?}"));
  };
  let Some(name) = object.get("name").and_then(Value::as_str) else {
    return Err(format!("invalid default cron entry without a string 'name': {entry:?}"));
  };
  let mut body = entry.clone();
  // `Map::remove` swap-removes under `preserve_order`, moving the last key into the dropped slot; the
  // seeded host file keeps the document order, so the removal shifts.
  body.as_object_mut().expect("checked above").shift_remove("name");
  Ok((name.to_string(), body))
}

/// The repo's `configs/cron.default.yaml`, as the JSON value the loader's writers and validators take. The
/// direct parse keeps the document's key order, which the seeded host file keeps; a missing or empty file
/// reads as `Null`.
fn read_defaults(repo_root: &Path) -> Result<Value, String> {
  let path: PathBuf = repo_root.join(CONFIGS_DIR_NAME).join("cron.default.yaml");
  let text = std::fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
  serde_yaml::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
  use std::fs;

  use super::*;
  use charliebot_core::config::BackendOption;
  use serde_json::json;

  /// The committed checkout, whose `configs/cron.default.yaml` and prompt files the server reads at start.
  fn checkout_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../..")
  }

  /// A temp repo with the checkout's own `configs/cron.default.yaml` and the prompt files it points to.
  fn repo_with_defaults() -> tempfile::TempDir {
    let repo = tempfile::tempdir().expect("a temp repo");
    let checkout = checkout_root();
    let defaults = fs::read_to_string(checkout.join("configs/cron.default.yaml")).expect("the committed defaults");
    fs::create_dir_all(repo.path().join("configs")).expect("create the configs directory");
    fs::write(repo.path().join("configs/cron.default.yaml"), defaults).expect("write the defaults");
    for prompt in [
      "prompts/cron/memory_curator/memory_selector.md",
      "prompts/cron/memory_curator/memory_reviewer.md",
      "prompts/cron/proactive_agent/prompt.md",
    ] {
      let path = repo.path().join(prompt);
      fs::create_dir_all(path.parent().expect("the prompt directory")).expect("create the prompt directory");
      fs::write(path, "Synthetic prompt\n").expect("write the prompt");
    }
    repo
  }

  fn cfg(home: &Path) -> CharlieBotConfig {
    let mut cfg = charliebot_core::config::load_config(home, &Default::default()).expect("the empty config loads");
    for id in ["claude-opus", "claude-fable"] {
      cfg.backends.options.push(BackendOption {
        id: id.to_string(),
        label: id.to_string(),
        prompt_overlay: None,
        billing: None,
        model: format!("{id}-model"),
        backend_type: "synthetic".to_string(),
        type_fields: Default::default(),
        visible: true,
      });
    }
    cfg
  }

  /// The default table's entries, each with its `name`.
  fn default_entries(repo: &Path) -> Vec<Value> {
    read_defaults(repo).expect("the default table")[SCHEDULED_TASKS_KEY]
      .as_array()
      .expect("the default table lists scheduled tasks")
      .clone()
  }

  #[test]
  #[ignore = "integration"]
  fn a_missing_host_file_carries_its_default_entry_keys() {
    let repo = checkout_root();
    let home = tempfile::tempdir().expect("a temp home");
    let cfg = cfg(home.path());

    seed_default_cron_tasks(&cfg, &repo, false).expect("the seed");
    for entry in default_entries(&repo) {
      let (name, body) = split_entry(&entry).expect("a default entry");
      let text = fs::read_to_string(loader::cron_path(&cfg, &name)).expect("the seeded host file");
      let seeded: Value = serde_yaml::from_str(&text).expect("the seeded host file is YAML");
      assert_eq!(seeded, body, "{name} carries its default entry's keys");
    }
  }

  #[test]
  #[ignore = "integration"]
  fn an_owner_edited_host_file_stays_byte_for_byte() {
    let repo = checkout_root();
    let home = tempfile::tempdir().expect("a temp home");
    let cfg = cfg(home.path());
    fs::create_dir_all(loader::cron_dir(&cfg)).expect("the cron directory");
    let owner_file = loader::cron_path(&cfg, "proactive-agent");
    let owner_text = "# paused by the owner\ncron: \"0 7 * * *\"\n\
      prompt_file: prompts/cron/proactive_agent/prompt.md\nmode: master\nenabled: false\n";
    fs::write(&owner_file, owner_text).expect("the owner's host file");

    let report = seed_default_cron_tasks(&cfg, &repo, false).expect("the seed");
    assert_eq!(fs::read_to_string(&owner_file).expect("the owner's host file"), owner_text);
    assert!(
      report.iter().any(|item| item.name == "proactive-agent" && item.status == home_init::SEED_EXISTS),
      "{report:?}"
    );
  }

  #[test]
  #[ignore = "integration"]
  fn a_home_with_every_default_file_gets_no_new_file() {
    let repo = checkout_root();
    let home = tempfile::tempdir().expect("a temp home");
    let cfg = cfg(home.path());
    fs::create_dir_all(loader::cron_dir(&cfg)).expect("the cron directory");
    let mut owner_files = Vec::new();
    for entry in default_entries(&repo) {
      let (name, _) = split_entry(&entry).expect("a default entry");
      let text = format!("# the owner's {name}\nenabled: false\n");
      fs::write(loader::cron_path(&cfg, &name), &text).expect("the owner's host file");
      owner_files.push((name, text));
    }

    let report = seed_default_cron_tasks(&cfg, &repo, false).expect("the seed");
    assert!(report.iter().all(|item| item.status == home_init::SEED_EXISTS), "{report:?}");
    let mut names: Vec<String> = fs::read_dir(loader::cron_dir(&cfg))
      .expect("the cron directory")
      .map(|entry| entry.expect("a directory entry").file_name().to_string_lossy().into_owned())
      .collect();
    names.sort();
    let mut wanted: Vec<String> = owner_files.iter().map(|(name, _)| format!("{name}.yaml")).collect();
    wanted.sort();
    assert_eq!(names, wanted, "the seed adds no file");
    for (name, text) in &owner_files {
      assert_eq!(fs::read_to_string(loader::cron_path(&cfg, name)).expect("the owner's host file"), *text);
    }
  }

  #[test]
  fn a_real_run_writes_one_host_file_per_default_entry_and_the_rerun_reports_exists() {
    let repo = repo_with_defaults();
    let (_home, features) = crate::test_support::feature_context_with_extra_backends(&["claude-opus", "claude-fable"]);
    let cfg = features.config.get().expect("the test config loads");

    let report = seed_default_cron_tasks(&cfg, repo.path(), false).expect("the first seed");
    assert_eq!(
      report.iter().map(|item| (item.name.as_str(), item.status)).collect::<Vec<_>>(),
      [("memory-curator", "created"), ("storage-cool", "created"), ("proactive-agent", "created")]
    );
    let curator = loader::cron_path(&cfg, "memory-curator");
    let body = fs::read_to_string(&curator).expect("the seeded host file");
    assert!(body.contains("cron: 27 6 * * *"), "{body}");
    assert!(!body.lines().any(|line| line.starts_with("name:")), "the host file carries no name key: {body}");
    assert!(body.contains("prompt_file: prompts/cron/memory_curator/memory_selector.md"), "{body}");
    let curator_value: Value = serde_yaml::from_str(&body).expect("the seeded host file is YAML");
    assert!(curator_value.get("project").is_none(), "the task file carries no project: {body}");
    assert!(curator_value.get("backend").is_none(), "the task file carries no task backend: {body}");
    assert_eq!(curator_value["steps"][0]["backend"], "claude-opus");
    assert_eq!(curator_value["steps"][1]["backend"], "claude-fable");
    for name in ["memory-curator", "storage-cool", "proactive-agent"] {
      let path = loader::cron_path(&cfg, name);
      let body = fs::read_to_string(&path).expect("the seeded host file");
      let value: Value = serde_yaml::from_str(&body).expect("the seeded host file is YAML");
      assert!(value.get("project").is_none(), "the seeded {name} carries no project: {body}");
      assert!(value.get("backend").is_none(), "the seeded {name} carries no task backend: {body}");
    }

    let report = seed_default_cron_tasks(&cfg, repo.path(), false).expect("the second seed");
    assert_eq!(
      report.iter().map(|item| (item.name.as_str(), item.status)).collect::<Vec<_>>(),
      [("memory-curator", "exists"), ("storage-cool", "exists"), ("proactive-agent", "exists")]
    );
  }

  #[test]
  fn a_default_entry_names_only_the_backend_ids_the_host_config_holds() {
    let (_home, features) = crate::test_support::feature_context();
    let cfg = features.config.get().unwrap();
    let body = json!({"steps": [
      {"name": "selector", "backend": "synthetic"},
      {"name": "reviewer", "backend": "gone-id"},
    ]});
    assert_eq!(missing_step_backend_ids(&cfg, &body), ["gone-id"]);
  }

  #[test]
  fn a_missing_default_step_backend_id_refuses_the_seed_before_writing_memory_curator() {
    let repo = repo_with_defaults();
    for (present, missing) in [("claude-fable", "claude-opus"), ("claude-opus", "claude-fable")] {
      let (_home, features) = crate::test_support::feature_context_with_extra_backends(&[present]);
      let cfg = features.config.get().expect("the test config loads");

      let error = seed_default_cron_tasks(&cfg, repo.path(), false).expect_err("the missing backend refuses the seed");

      assert!(error.contains(missing), "the error names {missing}: {error}");
      assert!(!error.contains(present), "the error does not name configured id {present}: {error}");
      assert!(
        !loader::cron_path(&cfg, "memory-curator").exists(),
        "the seed does not write memory-curator when {missing} is missing"
      );
    }
  }

  #[test]
  #[ignore = "integration"]
  fn a_dry_run_validates_and_writes_nothing() {
    let repo = repo_with_defaults();
    let home = tempfile::tempdir().expect("a temp home");
    let cfg = cfg(home.path());

    let report = seed_default_cron_tasks(&cfg, repo.path(), true).expect("the dry run");
    assert_eq!(
      report.iter().map(|item| (item.name.as_str(), item.status)).collect::<Vec<_>>(),
      [("memory-curator", "would-create"), ("storage-cool", "would-create"), ("proactive-agent", "would-create")]
    );
    assert!(!loader::cron_dir(&cfg).exists(), "the dry run created no cron.d directory");
  }

  #[test]
  #[ignore = "integration"]
  fn an_unreadable_prompt_file_fails_the_dry_run_and_the_real_run_alike() {
    let repo = repo_with_defaults();
    fs::remove_file(repo.path().join("prompts/cron/memory_curator/memory_selector.md"))
      .expect("remove the selector prompt");
    let home = tempfile::tempdir().expect("a temp home");
    let cfg = cfg(home.path());

    let error = seed_default_cron_tasks(&cfg, repo.path(), true).expect_err("the dry run fails loudly");
    assert!(error.contains("prompt_file"), "{error}");
    assert!(error.contains("memory-curator"), "the seed error names the failed entry: {error}");
    assert!(seed_default_cron_tasks(&cfg, repo.path(), false).is_err(), "the real run fails the same way");
    assert!(!loader::cron_dir(&cfg).exists(), "no partial write happened");
  }
}
