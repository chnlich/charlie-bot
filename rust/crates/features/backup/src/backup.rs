//! The profile backup: restic snapshots of the CharlieBot home in a restic repository, a tiered
//! retention policy, and the restore entry point.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, bail};
use charliebot_core::config::{CREDENTIALS_FILENAME, SESSIONS_DIR_NAME};
use charliebot_core::home::OPENCODE_DIR_NAME;
use charliebot_store::run_records::THREADS_DIR_NAME;
use serde_json::Value;

/// The restic executable the preflight resolves on `PATH`.
const RESTIC: &str = "restic";

/// The exit code of `restic backup` when the snapshot was written but some source file could not be
/// read ("at least one source file could not be read").
const RESTIC_PARTIAL_BACKUP: i32 = 3;

/// The state directory being backed up: this profile's home (`charliebot_core::home::charliebot_home_dir`).
pub fn charliebot_dir() -> anyhow::Result<PathBuf> {
  Ok(charliebot_core::home::charliebot_home_dir()?)
}

/// The one exclusion list of the backup. Restic excludes a matching directory with everything under
/// it and matches an unanchored pattern at any depth of the source tree: the names hold for every
/// depth, the `sessions/*/threads` shape is every session's thread store, and the one anchored
/// pattern, on the home's absolute path, is the profile's opencode store at the top level only.
pub fn exclude_patterns(anchor: &Path) -> Vec<String> {
  vec![
    format!("{}/{OPENCODE_DIR_NAME}", anchor.display()),
    ".git".to_string(),
    ".claude".to_string(),
    CREDENTIALS_FILENAME.to_string(),
    "__pycache__".to_string(),
    "*.pyc".to_string(),
    format!("{SESSIONS_DIR_NAME}/*/{THREADS_DIR_NAME}"),
  ]
}

/// The restic command for the repository: the repository URL and the password file ride the flags, so
/// no environment carries them.
fn restic(repository: &str, password_file: &Path) -> Command {
  let mut command = Command::new(RESTIC);
  command.arg("-r").arg(repository).arg("--password-file").arg(password_file);
  command
}

/// Runs one restic command and returns its stdout. A failure carries the command and stderr's tail,
/// which is where restic writes its causes.
fn run(command: &mut Command) -> anyhow::Result<String> {
  let output = run_accepting(command, &[0])?;
  Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Runs one restic command and returns its output when its exit code is one of `accepted`; any other
/// exit is a failure, reported as in [`run`].
fn run_accepting(command: &mut Command, accepted: &[i32]) -> anyhow::Result<Output> {
  let output = command.output().with_context(|| format!("running {}", command.get_program().to_string_lossy()))?;
  if !output.status.code().is_some_and(|code| accepted.contains(&code)) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let lines: Vec<&str> = stderr.lines().collect();
    let tail = lines[lines.len().saturating_sub(8)..].join("; ");
    bail!("restic {} failed ({}): {}", command_args(command), output.status, tail);
  }
  Ok(output)
}

/// The command's words after the executable, for error messages.
fn command_args(command: &Command) -> String {
  command.get_args().map(|argument| argument.to_string_lossy().into_owned()).collect::<Vec<_>>().join(" ")
}

/// The preflight the backup handler and the restore command share: restic resolvable on `PATH` and
/// the password file present.
pub fn preflight(password_file: &Path) -> anyhow::Result<()> {
  find_restic()?;
  if !password_file.exists() {
    bail!("the restic password file is missing: {}", password_file.display());
  }
  Ok(())
}

/// The first `PATH` entry holding an executable-looking `restic`.
fn find_restic() -> anyhow::Result<PathBuf> {
  let path = std::env::var("PATH").unwrap_or_default();
  for directory in path.split(':').filter(|directory| !directory.is_empty()) {
    let candidate = Path::new(directory).join(RESTIC);
    if candidate.is_file() {
      return Ok(candidate);
    }
  }
  bail!("restic was not found on PATH")
}

