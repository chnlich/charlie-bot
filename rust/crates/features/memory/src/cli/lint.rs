use std::path::PathBuf;
use std::process;

use charliebot_cli_common::exit::CliExit;
use charliebot_core::home;
use clap::Args;

use super::store;
use crate::store_root;

#[derive(Args)]
pub(super) struct LintArgs {
  /// Store root to validate.
  #[arg(long)]
  dir: Option<PathBuf>,
}

pub(super) fn lint(args: &LintArgs) -> Result<(), CliExit> {
  let root = match args.dir.as_deref().filter(|path| !path.as_os_str().is_empty()) {
    Some(path) => PathBuf::from(charliebot_core::paths::expanduser(&path.to_string_lossy())),
    None => store_root::memory_dir_in(&home::charliebot_home_dir().map_err(|error| CliExit::error(error.to_string()))?),
  };
  let (violations, warnings) = store::lint_store(&root)?;
  for warning in warnings {
    eprintln!("warning: {warning}");
  }
  if violations.is_empty() {
    println!("clean");
  } else {
    for violation in violations {
      println!("{violation}");
    }
    process::exit(1);
  }
  Ok(())
}
