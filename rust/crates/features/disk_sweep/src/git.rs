use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::AsyncWriteExt;

const GIT_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_UNTRACKED_BYTES: u64 = 10 * 1024 * 1024;

pub(crate) struct WorktreeInspection {
  pub status: Vec<u8>,
  pub untracked: Vec<PathBuf>,
  pub skip_reason: Option<&'static str>,
}

/// Caches and the one env file that a checkout rebuilds, so a removal may drop them. The 11-22 audit allowed exactly
/// these; a real `.venv` directory is not among them.
const REBUILDABLE_CACHE_DIRS: [&str; 4] = ["__pycache__", ".pytest_cache", ".mypy_cache", ".ruff_cache"];
const VENV_NAME: &str = ".venv";
const TORCH_LOCK_NAME: &str = ".torch-runtime.lock";

pub(crate) fn inspect_worktree(path: &Path) -> Result<WorktreeInspection> {
  let status =
    git_output(path, &["status", "--porcelain=v1", "-z", "--untracked-files=all", "--ignore-submodules=none"], None)?;
  if has_dirty_submodule(path, &status)? {
    return Ok(WorktreeInspection { status, untracked: Vec::new(), skip_reason: Some("dirty-submodule") });
  }
  if let Some(reason) = head_keeping_reason(path)? {
    return Ok(WorktreeInspection { status, untracked: Vec::new(), skip_reason: Some(reason) });
  }
  if has_non_rebuildable_ignored(path)? {
    return Ok(WorktreeInspection { status, untracked: Vec::new(), skip_reason: Some("non-rebuildable-ignored") });
  }
  let untracked = expanded_untracked_paths(path)?;
  for candidate in &untracked {
    if fs::symlink_metadata(candidate)
      .with_context(|| format!("inspecting untracked path {}", candidate.display()))?
      .size()
      > MAX_UNTRACKED_BYTES
    {
      return Ok(WorktreeInspection { status, untracked, skip_reason: Some("large-untracked") });
    }
  }
  Ok(WorktreeInspection { status, untracked, skip_reason: None })
}

/// Why a nested repository of a scratch candidate must be kept, or `None` when removing it loses nothing.
pub(crate) fn repo_unsaved_reason(path: &Path) -> Result<Option<&'static str>> {
  if !git_output(path, &["status", "--porcelain"], None)?.is_empty() {
    return Ok(Some("has-unsaved-repo"));
  }
  if !git_output(path, &["rev-list", "--all", "--not", "--remotes"], None)?.is_empty() {
    return Ok(Some("has-unsaved-repo"));
  }
  if has_non_rebuildable_ignored(path)? {
    return Ok(Some("non-rebuildable-ignored"));
  }
  head_keeping_reason(path)
}

/// Whether HEAD survives the removal of this checkout: some ref other than the checkout's own branch must contain it.
/// `unborn-head` names a repository with no commit, which keeps nothing and is skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeadKeeping {
  Reachable,
  Unreachable,
  Unborn,
}

fn head_keeping_reason(path: &Path) -> Result<Option<&'static str>> {
  Ok(match head_keeping(path)? {
    HeadKeeping::Reachable => None,
    HeadKeeping::Unreachable => Some("unreachable-head"),
    HeadKeeping::Unborn => Some("unborn-head"),
  })
}

fn head_keeping(path: &Path) -> Result<HeadKeeping> {
  let head = run_git(path, &["rev-parse", "--verify", "-q", "HEAD"], None)?;
  match head.status.code() {
    Some(0) => {}
    Some(1) => return Ok(HeadKeeping::Unborn),
    _ => bail!("git rev-parse HEAD failed in {}: {}", path.display(), decode_error(&head.stderr)),
  }
  let commit = decode_error(&head.stdout);
  let branch = run_git(path, &["symbolic-ref", "-q", "HEAD"], None)?;
  let own_branch = match branch.status.code() {
    Some(0) => Some(decode_error(&branch.stdout)),
    Some(1) => None,
    _ => bail!("git symbolic-ref HEAD failed in {}: {}", path.display(), decode_error(&branch.stderr)),
  };
  let containing = git_output(
    path,
    &["for-each-ref", "--contains", commit.as_str(), "--format=%(refname)", "refs/remotes", "refs/heads"],
    None,
  )?;
  let other_ref =
    String::from_utf8_lossy(&containing).lines().any(|reference| own_branch.as_deref() != Some(reference));
  Ok(if other_ref { HeadKeeping::Reachable } else { HeadKeeping::Unreachable })
}

