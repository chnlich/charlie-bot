//! The `setup` command (`src/app/setup.py`): provision the `$CHARLIEBOT_HOME` layout, then run the registered
//! setup steps — the cron seed, then the Claude Code check — in registration order. `--dry-run` writes nothing:
//! the home block reports what exists and what it would create, and each step runs in its dry form.
//!
//! Python's `--step MODULE:ATTR` re-entry (one registered function in a fresh interpreter after `uv sync`) has no
//! Rust form: the CLI has no interpreter to refresh, and no caller exists.

use std::ffi::OsString;

use charliebot_cli_common::args::with_infer_long_args;
use charliebot_cli_common::exit::CliExit;
use charliebot_core::home;
use charliebot_core::repo_root;
use clap::{CommandFactory, FromArgMatches, Parser};

#[derive(Parser)]
#[command(name = "setup", about = "Provision the profile home and run the registered setup steps")]
struct SetupCli {
  /// Write nothing; print what each step would do
  #[arg(long)]
  dry_run: bool,
}

/// Parse the arguments and run the setup flow. Parser errors print and exit on their own
/// (argparse-compatible: help 0, usage error 2).
pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  let command = with_infer_long_args(SetupCli::command());
  let argv = std::iter::once(OsString::from("charliebot setup")).chain(args.iter().cloned());
  let matches = match command.try_get_matches_from(argv) {
    Ok(matches) => matches,
    Err(error) => error.exit(),
  };
  let cli = SetupCli::from_arg_matches(&matches).expect("try_get_matches_from validated the arguments");
  if let Err(message) = run(cli.dry_run) {
    eprintln!("{message}");
    std::process::exit(1);
  }
  Ok(())
}

/// The setup flow: provision the home, then the steps in registration order. A step failure exits 1 with its
/// message on stderr, the way Python's `SystemExit` did.
fn run(dry_run: bool) -> Result<(), String> {
  let registry = charliebot_packages::registry().map_err(|error| error.to_string())?;
  let repo = repo_root::RepoRoot::new(std::env::var_os(repo_root::REPO_ROOT_ENV)).map_err(|error| error.to_string())?;
  let home_dir = home::charliebot_home_dir().map_err(|error| error.to_string())?;
  let cfg = registry.load_config(&home_dir).map_err(|error| error.to_string())?;

  provision_home(&cfg, repo.path(), dry_run)?;
  println!("  Reminder: fill in the secret key charliebot_access_key before first start.");
  // The provisioning seeds config.yaml on a fresh home; the steps read the config the layout now holds,
  // the way Python's process-wide cache hot-reloads the written file before the cron listing. A dry run
  // writes nothing, so its config is the one it loaded.
  let cfg = if dry_run { cfg } else { registry.load_config(&home_dir).map_err(|error| error.to_string())? };
  charliebot_cron::seed::setup_step(&cfg, repo.path(), dry_run)?;
  charliebot_claude_code::setup_check::setup_step()?;
  Ok(())
}

/// The home layout block (`_provision_home`): per-item created/exists for the layout
/// [`charliebot_core::home_init::init_charliebot_home`] provisions — the same path the server runs at startup.
/// In a dry run nothing is written and an absent item reads `would-create`; otherwise the run created each item
/// that did not exist before. The memory store creates its own scaffold at first use, so setup leaves the
/// memory directory alone.
fn provision_home(
  cfg: &charliebot_core::config::CharlieBotConfig,
  repo: &std::path::Path,
  dry_run: bool,
) -> Result<(), String> {
  let home_env = home::CHARLIEBOT_HOME_ENV;
  println!("==> Provisioning ${home_env}");
  let home_items = [
    ("dir", format!("${home_env}/"), cfg.charliebot_home.clone()),
    ("dir", format!("${home_env}/{}/", charliebot_core::config::SESSIONS_DIR_NAME), cfg.sessions_dir()),
    ("dir", format!("${home_env}/{}/", charliebot_core::config::CONFIG_D_DIR_NAME), cfg.config_d_dir()),
    ("file", format!("${home_env}/{}", charliebot_core::config::CONFIG_FILENAME), cfg.config_file()),
    ("file", format!("${home_env}/{}", charliebot_core::config::CREDENTIALS_FILENAME), cfg.credentials_file()),
  ];
  let existed_before: Vec<bool> = home_items.iter().map(|(_, _, path)| path.exists()).collect();
  if !dry_run {
    charliebot_core::home_init::init_charliebot_home(cfg, repo)
      .map_err(|error| format!("{}: {error}", cfg.charliebot_home.display()))?;
  }
  for ((_, label, path), existed) in home_items.iter().zip(existed_before) {
    let status = if dry_run {
      if path.exists() { "exists" } else { "would-create" }
    } else if existed {
      "exists"
    } else {
      "created"
    };
    println!("  home {label}: {status}");
  }
  Ok(())
}
