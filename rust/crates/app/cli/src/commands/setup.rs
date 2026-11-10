//! The `setup` command: provision the `$CHARLIEBOT_HOME` layout, then run the registered
//! setup steps — the cron seed, then the Claude Code check — in registration order. `--dry-run` writes nothing:
//! the home block reports what exists and what it would create, and each step runs in its dry form.

use std::ffi::OsString;

use charliebot_cli_common::args as cli_args;
use charliebot_cli_common::exit::CliExit;
use charliebot_core::home;
use charliebot_core::home_init;
use charliebot_core::repo_root;
use clap::Parser;

#[derive(Parser)]
#[command(name = "setup", about = "Provision the profile home and run the registered setup steps")]
struct SetupCli {
  /// Write nothing; print what each step would do
  #[arg(long)]
  dry_run: bool,
}

/// Parse the arguments and run the setup flow. Parser errors print and exit on their own
/// (help exits 0, a usage error exits 2).
pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  let cli: SetupCli = cli_args::parse("charliebot setup", args);
  if let Err(message) = run(cli.dry_run) {
    eprintln!("{message}");
    std::process::exit(1);
  }
  Ok(())
}

/// The setup flow: provision the home, then the steps in registration order. A step failure exits 1 with its
/// message on stderr.
fn run(dry_run: bool) -> Result<(), String> {
  let registry = charliebot_packages::registry().map_err(|error| error.to_string())?;
  let repo = repo_root::RepoRoot::new(std::env::var_os(repo_root::REPO_ROOT_ENV)).map_err(|error| error.to_string())?;
  let home_dir = home::charliebot_home_dir().map_err(|error| error.to_string())?;
  let cfg = registry.load_config(&home_dir).map_err(|error| error.to_string())?;

  provision_home(&cfg, repo.path(), dry_run)?;
  println!("  Reminder: fill in the secret key charliebot_access_key before first start.");
  // The provisioning seeds config.yaml on a fresh home; the steps read the config the layout now holds,
  // reloaded after provisioning so the cron listing sees the written file. A dry run
  // writes nothing, so its config is the one it loaded.
  let cfg = if dry_run { cfg } else { registry.load_config(&home_dir).map_err(|error| error.to_string())? };
  charliebot_cron::seed::setup_step(&cfg, repo.path(), dry_run)?;
  charliebot_claude_code::setup_check::setup_step()?;
  Ok(())
}

/// The home layout block: per-item created/exists for the layout
/// [`charliebot_core::home_init::init_charliebot_home`] provisions — the same path the server runs at startup.
/// In a dry run nothing is written and an absent item reads `would-create`; otherwise the run created each item
/// that did not exist before. The memory store creates its own scaffold at first use, so setup leaves the
/// memory directory alone.
fn provision_home(
  cfg: &charliebot_core::config::CharlieBotConfig,
  repo: &std::path::Path,
  dry_run: bool,
) -> Result<(), String> {
  println!("==> Provisioning {}", home::DISPLAY_HOME);
  let home_items = [
    ("dir", home::display_dir(""), cfg.charliebot_home.clone()),
    ("dir", home::display_dir(charliebot_core::config::SESSIONS_DIR_NAME), cfg.sessions_dir()),
    ("dir", home::display_dir(charliebot_core::config::CONFIG_D_DIR_NAME), cfg.config_d_dir()),
    ("file", home::display_path(charliebot_core::config::CONFIG_FILENAME), cfg.config_file()),
    ("file", home::display_path(charliebot_core::config::CREDENTIALS_FILENAME), cfg.credentials_file()),
  ];
  let existed_before: Vec<bool> = home_items.iter().map(|(_, _, path)| path.exists()).collect();
  if !dry_run {
    home_init::init_charliebot_home(cfg, repo)
      .map_err(|error| format!("{}: {error}", cfg.charliebot_home.display()))?;
  }
  for ((_, label, path), existed) in home_items.iter().zip(existed_before) {
    let status = if dry_run {
      if path.exists() { home_init::SEED_EXISTS } else { home_init::SEED_WOULD_CREATE }
    } else if existed {
      home_init::SEED_EXISTS
    } else {
      home_init::SEED_CREATED
    };
    println!("  home {label}: {status}");
  }
  Ok(())
}