/// Whether the checkout holds an ignored entry that a removal would destroy. Ignored entries are listed as the
/// untracked-ignored files and as the collapsed `!!` entries of `status --ignored`; each must be a cache directory,
/// a `.venv` symlink to the main checkout's env (or a dangling one), or a 0-byte `.torch-runtime.lock`.
fn has_non_rebuildable_ignored(repo: &Path) -> Result<bool> {
  let listed = git_output(repo, &["ls-files", "-o", "-i", "--exclude-standard", "-z"], None)?;
  let collapsed = git_output(repo, &["status", "--ignored=matching", "--porcelain=v1", "-z"], None)?;
  let main_venv = main_checkout(repo)?.join(VENV_NAME);
  let ignored_files = listed.split(|byte| *byte == 0).filter(|raw| !raw.is_empty());
  let ignored_entries = collapsed
    .split(|byte| *byte == 0)
    .filter_map(|entry| entry.strip_prefix(b"!! "))
    .map(|raw| raw.strip_suffix(b"/").unwrap_or(raw));
  for raw in ignored_files.chain(ignored_entries) {
    let relative = PathBuf::from(OsString::from_vec(raw.to_vec()));
    if !is_rebuildable(repo, &relative, &main_venv)? {
      return Ok(true);
    }
  }
  Ok(false)
}

fn is_rebuildable(repo: &Path, relative: &Path, main_venv: &Path) -> Result<bool> {
  let mut prefix = PathBuf::new();
  for component in relative.components() {
    prefix.push(component);
    let name = component.as_os_str();
    if name == OsStr::new(VENV_NAME) {
      return venv_link_is_rebuildable(&repo.join(&prefix), main_venv);
    }
    if REBUILDABLE_CACHE_DIRS.iter().any(|cache| name == OsStr::new(cache)) {
      return Ok(true);
    }
  }
  if relative.file_name().is_some_and(|name| name == OsStr::new(TORCH_LOCK_NAME)) {
    let metadata = fs::symlink_metadata(repo.join(relative))
      .with_context(|| format!("inspecting ignored path {}", repo.join(relative).display()))?;
    return Ok(metadata.is_file() && metadata.len() == 0);
  }
  Ok(false)
}

/// A `.venv` entry is rebuildable only as a symlink that dangles or resolves to the main checkout's `.venv`. A real
/// directory is refused without looking inside it.
fn venv_link_is_rebuildable(link: &Path, main_venv: &Path) -> Result<bool> {
  let metadata = fs::symlink_metadata(link).with_context(|| format!("inspecting ignored path {}", link.display()))?;
  if !metadata.file_type().is_symlink() {
    return Ok(false);
  }
  match fs::metadata(link) {
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
    Err(error) => Err(error).with_context(|| format!("inspecting ignored path {}", link.display())),
    Ok(_) => {
      let target = fs::canonicalize(link).with_context(|| format!("resolving {}", link.display()))?;
      match fs::canonicalize(main_venv) {
        Ok(main) => Ok(target == main),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("resolving {}", main_venv.display())),
      }
    }
  }
}

/// The main checkout that owns this repository's git directory: the parent of the common `.git` directory, which a
/// linked worktree shares with its main checkout.
pub(crate) fn main_checkout(repo: &Path) -> Result<PathBuf> {
  let common = git_output(repo, &["rev-parse", "--path-format=absolute", "--git-common-dir"], None)?;
  let common = PathBuf::from(OsString::from_vec(common.trim_ascii_end().to_vec()));
  common
    .parent()
    .map(Path::to_path_buf)
    .with_context(|| format!("git common directory {} has no parent", common.display()))
}