/// Removes the repository's stale locks: `restic unlock`, never with `--remove-all`, so a lock a
/// killed run left behind goes while a live run keeps its own. A server stop can kill the backup
/// handler's restic process, and its leftover lock makes the next `restic forget` refuse the
/// repository.
pub fn remove_stale_locks(repository: &str, password_file: &Path) -> anyhow::Result<()> {
  let mut command = restic(repository, password_file);
  command.arg("unlock");
  run(&mut command)?;
  Ok(())
}

/// Runs `restic backup` of `home` into the repository and returns the new snapshot's id. The source
/// is `.` so the snapshot's root is the home itself, and the anchored exclusion pattern rides the
/// home's canonical path, the path restic resolves the source to. A source file restic cannot read
/// (restic's exit 3) leaves the snapshot of the rest in place: the unreadable files are logged, and
/// the run goes on as the tar writer's skipped entries did.
pub fn create_snapshot(home: &Path, repository: &str, password_file: &Path) -> anyhow::Result<String> {
  let anchor = std::fs::canonicalize(home).with_context(|| format!("resolving the home {}", home.display()))?;
  let mut command = restic(repository, password_file);
  command.current_dir(home).arg("backup").arg(".").arg("--json");
  for pattern in exclude_patterns(&anchor) {
    command.arg("--exclude").arg(pattern);
  }
  let output = run_accepting(&mut command, &[0, RESTIC_PARTIAL_BACKUP])?;
  let stdout = String::from_utf8_lossy(&output.stdout);
  if output.status.code() == Some(RESTIC_PARTIAL_BACKUP) {
    let unreadable: Vec<String> = stdout
      .lines()
      .filter_map(|line| serde_json::from_str::<Value>(line).ok())
      .filter(|value| value.get("message_type").and_then(Value::as_str) == Some("error"))
      .map(|value| value.to_string())
      .collect();
    tracing::warn!(count = unreadable.len(), details = %unreadable.join(" "), "backup_source_partially_read");
  }
  snapshot_id(&stdout)
}

/// The `snapshot_id` of the backup's summary line, the last JSON line `--json` prints.
pub(crate) fn snapshot_id(backup_output: &str) -> anyhow::Result<String> {
  for line in backup_output.lines().rev() {
    let Ok(value) = serde_json::from_str::<Value>(line) else { continue };
    if value.get("message_type").and_then(Value::as_str) == Some("summary") {
      return value
        .get("snapshot_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .context("the restic backup summary carries no snapshot_id");
    }
  }
  bail!("restic backup printed no summary line")
}

/// Runs the retention policy: `restic forget --keep-daily 3 --keep-within-weekly 15d --prune` — the
/// last three calendar days, plus each ISO week's last snapshot within fifteen days, pruned.
pub fn apply_retention(repository: &str, password_file: &Path) -> anyhow::Result<()> {
  let mut command = restic(repository, password_file);
  command.args(["forget", "--keep-daily", "3", "--keep-within-weekly", "15d", "--prune"]);
  run(&mut command)?;
  Ok(())
}

/// The restore preflight: restic on `PATH`, the password file present, the target an empty directory,
/// and the repository reachable, each failing with its cause.
pub fn restore_preflight(repository: &str, password_file: &Path, target: &Path) -> anyhow::Result<()> {
  preflight(password_file)?;
  let mut entries =
    std::fs::read_dir(target).with_context(|| format!("reading the restore target {}", target.display()))?;
  if entries.next().is_some() {
    bail!("the restore target is not empty: {}", target.display());
  }
  let mut command = restic(repository, password_file);
  command.args(["snapshots", "--latest", "1"]);
  run(&mut command).with_context(|| format!("the restic repository is unreachable: {repository}"))?;
  Ok(())
}

/// Restores `snapshot` (`latest` or an id) of the repository into the empty directory `target`.
pub fn restore(repository: &str, password_file: &Path, snapshot: &str, target: &Path) -> anyhow::Result<()> {
  restore_preflight(repository, password_file, target)?;
  let mut command = restic(repository, password_file);
  command.arg("restore").arg(snapshot).arg("--target").arg(target);
  run(&mut command)?;
  Ok(())
}
