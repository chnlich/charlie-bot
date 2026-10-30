//! The `gc-trash` command (`src/runtime/cli/gc_trash.py`): inspect and (only with `--yes`) purge
//! the worktree quarantine trash.
//!
//! The startup sweep moves stale failed worktrees into `<paths.worktree_dir>/.trash/`; nothing
//! hard-deletes them automatically. This command lists what is there — path, age, size — and, only
//! with `--yes`, removes every entry. A failed deletion prints to stderr and exits 1 after the
//! remaining entries; root-owned entries are reported for a manual `sudo rm`, never escalated.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use charliebot_agents::cli_host::LazyHost;
use charliebot_cli_common::args::with_infer_long_args;
use charliebot_cli_common::exit::CliExit;
use charliebot_core::git::TRASH_DIR_NAME;
use charliebot_core::paths;
use charliebot_core::text_repr::format_f;
use clap::{CommandFactory, FromArgMatches, Parser};

#[derive(Parser)]
#[command(name = "gc-trash", about = "List or purge the CharlieBot worktree quarantine trash")]
struct GcTrashCli {
  /// Actually hard-delete every entry. Without it, this is a dry-run that deletes nothing.
  #[arg(long)]
  yes: bool,
}

/// One top-level entry sitting in the quarantine trash.
struct TrashEntry {
  path: PathBuf,
  age_days: f64,
  size_bytes: u64,
}

/// Parse the arguments and run one sweep. Parser errors print and exit on their own
/// (argparse-compatible: help 0, usage error 2).
pub fn main(args: &[OsString], lazy_host: &LazyHost) -> Result<(), CliExit> {
  let command = with_infer_long_args(GcTrashCli::command());
  let argv = std::iter::once(OsString::from("charliebot gc-trash")).chain(args.iter().cloned());
  let matches = match command.try_get_matches_from(argv) {
    Ok(matches) => matches,
    Err(error) => error.exit(),
  };
  let cli = GcTrashCli::from_arg_matches(&matches).expect("try_get_matches_from validated the arguments");
  let host = lazy_host.get().map_err(|error| CliExit::error(format!("{error:#}")))?;
  let worktree_dir = paths::expanduser(&host.cfg.paths.worktree_dir);
  run(&PathBuf::from(worktree_dir).join(TRASH_DIR_NAME), cli.yes)
}

fn run(trash_path: &Path, yes: bool) -> Result<(), CliExit> {
  let entries = list_trash_entries(trash_path, SystemTime::now())?;

  if entries.is_empty() {
    println!("Quarantine trash is empty: {}", trash_path.display());
    return Ok(());
  }

  let total: u64 = entries.iter().map(|entry| entry.size_bytes).sum();
  for entry in &entries {
    println!("{}  age={}d  size={}", entry.path.display(), format_f(entry.age_days, 1), format_size(entry.size_bytes));
  }
  println!(
    "\n{} {}, {} total in {}",
    entries.len(),
    if entries.len() == 1 { "entry" } else { "entries" },
    format_size(total),
    trash_path.display()
  );

  if !yes {
    println!("Dry run — nothing deleted. Re-run with --yes to hard-delete.");
    return Ok(());
  }

  if purge(&entries) {
    std::process::exit(1);
  }
  Ok(())
}

/// `worktree_trash.list_trash_entries`: every top-level child of the trash directory with its age
/// (since the entry's own last modification, never negative) and recursive size.
fn list_trash_entries(trash_path: &Path, now: SystemTime) -> Result<Vec<TrashEntry>, CliExit> {
  if !trash_path.is_dir() {
    return Ok(Vec::new());
  }
  let now_seconds = unix_seconds(now);
  let mut children: Vec<PathBuf> = std::fs::read_dir(trash_path)
    .map_err(|error| CliExit::error(os_error(&paths::display(trash_path), &error)))?
    .map(|entry| entry.map(|entry| entry.path()))
    .collect::<Result<_, _>>()
    .map_err(|error| CliExit::error(os_error(&paths::display(trash_path), &error)))?;
  children.sort_by(|first, second| first.as_os_str().cmp(second.as_os_str()));
  let mut entries = Vec::new();
  for child in children {
    let metadata =
      std::fs::symlink_metadata(&child).map_err(|error| CliExit::error(os_error(&paths::display(&child), &error)))?;
    let age_seconds = now_seconds - modified_seconds(&metadata);
    entries.push(TrashEntry {
      size_bytes: dir_size_bytes(&child),
      path: child,
      age_days: (age_seconds / 86_400.0).max(0.0),
    });
  }
  Ok(entries)
}