pub(crate) fn tracked_diff(path: &Path) -> Result<Vec<u8>> {
  git_output(path, &["diff", "--binary", "HEAD"], None)
}

fn git_output(path: &Path, arguments: &[&str], input: Option<Vec<u8>>) -> Result<Vec<u8>> {
  let output = run_git(path, arguments, input)?;
  if !output.status.success() {
    bail!("git {} failed in {}: {}", arguments.join(" "), path.display(), decode_error(&output.stderr));
  }
  Ok(output.stdout)
}

pub(crate) fn run_git(path: &Path, arguments: &[&str], input: Option<Vec<u8>>) -> Result<Output> {
  // The sweep handlers already run on the blocking pool of the server runtime, so the async `git` subprocess rides
  // that runtime. The `block_on` call owns the pipe pump and the deadline.
  tokio::runtime::Handle::current().block_on(async {
    let mut command = tokio::process::Command::new("git");
    command
      .args(arguments)
      .current_dir(path)
      .env("GIT_OPTIONAL_LOCKS", "0")
      .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .kill_on_drop(true);
    let mut child =
      command.spawn().with_context(|| format!("spawning git {} in {}", arguments.join(" "), path.display()))?;
    let stdin = child.stdin.take();
    // The input is written while the output is drained: a git that fills its stdout before it reads all of its stdin
    // would otherwise block both pipes. A git that exits before reading its input makes the write fail with a broken
    // pipe. The exit status carries the error.
    let write = async move {
      let Some(bytes) = input else {
        return Ok(());
      };
      let mut stdin = stdin.expect("git stdin is piped when input is present");
      match stdin.write_all(&bytes).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(anyhow::Error::new(error).context("writing git stdin")),
      }
    };
    let finished = async { tokio::join!(write, child.wait_with_output()) };
    match tokio::time::timeout(GIT_TIMEOUT, finished).await {
      Ok((written, output)) => {
        written?;
        output.context("waiting for git subprocess")
      }
      Err(_) => bail!("git {} timed out in {} after 60 seconds", arguments.join(" "), path.display()),
    }
  })
}

pub(crate) fn decode_error(bytes: &[u8]) -> String {
  String::from_utf8_lossy(bytes).trim().to_string()
}

fn submodule_paths(repo: &Path) -> Result<HashSet<Vec<u8>>> {
  let output = git_output(repo, &["ls-files", "--stage", "-z"], None)?;
  let mut result = HashSet::new();
  for entry in output.split(|byte| *byte == 0).filter(|entry| !entry.is_empty()) {
    let Some(separator) = entry.iter().position(|byte| *byte == b'\t') else {
      bail!("git ls-files --stage returned a row without a path in {}", repo.display());
    };
    let metadata = &entry[..separator];
    let path = &entry[separator + 1..];
    if metadata.split(|byte| *byte == b' ').next() == Some(b"160000") {
      result.insert(path.to_vec());
    }
  }
  Ok(result)
}

fn has_dirty_submodule(repo: &Path, status: &[u8]) -> Result<bool> {
  let paths = submodule_paths(repo)?;
  if paths.is_empty() {
    return Ok(false);
  }
  for entry in status.split(|byte| *byte == 0).filter(|entry| entry.len() >= 4) {
    let changed_path = &entry[3..];
    if paths.contains(changed_path) && &entry[..2] != b"  " {
      return Ok(true);
    }
  }
  let submodule_status = git_output(repo, &["submodule", "status", "--recursive"], None)?;
  let submodule_status = String::from_utf8_lossy(&submodule_status);
  Ok(submodule_status.lines().any(|line| line.starts_with('+') || line.starts_with('U') || line.contains("-dirty")))
}

