//! The profile backup: a restic snapshot of the CharlieBot home in the configured repository, pruned by the module's
//! retention policy.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, anyhow, bail};
use charliebot_core::config::{CREDENTIALS_FILENAME, SESSIONS_DIR_NAME};
use charliebot_core::home::OPENCODE_DIR_NAME;
use charliebot_store::run_records::THREADS_DIR_NAME;

use crate::config::ResticSettings;

/// The retention policy every run applies after its snapshot: three daily snapshots plus the last snapshot of each
/// week within 15 days, relative to the newest snapshot.
pub const RETENTION_ARGS: &[&str] = &["--keep-daily", "3", "--keep-within-weekly", "15d", "--prune"];

/// The snapshot's exclusions, as restic patterns. A pattern without a slash matches that name at any depth, so the
/// secrets file, `.git`, `.claude`, bytecode and caches are left out wherever they sit.
pub fn exclude_patterns(home: &Path) -> Vec<String> {
  let home = home.to_string_lossy();
  vec![
    // The profile's opencode store is the top-level directory only; a deeper part with the same name stays.
    format!("{home}/{OPENCODE_DIR_NAME}"),
    ".git".to_string(),
    ".claude".to_string(),
    // CREDENTIALS_FILENAME is the profile's secrets file; it must never ride a snapshot.
    CREDENTIALS_FILENAME.to_string(),
    "__pycache__".to_string(),
    "*.pyc".to_string(),
    // Every session's thread directory and everything under it.
    format!("{home}/{SESSIONS_DIR_NAME}/*/{THREADS_DIR_NAME}"),
  ]
}

/// The restic executable on PATH.
pub fn restic_program() -> anyhow::Result<PathBuf> {
  let path = std::env::var_os("PATH").ok_or_else(|| anyhow!("PATH is unset, so restic cannot be found"))?;
  for directory in std::env::split_paths(&path) {
    let candidate = directory.join("restic");
    if let Ok(metadata) = fs::metadata(&candidate)
      && metadata.is_file()
      && metadata.permissions().mode() & 0o111 != 0
    {
      return Ok(candidate);
    }
  }
  bail!("restic is not on PATH")
}

impl ResticSettings {
  /// The restic executable, after a preflight that asserts restic on PATH, a readable password file and a reachable
  /// repository. Nothing here writes a snapshot.
  pub fn preflight(&self) -> anyhow::Result<PathBuf> {
    let program = restic_program()?;
    self.check_password_file()?;
    let output = self
      .command(&program)
      .arg("cat")
      .arg("config")
      .arg("--no-cache")
      .output()
      .with_context(|| format!("running {}", program.display()))?;
    checked(output, &format!("opening the restic repository {}", self.repository))?;
    Ok(program)
  }

  /// A preflight, then `restic backup` with [`exclude_patterns`], then the retention pass.
  pub fn backup(&self, home: &Path) -> anyhow::Result<()> {
    let program = self.preflight()?;
    let mut command = self.command(&program);
    command.arg("backup").arg(home);
    for pattern in exclude_patterns(home) {
      command.arg("--exclude").arg(pattern);
    }
    checked(command.output().with_context(|| format!("running {}", program.display()))?, "restic backup")?;
    tracing::info!(repository = %self.repository, "backup_snapshot_created");
    self.forget()
  }

  /// The retention pass: forget by [`RETENTION_ARGS`] and prune the repository.
  pub fn forget(&self) -> anyhow::Result<()> {
    let program = restic_program()?;
    let mut command = self.command(&program);
    command.arg("forget").args(RETENTION_ARGS);
    checked(command.output().with_context(|| format!("running {}", program.display()))?, "restic forget")?;
    tracing::info!(repository = %self.repository, "backup_retention_applied");
    Ok(())
  }

  /// A preflight, then `restic restore` of `snapshot` into the empty directory `target`. The selection is the
  /// snapshot's subtree of `home`, so the target holds the home's contents rather than the home path.
  pub fn restore(&self, home: &Path, snapshot: &str, target: &Path) -> anyhow::Result<()> {
    let program = self.preflight()?;
    if !target.is_dir() {
      bail!("the restore target {} is not a directory", target.display());
    }
    let mut entries =
      fs::read_dir(target).with_context(|| format!("reading the restore target {}", target.display()))?;
    if entries.next().is_some() {
      bail!("the restore target {} is not empty", target.display());
    }
    let selection = format!("{snapshot}:{}", home.display());
    let output = self
      .command(&program)
      .arg("restore")
      .arg(&selection)
      .arg("--target")
      .arg(target)
      .output()
      .with_context(|| format!("running {}", program.display()))?;
    checked(output, "restic restore")?;
    Ok(())
  }

  /// A restic command against the configured repository, authenticated by its password file.
  fn command(&self, program: &Path) -> Command {
    let mut command = Command::new(program);
    command.arg("--repo").arg(&self.repository).arg("--password-file").arg(&self.password_file);
    command
  }
}

/// The output of a restic call, or the reason restic gave for failing.
fn checked(output: Output, action: &str) -> anyhow::Result<Output> {
  if output.status.success() {
    return Ok(output);
  }
  let reason = String::from_utf8_lossy(&output.stderr);
  let reason = reason.trim();
  match output.status.code() {
    Some(code) => bail!("{action} failed (restic exit {code}): {reason}"),
    None => bail!("{action} failed (restic terminated by a signal): {reason}"),
  }
}