/// `worktree_trash.dir_size_bytes`: every non-directory entry's own size; unreadable entries are
/// skipped, as the Python walker skips them with a warning. A symlink is counted when its target
/// is not a directory, matching `os.walk(followlinks=False)`.
fn dir_size_bytes(path: &Path) -> u64 {
  let Ok(children) = std::fs::read_dir(path) else { return 0 };
  let mut total = 0;
  for child in children.flatten() {
    let Ok(file_type) = child.file_type() else { continue };
    if file_type.is_dir() {
      total += dir_size_bytes(&child.path());
    } else if file_type.is_symlink()
      && std::fs::metadata(child.path()).map(|metadata| metadata.is_dir()).unwrap_or(false)
    {
      continue;
    } else if let Ok(metadata) = child.metadata() {
      total += metadata.len();
    }
  }
  total
}

/// `shutil.rmtree` over every entry; `true` when any deletion failed. Only entries under the trash
/// directory are removed; the trash directory itself stays.
fn purge(entries: &[TrashEntry]) -> bool {
  let mut failed = false;
  for entry in entries {
    match std::fs::remove_dir_all(&entry.path) {
      Ok(()) => println!("deleted {}", entry.path.display()),
      Err(error) => {
        failed = true;
        if error.kind() == std::io::ErrorKind::PermissionDenied {
          eprintln!(
            "PERMISSION DENIED: {} ({}); contains root-owned files — remove manually with: sudo rm -rf {}",
            entry.path.display(),
            os_error(&entry.path.display().to_string(), &error),
            entry.path.display()
          );
        } else {
          eprintln!(
            "FAILED to delete {}: {}",
            entry.path.display(),
            os_error(&entry.path.display().to_string(), &error)
          );
        }
      }
    }
  }
  failed
}

/// `human_size.format_size`: `"5 B"`, `"2.0 KB"`, and upward with one decimal above bytes.
fn format_size(size: u64) -> String {
  let mut size = size as f64;
  for unit in ["B", "KB", "MB", "GB", "TB"] {
    if size < 1024.0 {
      return if unit == "B" { format!("{} {unit}", size as u64) } else { format!("{} {unit}", format_f(size, 1)) };
    }
    size /= 1024.0;
  }
  format!("{} PB", format_f(size, 1))
}

fn unix_seconds(moment: SystemTime) -> f64 {
  match moment.duration_since(UNIX_EPOCH) {
    Ok(duration) => duration.as_secs_f64(),
    Err(error) => -(error.duration().as_secs_f64()),
  }
}

fn modified_seconds(metadata: &std::fs::Metadata) -> f64 {
  metadata.modified().map(unix_seconds).unwrap_or(0.0)
}

/// `str(OSError)` with a filename: `[Errno N] <strerror>: '<path>'`.
fn os_error(path: &str, error: &std::io::Error) -> String {
  match error.raw_os_error() {
    Some(errno) => {
      let text = error.to_string();
      let strerror = text.strip_suffix(&format!(" (os error {errno})")).unwrap_or(&text);
      format!("[Errno {errno}] {strerror}: '{}'", charliebot_core::text_repr::str_repr(path))
    }
    None => format!("{error}: '{path}'"),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// `test_gc_trash_yes_hard_deletes`: `--yes` removes every listed entry and keeps the trash
  /// directory itself (the golden case pins the printed lines; this pins the deletion).
  #[test]
  fn test_gc_trash_yes_hard_deletes() {
    let temp = tempfile::tempdir().unwrap();
    let trash = temp.path().join(".trash");
    for name in ["charliebot-task-a", "charliebot-task-b"] {
      let entry = trash.join(name);
      std::fs::create_dir_all(&entry).unwrap();
      std::fs::write(entry.join("diff.txt"), "content").unwrap();
    }

    let entries = list_trash_entries(&trash, SystemTime::now()).unwrap();

    assert_eq!(entries.len(), 2);
    assert!(!purge(&entries));

    assert!(!trash.join("charliebot-task-a").exists());
    assert!(!trash.join("charliebot-task-b").exists());
    // The trash dir itself remains; it is just empty now.
    assert_eq!(std::fs::read_dir(&trash).unwrap().count(), 0);
  }
}