fn expanded_untracked_paths(repo: &Path) -> Result<Vec<PathBuf>> {
  let listed = git_output(repo, &["ls-files", "--others", "--exclude-standard", "-z"], None)?;
  let mut candidates: HashSet<PathBuf> = HashSet::new();
  for raw in listed.split(|byte| *byte == 0).filter(|raw| !raw.is_empty()) {
    let relative = PathBuf::from(OsString::from_vec(raw.to_vec()));
    let path = repo.join(relative);
    let metadata =
      fs::symlink_metadata(&path).with_context(|| format!("inspecting untracked path {}", path.display()))?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
      collect_untracked_leaves(&path, &mut candidates)?;
    } else {
      candidates.insert(path);
    }
  }
  let mut relative_paths = candidates
    .into_iter()
    .map(|path| path.strip_prefix(repo).expect("untracked path is below its repository").to_path_buf())
    .collect::<Vec<_>>();
  relative_paths.sort();
  if relative_paths.is_empty() {
    return Ok(Vec::new());
  }
  let mut input = Vec::new();
  for path in &relative_paths {
    input.extend_from_slice(path.as_os_str().as_bytes());
    input.push(0);
  }
  let output = run_git(repo, &["check-ignore", "--no-index", "-z", "--stdin"], Some(input))?;
  if !matches!(output.status.code(), Some(0 | 1)) {
    bail!("git check-ignore failed in {}: {}", repo.display(), decode_error(&output.stderr));
  }
  let ignored: HashSet<PathBuf> = output
    .stdout
    .split(|byte| *byte == 0)
    .filter(|raw| !raw.is_empty())
    .map(|raw| PathBuf::from(OsString::from_vec(raw.to_vec())))
    .collect();
  Ok(relative_paths.into_iter().filter(|path| !ignored.contains(path)).map(|path| repo.join(path)).collect())
}

fn collect_untracked_leaves(directory: &Path, output: &mut HashSet<PathBuf>) -> Result<()> {
  for entry in sorted_entries(directory)? {
    let name = entry.file_name();
    if name.as_bytes() == b".git" {
      continue;
    }
    let path = entry.path();
    let metadata =
      fs::symlink_metadata(&path).with_context(|| format!("inspecting untracked path {}", path.display()))?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
      collect_untracked_leaves(&path, output)?;
    } else {
      output.insert(path);
    }
  }
  Ok(())
}

pub(crate) fn sorted_entries(path: &Path) -> Result<Vec<fs::DirEntry>> {
  let mut entries = fs::read_dir(path)
    .with_context(|| format!("reading directory {}", path.display()))?
    .collect::<std::io::Result<Vec<_>>>()
    .with_context(|| format!("reading directory entry in {}", path.display()))?;
  entries.sort_by_key(|entry| entry.file_name());
  Ok(entries)
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::path::Path;
  use std::process::Command;

  use tempfile::tempdir;

  use super::expanded_untracked_paths;

  fn git_init(path: &Path) {
    assert!(Command::new("git").args(["init", "-q"]).current_dir(path).status().unwrap().success());
  }

  /// Every leaf of the nested repository is ignored by the outer one, so `check-ignore` answers 10,000 paths on its
  /// stdout while it still reads them on stdin. Both pipes fill, and the answers must be drained as the paths go in.
  #[tokio::test]
  #[ignore = "integration"]
  async fn check_ignore_answers_a_large_ignored_nested_repository_without_blocking() {
    let root = tempdir().unwrap();
    let repo = root.path().join("outer");
    let nested = repo.join("sub");
    fs::create_dir_all(&nested).unwrap();
    fs::write(repo.join(".gitignore"), "*.o\n").unwrap();
    git_init(&repo);
    git_init(&nested);
    for index in 0..10_000 {
      fs::write(nested.join(format!("f{index}.o")), b"").unwrap();
    }

    let gitignore = repo.join(".gitignore");
    let kept = tokio::task::spawn_blocking(move || expanded_untracked_paths(&repo)).await.unwrap().unwrap();
    // The outer `.gitignore` is untracked and not ignored, so it is the one candidate that survives.
    assert_eq!(kept, vec![gitignore]);
  }
}
