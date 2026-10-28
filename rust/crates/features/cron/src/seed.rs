//! Seed repo-owned default cron tasks into the host's per-job cron files (`src/features/cron/seed.py`). The setup
//! command (`charliebot setup`) runs [`setup_step`] after the home provisioning, in registration order.

use std::path::{Path, PathBuf};

use charliebot_core::config::CharlieBotConfig;
use serde_json::Value;

use crate::loader;
use crate::scheduler::effective_backend;

/// One seeded entry's report: the job name and whether its host file was `created`, already `exists`, or in a dry
/// run `would-create`.
#[derive(Debug)]
pub struct SeedReport {
  pub name: String,
  pub status: &'static str,
}

/// Seed repo-owned default cron tasks into per-job host files by name.
///
/// Reads `configs/cron.default.yaml` from the repo and the host `config.d/cron.d/` directory. For each default
/// entry: if no host file `cron.d/<name>.yaml` exists, create it with the entry body minus `name`, keeping the
/// entry's `prompt_file` pointer intact (the pointed file owns the prompt body and the host file carries only the
/// path to it); if one exists, change nothing about it. Never rewrites or drops host-only files.
///
/// With `dry_run` the same validation runs, nothing is written or created, and a would-be seed reports
/// `would-create` — the preview `./scripts/setup.sh -n` shows must fail exactly where the real run would.
///
/// Creates `config.d/cron.d/` when absent. Writes through [`loader::write_task_file`] (the same writer the cron
/// API uses).
/// Validates every default entry before writing: each must construct a [`crate::config::ScheduledTaskConfig`]
/// after `prompt_file`/`local` resolution (an entry with `steps` resolves each step's `prompt_file` exactly like
/// the task-level pointer) and every `prompt_file` must resolve to an existing file. Fails loudly without writing
/// if validation fails.
///
/// This is a library function invoked only by [`setup_step`], which `charliebot setup` runs. The server startup
/// path ([`charliebot_core::home_init::init_charliebot_home`]) never calls it, so the server never writes cron
/// config — that is an invariant the tests assert directly.
pub fn seed_default_cron_tasks(
  cfg: &CharlieBotConfig,
  repo_root: &Path,
  dry_run: bool,
) -> Result<Vec<SeedReport>, String> {
  let defaults_data = read_defaults(repo_root)?;
  let default_entries = defaults_data.get("scheduled_tasks").and_then(Value::as_array).cloned().unwrap_or_default();

  // Validate every default entry on a resolved copy before touching the host
  // directory, so a bad repo default fails loudly without any partial write.
  for entry in &default_entries {
    let (name, body) = split_entry(entry)?;
    loader::validate_body(&name, body, repo_root)?;
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
      report.push(SeedReport { name, status: "exists" });
      continue;
    }
    if dry_run {
      report.push(SeedReport { name, status: "would-create" });
      continue;
    }
    let (_, body) = split_entry(entry)?;
    // Persist the pointer unchanged: the pointed file owns the prompt body,
    // and this host file carries only its path.
    loader::write_task_file(&path, &body).map_err(|error| format!("{}: {error}", path.display()))?;
    report.push(SeedReport { name, status: "created" });
  }
  Ok(report)
}

/// Seed the default cron tasks, then list the effective scheduled tasks (`seed.setup_step`).
pub fn setup_step(cfg: &CharlieBotConfig, repo_root: &Path, dry_run: bool) -> Result<(), String> {
  println!("==> Seeding default cron tasks");
  // Per-task created/exists for repo-default cron entries, keyed on whether the
  // per-job host file config.d/cron.d/<name>.yaml exists. The dry-run runs the
  // same validation as the real run and writes nothing, so the preview fails
  // exactly where the real run would.
  for item in seed_default_cron_tasks(cfg, repo_root, dry_run)? {
    println!("  cron {}: {}", item.name, item.status);
  }

  // Effective scheduled task list: name / cron / resolved timezone / resolved
  // backend. If backend resolution raises (e.g. empty backends.options on a fresh
  // host), print the reason instead of aborting setup.
  println!("  effective scheduled tasks:");
  let (snapshot, _) =
    loader::load_snapshot_for_paths(&loader::cron_dir(cfg), repo_root).map_err(|error| error.to_string())?;
  if snapshot.tasks.is_empty() {
    println!("    (none)");
  }
  for task in &snapshot.tasks {
    let backend = match effective_backend(task, cfg) {
      Ok(backend) => backend,
      Err(error) => format!("unresolved: {error}"),
    };
    println!("    - {} | cron={} | tz={} | backend={}", task.name, task.cron, task.timezone, backend);
  }
  Ok(())
}

