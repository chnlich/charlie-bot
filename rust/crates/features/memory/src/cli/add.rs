use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use charliebot_cli_common::exit::{CliExit, charliebot_home_or_exit};
use charliebot_core::{config, str_methods, timefmt};
use clap::Args;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::error::exit_error;
use super::store;
use crate::store_root;

const TEST_NOW_ENV: &str = "CHARLIEBOT_MEMORY_CLI_TEST_NOW";

#[derive(Args)]
pub(super) struct AddArgs {
  /// Read the capture body from a file. Stdin is used by default.
  #[arg(long)]
  file: Option<PathBuf>,
}

pub(super) fn add(args: &AddArgs) -> Result<(), CliExit> {
  let body = match &args.file {
    Some(path) => store::read_text(path)?,
    None => {
      let mut body = String::new();
      io::stdin().read_to_string(&mut body).map_err(|error| CliExit::error(error.to_string()))?;
      str_methods::universal_newlines(&body)
    }
  };
  let first_line = body.split('\n').next().unwrap_or_default();
  if !first_line.starts_with("# ") {
    exit_error("error: body must start with '# <title>'");
  }
  let title = str_methods::strip(&first_line[2..]);
  if title.is_empty() {
    exit_error("error: empty title after '# '");
  }
  let home_dir = charliebot_home_or_exit()?;
  let cwd = std::env::current_dir().map_err(|error| CliExit::error(error.to_string()))?;
  let memory_dir = store_root::memory_dir_in(&home_dir);
  let target = write_capture(&memory_dir, &config::sessions_dir_in(&home_dir), &cwd, title, &body, cli_clock)?;
  println!("{}", target.display());
  Ok(())
}

pub(super) fn write_capture(
  memory_dir: &Path,
  sessions_root: &Path,
  cwd: &Path,
  title: &str,
  body: &str,
  clock: impl FnOnce() -> Result<OffsetDateTime, CliExit>,
) -> Result<PathBuf, CliExit> {
  crate::memory::ensure_store(memory_dir).map_err(|error| CliExit::error(error.to_string()))?;
  let timestamp = format!("{}Z", timefmt::compact_utc(clock()?));
  let session_slug = session_slug8(sessions_root, cwd);
  let slug = slugify(title);
  let slug = if slug.is_empty() { "capture" } else { &slug };
  let filename = format!("{timestamp}-{session_slug}-{slug}.md");
  let target = memory_dir.join(crate::memory::STAGING_DIRNAME).join(filename);
  fs::write(&target, body).map_err(|error| CliExit::error(format!("write {}: {error}", target.display())))?;
  Ok(target)
}

fn cli_clock() -> Result<OffsetDateTime, CliExit> {
  match std::env::var(TEST_NOW_ENV) {
    Ok(value) => {
      OffsetDateTime::parse(&value, &Rfc3339).map_err(|error| CliExit::error(format!("{TEST_NOW_ENV}: {error}")))
    }
    Err(std::env::VarError::NotPresent) => Ok(OffsetDateTime::now_utc()),
    Err(error) => Err(CliExit::error(format!("{TEST_NOW_ENV}: {error}"))),
  }
}

pub(super) fn session_slug8(sessions_root: &Path, cwd: &Path) -> String {
  // The comparison runs against the canonical sessions root: a symlinked CHARLIEBOT_HOME must not hide
  // the session directory, since the cwd a process reads is already symlink-free.
  let sessions_root = &fs::canonicalize(sessions_root).unwrap_or_else(|_| sessions_root.to_path_buf());
  if cwd.parent() == Some(sessions_root) {
    cwd.file_name().map(|name| name.to_string_lossy().chars().take(8).collect()).unwrap_or_else(|| "nosess".to_string())
  } else {
    "nosess".to_string()
  }
}

fn slugify(title: &str) -> String {
  let mut slug = String::new();
  let mut previous_dash = false;
  for character in title.to_lowercase().chars() {
    if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
      if character == '-' && previous_dash {
        continue;
      }
      slug.push(character);
      previous_dash = character == '-';
    } else if !previous_dash {
      slug.push('-');
      previous_dash = true;
    }
  }
  slug.trim_matches('-').to_string()
}
