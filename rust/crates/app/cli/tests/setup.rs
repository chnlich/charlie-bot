//! The `setup` command, against a scratch home: the tests of `tests/test_setup_runner.py`. The runner calls each
//! registered step in registration order with the dry-run flag, and the packages register the cron seed before
//! the Claude Code check.

use std::path::Path;

use charliebot_shell_harness::{self as harness, Env, Run, Sandbox};

const OK_LINE: &str = "OK: backend disallows Monitor,ScheduleWakeup,CronCreate,CronDelete,CronList";
const CRON_BANNER: &str = "==> Seeding default cron tasks";
const CLAUDE_BANNER: &str = "==> Checking Claude Code backend tools";

/// The committed repo, whose `configs/cron.default.yaml` and prompt files the seed validates.
fn run_setup(sandbox: &Sandbox, home: &Path, dry_run: bool) -> Run {
  let mut env: Env = sandbox.env();
  env.insert(charliebot_core::home::CHARLIEBOT_HOME_ENV.to_string(), home.display().to_string());
  env.insert(charliebot_core::repo_root::REPO_ROOT_ENV.to_string(), harness::repo_root().display().to_string());
  let mut command = harness::command(env!("CARGO_BIN_EXE_charliebot"), &env);
  command.arg("setup");
  if dry_run {
    command.arg("--dry-run");
  }
  harness::run(&mut command)
}

fn banner_position(lines: &[&str], banner: &str) -> usize {
  lines.iter().position(|line| *line == banner).unwrap_or_else(|| panic!("{banner} did not run: {lines:?}"))
}

/// The home layout block's status lines, after the banner.
fn home_statuses<'a>(lines: &[&'a str]) -> Vec<&'a str> {
  lines
    .iter()
    .filter(|line| line.starts_with("  home "))
    .map(|line| line.split(": ").last().expect("a status"))
    .collect()
}

/// `test_the_packages_register_the_cron_and_claude_code_steps_in_that_order`: the cron seed runs before the
/// Claude Code check, in registration order.
#[test]
fn the_packages_register_the_cron_and_claude_code_steps_in_that_order() {
  let sandbox = Sandbox::new();
  let home = sandbox.dir("home");
  let run = run_setup(&sandbox, &home, true);

  assert_eq!(run.code, 0, "{}", run.describe());
  let lines: Vec<&str> = run.stdout.lines().collect();
  assert!(banner_position(&lines, CRON_BANNER) < banner_position(&lines, CLAUDE_BANNER), "{}", run.stdout);
  assert!(lines.contains(&OK_LINE), "{}", run.stdout);
}

/// `test_the_runner_calls_each_step_in_registration_order_with_the_dry_run_flag` (`--dry-run`): both steps run
/// in order, the config home is the scratch one, and nothing is written.
#[test]
fn the_runner_calls_each_step_in_registration_order_with_the_dry_run_flag() {
  let sandbox = Sandbox::new();
  let home = sandbox.dir("home");
  let run = run_setup(&sandbox, &home, true);

  assert_eq!(run.code, 0, "{}", run.describe());
  let lines: Vec<&str> = run.stdout.lines().collect();
  assert!(banner_position(&lines, CRON_BANNER) < banner_position(&lines, CLAUDE_BANNER), "{}", run.stdout);
  assert!(lines.contains(&OK_LINE), "{}", run.stdout);
  // A dry run writes nothing: the home layout step has not created config.yaml, and the cron seed created no
  // per-job host file.
  assert!(!home.join("config.yaml").exists());
  assert!(!home.join("config.d").exists());
  assert_eq!(home_statuses(&lines), ["exists", "would-create", "would-create", "would-create", "would-create"]);
  assert!(lines.contains(&"  cron memory-curator: would-create"), "{}", run.stdout);
  assert!(lines.contains(&"  cron storage-cool: would-create"), "{}", run.stdout);
  // No host cron file exists yet, so the effective task list is empty.
  assert!(lines.contains(&"  effective scheduled tasks:") && lines.contains(&"    (none)"), "{}", run.stdout);
}

