//! The stat-only signatures of a repository's state (`src/features/diff_view/api.py`): the ref-state signature that
//! keys the branch memo, and the `.gitattributes` signature that keys the manifest memo.

use std::fs::Metadata;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use charliebot_core::paths;
use walkdir::WalkDir;

/// `(mtime_ns, size)` of one file. Python compares the nanosecond clock; a pair of seconds and nanoseconds compares
/// the same way.
pub(crate) type FileStamp = (i64, i64, u64);

/// `_RefSignature`: every file and directory that feeds ref resolution, sorted by path.
pub(crate) type Signature = Vec<(String, FileStamp)>;

/// `_attributes_signature`: the stamp of `<repo>/.gitattributes`, or `None` when the file is absent.
pub(crate) fn attributes_signature(repo: &Path) -> io::Result<Option<FileStamp>> {
  match std::fs::metadata(repo.join(".gitattributes")) {
    Ok(metadata) => Ok(Some(stamp(&metadata))),
    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
    Err(error) => Err(error),
  }
}

/// `_refs_signature`: the git dir's top-level files, `packed-refs`, `shallow`, `info/grafts`, the shared refs tree
/// (its directories), and, in a linked worktree, the worktree's own refs tree.
pub(crate) fn signature(repo: &Path) -> io::Result<Signature> {
  let (git_dir, common_dir) = git_dirs(repo)?;
  let mut sig = Vec::new();
  for entry in std::fs::read_dir(&git_dir)? {
    let entry = entry?;
    if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
      continue;
    }
    // A ref deleted between the listing and the stat drops from the signature; the next walk sees the same absence.
    let Ok(metadata) = std::fs::symlink_metadata(entry.path()) else {
      continue;
    };
    sig.push((paths::display(&entry.path()), stamp(&metadata)));
  }
  for name in ["packed-refs", "shallow"] {
    stat_or_skip(&common_dir.join(name), &mut sig);
  }
  stat_or_skip(&common_dir.join("info").join("grafts"), &mut sig);
  walk_refs_dirs(&common_dir.join("refs"), &mut sig);
  if git_dir != common_dir {
    walk_refs_dirs(&git_dir.join("refs"), &mut sig);
  }
  sig.sort();
  Ok(sig)
}

/// `_git_dirs`: the git dir and the common dir, following a worktree's `.git` file and its `commondir` link.
fn git_dirs(repo: &Path) -> io::Result<(PathBuf, PathBuf)> {
  let dot_git = repo.join(".git");
  let git_dir = if dot_git.is_file() {
    let text = std::fs::read_to_string(&dot_git)?;
    let line = text.trim();
    let Some(target) = line.strip_prefix("gitdir:") else {
      return Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!("unreadable .git file: {}", paths::display(&dot_git)),
      ));
    };
    let target = PathBuf::from(target.trim());
    if target.is_absolute() { target } else { paths::realpath(&repo.join(target)) }
  } else {
    dot_git
  };
  let rel = match std::fs::read_to_string(git_dir.join("commondir")) {
    Ok(text) => text,
    Err(error) if error.kind() == io::ErrorKind::InvalidData => return Err(error),
    Err(_) => return Ok((git_dir.clone(), git_dir)),
  };
  let rel = PathBuf::from(rel.trim());
  let common_dir = if rel.is_absolute() { rel } else { paths::realpath(&git_dir.join(&rel)) };
  Ok((git_dir, common_dir))
}

/// `_stat_or_skip`: records `path` with its stamp when it exists; `stat` follows symbolic links.
fn stat_or_skip(path: &Path, sig: &mut Signature) {
  if let Ok(metadata) = std::fs::metadata(path) {
    sig.push((paths::display(path), stamp(&metadata)));
  }
}

/// `_walk_refs_dirs`: records `root` and every directory under it. Git publishes each ref by renaming a lockfile into
/// its directory, which moves that directory's own stamp, so the directories carry every ref change.
fn walk_refs_dirs(root: &Path, sig: &mut Signature) {
  for entry in WalkDir::new(root) {
    let Ok(entry) = entry else {
      continue;
    };
    if !entry.file_type().is_dir() {
      continue;
    }
    let Ok(metadata) = entry.metadata() else {
      continue;
    };
    sig.push((paths::display(entry.path()), stamp(&metadata)));
  }
}

fn stamp(metadata: &Metadata) -> FileStamp {
  (metadata.mtime(), metadata.mtime_nsec(), metadata.size())
}