/// One default entry's name and the body without it, or the failure Python's `seed_default_cron_tasks` raises for
/// an entry that is not a mapping or carries no usable `name`.
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
  let path: PathBuf = repo_root.join("configs").join("cron.default.yaml");
  let text = std::fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
  serde_yaml::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
  use std::fs;

  use super::*;

  /// A temp repo with the checkout's own `configs/cron.default.yaml` and the prompt files it points to.
  fn repo_with_defaults() -> tempfile::TempDir {
    let repo = tempfile::tempdir().expect("a temp repo");
    let checkout = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../..");
    let defaults = fs::read_to_string(checkout.join("configs/cron.default.yaml")).expect("the committed defaults");
    fs::create_dir_all(repo.path().join("configs")).expect("create the configs directory");
    fs::write(repo.path().join("configs/cron.default.yaml"), defaults).expect("write the defaults");
    for prompt in ["prompts/cron/memory_curator/memory_selector.md", "prompts/cron/memory_curator/memory_reviewer.md"] {
      let path = repo.path().join(prompt);
      fs::create_dir_all(path.parent().expect("the prompt directory")).expect("create the prompt directory");
      fs::write(path, "Synthetic prompt\n").expect("write the prompt");
    }
    repo
  }

  fn cfg(home: &Path) -> CharlieBotConfig {
    charliebot_core::config::load_config(home, &Default::default()).expect("the empty config loads")
  }

  #[test]
  fn a_real_run_writes_one_host_file_per_default_entry_and_the_rerun_reports_exists() {
    // Python: tests/test_setup_runner.py seeds through the registered step; the seed idempotence is
    // tests/test_cron_defaults.py::test_seed_idempotence.
    let repo = repo_with_defaults();
    let home = tempfile::tempdir().expect("a temp home");
    let cfg = cfg(home.path());

    let report = seed_default_cron_tasks(&cfg, repo.path(), false).expect("the first seed");
    assert_eq!(
      report.iter().map(|item| (item.name.as_str(), item.status)).collect::<Vec<_>>(),
      [("memory-curator", "created"), ("storage-cool", "created")]
    );
    let curator = loader::cron_path(&cfg, "memory-curator");
    let body = fs::read_to_string(&curator).expect("the seeded host file");
    assert!(body.contains("cron: 27 6 * * *"), "{body}");
    assert!(!body.lines().any(|line| line.starts_with("name:")), "the host file carries no name key: {body}");
    assert!(body.contains("prompt_file: prompts/cron/memory_curator/memory_selector.md"), "{body}");

    let report = seed_default_cron_tasks(&cfg, repo.path(), false).expect("the second seed");
    assert_eq!(
      report.iter().map(|item| (item.name.as_str(), item.status)).collect::<Vec<_>>(),
      [("memory-curator", "exists"), ("storage-cool", "exists")]
    );
  }

  #[test]
  fn a_dry_run_validates_and_writes_nothing() {
    let repo = repo_with_defaults();
    let home = tempfile::tempdir().expect("a temp home");
    let cfg = cfg(home.path());

    let report = seed_default_cron_tasks(&cfg, repo.path(), true).expect("the dry run");
    assert_eq!(
      report.iter().map(|item| (item.name.as_str(), item.status)).collect::<Vec<_>>(),
      [("memory-curator", "would-create"), ("storage-cool", "would-create")]
    );
    assert!(!loader::cron_dir(&cfg).exists(), "the dry run created no cron.d directory");
  }

  #[test]
  fn an_unreadable_prompt_file_fails_the_dry_run_and_the_real_run_alike() {
    let repo = repo_with_defaults();
    fs::remove_file(repo.path().join("prompts/cron/memory_curator/memory_selector.md"))
      .expect("remove the selector prompt");
    let home = tempfile::tempdir().expect("a temp home");
    let cfg = cfg(home.path());

    let error = seed_default_cron_tasks(&cfg, repo.path(), true).expect_err("the dry run fails loudly");
    assert!(error.contains("prompt_file"), "{error}");
    assert!(seed_default_cron_tasks(&cfg, repo.path(), false).is_err(), "the real run fails the same way");
    assert!(!loader::cron_dir(&cfg).exists(), "no partial write happened");
  }
}