/// `test_the_runner_calls_each_step_in_registration_order_with_the_dry_run_flag` (no flag): the same order, and
/// every call writes into the scratch home the command was given.
#[test]
fn the_runner_calls_each_step_in_registration_order_and_writes_the_scratch_home_in_a_real_run() {
  let sandbox = Sandbox::new();
  let home = sandbox.dir("home");
  let run = run_setup(&sandbox, &home, false);

  assert_eq!(run.code, 0, "{}", run.describe());
  let lines: Vec<&str> = run.stdout.lines().collect();
  assert!(banner_position(&lines, CRON_BANNER) < banner_position(&lines, CLAUDE_BANNER), "{}", run.stdout);
  assert!(lines.contains(&OK_LINE), "{}", run.stdout);
  // The layout lands in the scratch home, seeded from the checkout's templates.
  assert!(home.join("config.yaml").is_file());
  assert!(home.join("credentials.yaml").is_file());
  assert!(home.join("sessions").is_dir());
  assert_eq!(home_statuses(&lines), ["exists", "created", "created", "created", "created"]);
  // The cron seed created one host file per default entry, and the same run lists them as effective tasks.
  assert!(home.join("config.d/cron.d/memory-curator.yaml").is_file());
  assert!(home.join("config.d/cron.d/storage-cool.yaml").is_file());
  assert!(lines.contains(&"  cron memory-curator: created"), "{}", run.stdout);
  assert!(lines.contains(&"  cron storage-cool: created"), "{}", run.stdout);
  let effective =
    lines.iter().position(|line| *line == "  effective scheduled tasks:").expect("the effective task list");
  let curator = lines[effective..]
    .iter()
    .find(|line| line.starts_with("    - memory-curator | cron=27 6 * * * | tz="))
    .expect("memory-curator is listed");
  assert!(curator.contains("| backend="), "{curator}");
  assert!(lines.iter().any(|line| line.starts_with("    - storage-cool | cron=30 4 * * * | tz=")));
}

/// A repo whose default cron entry points at a missing prompt file fails the run before anything is written,
/// in the dry run and the real run alike (`seed_default_cron_tasks` validates every entry before writing).
#[test]
fn an_unreadable_default_prompt_fails_the_setup_before_any_write() {
  let sandbox = Sandbox::new();
  let repo = sandbox.dir("repo");
  let configs = sandbox.dir("repo/configs");
  let defaults = std::fs::read_to_string(harness::repo_root().join("configs/cron.default.yaml")).expect("the defaults");
  std::fs::write(configs.join("cron.default.yaml"), defaults).expect("write the defaults");
  let prompt_dir = sandbox.dir("repo/prompts/cron/memory_curator");
  std::fs::write(prompt_dir.join("memory_reviewer.md"), "Synthetic prompt\n").expect("write the reviewer prompt");
  // memory_selector.md stays missing, so the first entry fails validation.

  let home = sandbox.dir("home");
  let mut env: Env = sandbox.env();
  env.insert(charliebot_core::home::CHARLIEBOT_HOME_ENV.to_string(), home.display().to_string());
  env.insert(charliebot_core::repo_root::REPO_ROOT_ENV.to_string(), repo.display().to_string());
  let mut command = harness::command(env!("CARGO_BIN_EXE_charliebot"), &env);
  command.arg("setup").arg("--dry-run");
  let run = harness::run(&mut command);

  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(run.stderr.contains("prompt_file"), "{}", run.stderr);
  assert!(!home.join("config.d").exists(), "no partial write happened");
}

/// The committed defaults seed byte-identical host files to the Python seed's `yaml_utils.save_yaml` output.
#[test]
fn the_seeded_host_files_match_the_recorded_bytes() {
  let sandbox = Sandbox::new();
  let home = sandbox.dir("home");
  let run = run_setup(&sandbox, &home, false);

  assert_eq!(run.code, 0, "{}", run.describe());
  for (name, expected) in [
    ("memory-curator", include_str!("fixtures/setup_seed/memory-curator_golden.yaml")),
    ("storage-cool", include_str!("fixtures/setup_seed/storage-cool_golden.yaml")),
  ] {
    let seeded = std::fs::read_to_string(home.join("config.d/cron.d").join(format!("{name}.yaml")))
      .unwrap_or_else(|error| panic!("read the seeded {name}: {error}"));
    assert_eq!(seeded, expected, "the seeded {name} does not match the recorded Python bytes");
  }
}
