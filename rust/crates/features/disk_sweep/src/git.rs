use std::collections::HashSet;
use std::ffi::OsString;
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

pub(crate) fn inspect_worktree(path: &Path) -> Result<WorktreeInspection> {
  let status =
    git_output(path, &["status", "--porcelain=v1", "-z", "--untracked-files=all", "--ignore-submodules=none"], None)?;
  if has_dirty_submodule(path, &status)? {
    return Ok(WorktreeInspection { status, untracked: Vec::new(), skip_reason: Some("dirty-submodule") });
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

pub(crate) fn repo_has_unsaved_changes(path: &Path) -> Result<bool> {
  if !git_output(path, &["status", "--porcelain"], None)?.is_empty() {
    return Ok(true);
  }
  Ok(!git_output(path, &["rev-list", "--all", "--not", "--remotes"], None)?.is_empty())
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

fn run_git(path: &Path, arguments: &[&str], input: Option<Vec<u8>>) -> Result<Output> {
  // The sweep handlers already run on the blocking pool of the server runtime, so the async `git` subprocess rides
  // that runtime; `subprocess.run(..., timeout=60)` in Python owns the pipe pump and the deadline.
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
    if let Some(bytes) = input {
      let mut stdin = child.stdin.take().expect("git stdin is piped when input is present");
      // A git that exits before reading its input makes the write fail with a broken pipe; the exit status carries the error.
      if let Err(error) = stdin.write_all(&bytes).await
        && error.kind() != std::io::ErrorKind::BrokenPipe
      {
        return Err(anyhow::Error::new(error).context("writing git stdin"));
      }
    }
    match tokio::time::timeout(GIT_TIMEOUT, child.wait_with_output()).await {
      Ok(output) => Ok(output.context("waiting for git subprocess")?),
      Err(_) => bail!("git {} timed out in {} after 60 seconds", arguments.join(" "), path.display()),
    }
  })
}

fn decode_error(bytes: &[u8]) -> String {
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
