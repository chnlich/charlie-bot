//! Git helpers for task runs, the delivery chain and the improve loop.
//!
//! Ported from `src/infra/git.py`: base-branch resolution, worktree creation and
//! removal, and the remote push/fetch probes. One subprocess path (`git_proc_bytes`,
//! Python `_git_proc_bytes`) serves the whole module; every caller keeps its own
//! timeout policy and error text.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use charliebot_core::pyfmt;
use ignore::WalkBuilder;
use tokio::process::Command;
use tokio::time::timeout;

/// `src/infra/timeouts.py:14` — read-class git subprocesses (`SUBPROCESS_GIT_READ_TIMEOUT_ASYNC`).
const SUBPROCESS_GIT_READ_TIMEOUT_ASYNC: Duration = Duration::from_secs(30);
/// `src/infra/timeouts.py:20` — worktree creation and other write-class git subprocesses.
#[cfg(not(feature = "test-hooks"))]
const SUBPROCESS_GIT_WRITE_TIMEOUT: Duration = Duration::from_secs(60);
/// `src/infra/timeouts.py:27` — one attempt of a remote git operation.
#[cfg(not(feature = "test-hooks"))]
const SUBPROCESS_GIT_REMOTE_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(120);
/// `src/infra/timeouts.py:28` — attempts per remote operation, the first included.
const GIT_REMOTE_MAX_ATTEMPTS: usize = 3;

/// `src/infra/git.py:15` — cache/env/build artifact directory names removed from a worktree before `git worktree remove`.
const WORKTREE_LOCAL_ARTIFACT_NAMES: [&str; 6] = [".pixi", ".pixi-cache", ".uv-cache", ".venv", ".local", "build"];
/// `src/infra/git.py:27` — stderr markers of git's lock-class errors, the only worktree-add failures that retry.
const WORKTREE_LOCK_RETRY_MARKERS: [&str; 3] = ["could not lock", "unable to create", "file exists"];
/// `src/infra/git.py:28` — the first invocation plus 3 retries.
const WORKTREE_ADD_ATTEMPTS: usize = 4;
/// `src/infra/git.py:29` — retry backoff base, doubled per retry: 100ms, 200ms, 400ms.
const WORKTREE_RETRY_BACKOFF_S: f64 = 0.1;

/// `ref: refs/heads/` — the symref prefix `git ls-remote --symref` reports the default branch with (`src/infra/git.py:138`).
const SYMREF_PREFIX: &str = "ref: refs/heads/";

// Test hooks (`test-hooks` feature, dev-dependency only): the hook values keep the
// hung-git tests under a second, and the git program override routes the module's
// subprocesses through a test's `/bin/sh` script. Production builds keep the Python
// values and run plain `git`.
#[cfg(feature = "test-hooks")]
static GIT_PROGRAM_OVERRIDE: Mutex<Option<(String, Vec<String>)>> = Mutex::new(None);

/// The git program the module spawns, with the arguments prepended before its own.
#[cfg(feature = "test-hooks")]
pub fn set_git_program_override(program: &str, prefix_args: &[&str]) {
  let mut override_value = GIT_PROGRAM_OVERRIDE.lock().expect("git program override is not poisoned");
  *override_value = Some((program.to_string(), prefix_args.iter().map(|argument| argument.to_string()).collect()));
}

/// Restores the plain `git` spawn.
#[cfg(feature = "test-hooks")]
pub fn clear_git_program_override() {
  *GIT_PROGRAM_OVERRIDE.lock().expect("git program override is not poisoned") = None;
}

/// The write-class limit a test set in place of the hook value; `None` keeps the hook value.
#[cfg(feature = "test-hooks")]
static GIT_WRITE_TIMEOUT_OVERRIDE: Mutex<Option<Duration>> = Mutex::new(None);

/// Holds a write-limit override; dropping it restores the hook value, even when the test panics.
#[cfg(feature = "test-hooks")]
pub struct GitWriteTimeoutOverride;

#[cfg(feature = "test-hooks")]
impl Drop for GitWriteTimeoutOverride {
  fn drop(&mut self) {
    *GIT_WRITE_TIMEOUT_OVERRIDE.lock().expect("git write timeout override is not poisoned") = None;
  }
}

/// Replaces the write-class limit until the returned guard drops.
#[cfg(feature = "test-hooks")]
pub fn override_git_write_timeout(limit: Duration) -> GitWriteTimeoutOverride {
  *GIT_WRITE_TIMEOUT_OVERRIDE.lock().expect("git write timeout override is not poisoned") = Some(limit);
  GitWriteTimeoutOverride
}

#[cfg(feature = "test-hooks")]
fn git_program() -> (String, Vec<String>) {
  GIT_PROGRAM_OVERRIDE
    .lock()
    .expect("git program override is not poisoned")
    .clone()
    .unwrap_or_else(|| ("git".to_string(), Vec::new()))
}

#[cfg(not(feature = "test-hooks"))]
fn git_program() -> (String, Vec<String>) {
  ("git".to_string(), Vec::new())
}

/// The test-build limit for write and remote calls: 300 ms, which keeps the hung-git tests under a second.
#[cfg(feature = "test-hooks")]
fn test_hook_git_timeout() -> Duration {
  Duration::from_millis(300)
}

/// The worktree-add write limit: the hook value keeps the hung-worktree-add test under a second,
/// unless a test replaced it with [`override_git_write_timeout`].
#[cfg(feature = "test-hooks")]
pub fn subprocess_git_write_timeout() -> Duration {
  let limit = *GIT_WRITE_TIMEOUT_OVERRIDE.lock().expect("git write timeout override is not poisoned");
  limit.unwrap_or_else(test_hook_git_timeout)
}

/// The per-attempt remote limit; the hook value keeps the hung-remote tests under a second.
#[cfg(feature = "test-hooks")]
pub fn subprocess_git_remote_attempt_timeout() -> Duration {
  test_hook_git_timeout()
}

#[cfg(not(feature = "test-hooks"))]
fn subprocess_git_write_timeout() -> Duration {
  SUBPROCESS_GIT_WRITE_TIMEOUT
}

#[cfg(not(feature = "test-hooks"))]
fn subprocess_git_remote_attempt_timeout() -> Duration {
  SUBPROCESS_GIT_REMOTE_ATTEMPT_TIMEOUT
}

/// A timeout's seconds the way Python's f-string prints its float value: `0.3s`, `30.0s`, `120.0s`.
fn py_secs(limit: Duration) -> String {
  format!("{:.1}", limit.as_secs_f64())
}

/// The result of resolving a `--base-branch` value into a worktree start point (`src/infra/git.py:68`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseResolution {
  /// Bare branch name, or the 40-hex SHA itself.
  pub canonical: String,
  /// Ref handed to `git worktree add` (`origin/<b>`, `<b>`, or the SHA).
  pub start_point: String,
  /// Human-readable resolution summary for prompt/log audit.
  pub detail: String,
}

/// Raised when a `--base-branch` value is ambiguous or unresolvable (`src/infra/git.py:44`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseBranchResolutionError {
  message: String,
}

impl BaseBranchResolutionError {
  fn new(message: impl Into<String>) -> Self {
    Self { message: message.into() }
  }
}

impl std::fmt::Display for BaseBranchResolutionError {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter.write_str(&self.message)
  }
}

impl std::error::Error for BaseBranchResolutionError {}

/// The spawn-and-capture outcome of one git subprocess: raw pipes plus the exit code.
struct GitProc {
  returncode: i32,
  stdout: Vec<u8>,
  stderr: Vec<u8>,
}

/// Why a git subprocess produced no result: the pipes were read past the caller's limit, or the spawn failed.
enum GitProcError {
  TimedOut,
  Spawn(String),
}

impl GitProcError {
  fn text(&self) -> String {
    match self {
      // Python's bare `TimeoutError()` stringifies to the empty string.
      Self::TimedOut => String::new(),
      Self::Spawn(error) => error.clone(),
    }
  }
}

/// Spawn git in *repo_path*, read both pipes under *limit*, and kill the process on timeout
/// (`src/infra/git.py:76`). The timeout surfaces as [`GitProcError::TimedOut`] so each caller
/// keeps its own timeout policy.
async fn git_proc_bytes(repo_path: &Path, args: &[&str], limit: Duration) -> Result<GitProc, GitProcError> {
  let (program, prefix_args) = git_program();
  let mut command = Command::new(program);
  command
    .args(prefix_args)
    .args(args)
    .current_dir(repo_path)
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .kill_on_drop(true);
  match timeout(limit, command.output()).await {
    Ok(Ok(output)) => {
      Ok(GitProc { returncode: output.status.code().unwrap_or(-1), stdout: output.stdout, stderr: output.stderr })
    }
    Ok(Err(error)) => Err(GitProcError::Spawn(error.to_string())),
    Err(_) => Err(GitProcError::TimedOut),
  }
}

/// Spawn `git -C <repo> <args>` with piped output under *limit* (`src/infra/git.py:665`).
async fn git_c_proc_bytes(repo_path: &Path, args: &[&str], limit: Duration) -> Result<GitProc, GitProcError> {
  let (program, prefix_args) = git_program();
  let mut command = Command::new(program);
  command
    .args(prefix_args)
    .arg("-C")
    .arg(repo_path)
    .args(args)
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .kill_on_drop(true);
  match timeout(limit, command.output()).await {
    Ok(Ok(output)) => {
      Ok(GitProc { returncode: output.status.code().unwrap_or(-1), stdout: output.stdout, stderr: output.stderr })
    }
    Ok(Err(error)) => Err(GitProcError::Spawn(error.to_string())),
    Err(_) => Err(GitProcError::TimedOut),
  }
}

fn trimmed(bytes: &[u8]) -> String {
  String::from_utf8_lossy(bytes).trim().to_string()
}

/// Run a git command with a timeout; returns `(success, stdout, stderr)` (`src/infra/git.py:102`).
/// A timeout reports `"<label> timed out after <limit>s"` in its stderr slot.
pub(crate) async fn git_stdout(
  repo_path: &Path,
  args: &[&str],
  limit: Duration,
  timeout_label: &str,
) -> (bool, String, String) {
  match git_proc_bytes(repo_path, args, limit).await {
    Ok(proc) if proc.returncode == 0 => (true, trimmed(&proc.stdout), String::new()),
    Ok(proc) => (false, trimmed(&proc.stdout), trimmed(&proc.stderr)),
    Err(_) => (false, String::new(), format!("{timeout_label} timed out after {}s", py_secs(limit))),
  }
}

/// Resolve a ref to a commit SHA; `None` when it does not resolve (`src/infra/git.py:118`).
pub(crate) async fn git_rev_parse_optional(repo_path: &Path, reference: &str) -> Option<String> {
  let (ok, out, _) = git_stdout(
    repo_path,
    &["rev-parse", "--verify", "--quiet", reference],
    SUBPROCESS_GIT_READ_TIMEOUT_ASYNC,
    "git rev-parse",
  )
  .await;
  (ok && !out.is_empty()).then_some(out)
}

/// Run a git command that talks to a remote, retrying failed attempts at once
/// (`src/infra/git.py:631`). Each attempt runs under the per-attempt limit; a hung
/// connection never recovers, so a wait before the retry buys nothing. Returns
/// [`git_stdout`]'s triple from the first successful attempt, or from the last one.
async fn git_remote(repo_path: &Path, args: &[&str], label: &str) -> (bool, String, String) {
  let mut attempt = 1;
  loop {
    let (ok, out, err) = git_stdout(repo_path, args, subprocess_git_remote_attempt_timeout(), label).await;
    if ok {
      return (ok, out, err);
    }
    tracing::warn!(
      target: "charliebot_tasks",
      repo = %repo_path.display(),
      command = label,
      attempt,
      max_attempts = GIT_REMOTE_MAX_ATTEMPTS,
      error = %err,
      "git_remote_attempt_failed"
    );
    if attempt == GIT_REMOTE_MAX_ATTEMPTS {
      return (ok, out, err);
    }
    attempt += 1;
  }
}

/// The ref a branch lives at: `refs/heads/<branch>` (`src/infra/git.py:142`).
fn heads_ref(branch: &str) -> String {
  format!("refs/heads/{branch}")
}

/// The remote-tracking ref origin holds for a published branch (`src/infra/git.py:147`).
fn origin_tracking_ref(branch: &str) -> String {
  format!("refs/remotes/origin/{branch}")
}

/// Parse the symref line of `git ls-remote --symref` output into the default branch name
/// (`src/infra/git.py:152`).
fn default_branch_from_ls_remote(
  out: &str,
  source: &str,
  repo_path: &Path,
) -> Result<String, BaseBranchResolutionError> {
  for line in out.lines() {
    if let Some(rest) = line.strip_prefix(SYMREF_PREFIX) {
      let branch = rest.split('\t').next().unwrap_or("").trim();
      if !branch.is_empty() {
        return Ok(branch.to_string());
      }
    }
  }
  Err(BaseBranchResolutionError::new(format!(
    "{source} in {} advertised no {SYMREF_PREFIX}<branch> symref line; refusing to infer a default from local refs. Output: {}",
    repo_path.display(),
    pyfmt::str_repr(out)
  )))
}

/// The SHA ls-remote reported for exactly *reference*, or `None` when it listed no such line
/// (`src/infra/git.py:164`).
fn ls_remote_ref_sha(out: &str, reference: &str) -> Option<String> {
  for line in out.lines() {
    let (sha, listed) = line.split_once('\t').unwrap_or((line, ""));
    if listed.trim() == reference && !sha.is_empty() {
      return Some(sha.to_string());
    }
  }
  None
}

/// Return the directory name CharlieBot uses for a git worktree branch (`src/infra/git.py:32`).
pub fn git_worktree_dir_name(branch_name: &str) -> String {
  branch_name.replace('/', "-")
}

/// Get the current branch of the repo (`src/infra/git.py:37`).
pub async fn git_current_branch(repo_path: &Path) -> anyhow::Result<String> {
  let proc =
    match git_proc_bytes(repo_path, &["rev-parse", "--abbrev-ref", "HEAD"], SUBPROCESS_GIT_READ_TIMEOUT_ASYNC).await {
      Ok(proc) => proc,
      Err(GitProcError::TimedOut) => {
        anyhow::bail!(
          "git rev-parse timed out after {}s in {}",
          py_secs(SUBPROCESS_GIT_READ_TIMEOUT_ASYNC),
          repo_path.display()
        )
      }
      Err(GitProcError::Spawn(error)) => anyhow::bail!("git rev-parse failed: {error}"),
    };
  if proc.returncode != 0 {
    let error_message = trimmed(&proc.stderr);
    if error_message.contains("unknown revision") {
      tracing::warn!(
        target: "charliebot_tasks",
        repo = %repo_path.display(),
        detail = "no commits yet, defaulting to main",
        "git_empty_repo_fallback"
      );
      return Ok("main".to_string());
    }
    anyhow::bail!("git rev-parse failed: {error_message}");
  }
  Ok(trimmed(&proc.stdout))
}

/// The full commit sha *rev* resolves to, or `None` when it does not exist (`src/infra/git.py:658`).
pub async fn git_rev_parse(repo_path: &Path, rev: &str) -> Option<String> {
  let commit_object = format!("{rev}^{{commit}}");
  match git_c_proc_bytes(repo_path, &["rev-parse", "--verify", &commit_object], SUBPROCESS_GIT_READ_TIMEOUT_ASYNC).await
  {
    Ok(proc) if proc.returncode == 0 => {
      let out = trimmed(&proc.stdout);
      (!out.is_empty()).then_some(out)
    }
    Ok(_) => None,
    Err(error) => {
      tracing::warn!(
        target: "charliebot_tasks",
        repo = %repo_path.display(),
        rev,
        error = %error.text(),
        "git_rev_parse_failed"
      );
      None
    }
  }
}

/// Count commits in a range, or return None when git rev-list fails or prints no count.
pub async fn git_rev_list_count(repo_path: &Path, range: &str) -> Option<i64> {
  let (ok, output, _) =
    git_stdout(repo_path, &["rev-list", "--count", range], SUBPROCESS_GIT_READ_TIMEOUT_ASYNC, "git rev-list --count")
      .await;
  if !ok || output.is_empty() || !output.chars().all(|character| character.is_ascii_digit()) {
    return None;
  }
  output.parse().ok()
}

/// The remote's default branch and its tip SHA from ONE `git ls-remote --symref origin`
/// (`src/infra/git.py:173`). Never consults `refs/remotes/origin/HEAD` or any local branch.
/// The tip is `None` when the listing names a default branch without listing its ref.
pub async fn git_remote_default_branch_and_tip(
  repo_path: &Path,
) -> Result<(String, Option<String>), BaseBranchResolutionError> {
  let (ok, out, err) =
    git_remote(repo_path, &["-c", "protocol.version=0", "ls-remote", "--symref", "origin"], "git ls-remote").await;
  if !ok {
    return Err(BaseBranchResolutionError::new(format!(
      "cannot read origin's default branch in {} via git ls-remote: {err}",
      repo_path.display()
    )));
  }
  let branch = default_branch_from_ls_remote(&out, "git ls-remote --symref origin", repo_path)?;
  let tip = ls_remote_ref_sha(&out, &heads_ref(&branch));
  Ok((branch, tip))
}

/// Resolve a `--base-branch` value to a worktree start point, failing loudly on ambiguity
/// (`src/infra/git.py:200`). Resolution matrix:
/// - 40-hex SHA → pinned base, verified present; no freshness semantics, no network.
/// - `origin/<b>` → explicit remote: must exist on origin; start at its freshly resolved tip.
/// - `<b>` (bare) → if the remote branch exists, start at the freshly resolved `origin/<b>`
///   unless the local branch is ahead or diverged (a hard error). If the remote branch does
///   not exist, require the branch to exist locally and start there.
///
/// *remote_tip* is a caller's own fresh `git ls-remote` answer for `refs/heads/<branch>`
/// (`None` = the caller did not probe); supplying it skips this function's duplicate
/// ls-remote and origin-configured probe.
pub async fn resolve_base_branch(
  repo_path: &Path,
  base_branch: &str,
  remote_tip: Option<&str>,
) -> Result<BaseResolution, BaseBranchResolutionError> {
  let raw = base_branch.trim();
  if raw.is_empty() {
    return Err(BaseBranchResolutionError::new("base branch is empty"));
  }

  // Full commit SHA: pinned base. Verified by cat-file; no fetch, no remote probe.
  if is_full_sha(raw) {
    let (ok, _, err) = git_stdout(
      repo_path,
      &["cat-file", "-e", &format!("{raw}^{{commit}}")],
      SUBPROCESS_GIT_READ_TIMEOUT_ASYNC,
      "git cat-file",
    )
    .await;
    if !ok {
      return Err(BaseBranchResolutionError::new(format!("base commit {raw} not found in repo: {err}")));
    }
    return Ok(BaseResolution {
      canonical: raw.to_string(),
      start_point: raw.to_string(),
      detail: format!("pinned commit {}", &raw[..12]),
    });
  }

  let explicit_remote = raw.starts_with("origin/");
  let branch = raw.strip_prefix("origin/").unwrap_or(raw);
  if branch.is_empty() || branch.contains("..") || branch.chars().any(char::is_whitespace) {
    return Err(BaseBranchResolutionError::new(format!(
      "invalid branch name in --base-branch: {}",
      pyfmt::str_repr(raw)
    )));
  }

  // Probe origin state: configured? reachable? branch published? A supplied remote_tip
  // is a caller's own successful ls-remote, which already proves origin configured and
  // reachable, so the get-url probe stays on the self-probing path only.
  let origin_configured = if remote_tip.is_none() {
    git_stdout(repo_path, &["remote", "get-url", "origin"], SUBPROCESS_GIT_READ_TIMEOUT_ASYNC, "git remote get-url")
      .await
      .0
  } else {
    true
  };
  let mut remote_tip = remote_tip.map(str::to_string);
  if origin_configured && remote_tip.is_none() {
    let (ok, out, ls_err) =
      git_remote(repo_path, &["-c", "protocol.version=0", "ls-remote", "origin", &heads_ref(branch)], "git ls-remote")
        .await;
    if !ok {
      return Err(BaseBranchResolutionError::new(format!(
        "cannot reach origin to resolve base branch {}: {ls_err}. \
         Retry when the remote is reachable, or pass a full commit SHA to pin the base.",
        pyfmt::str_repr(branch)
      )));
    }
    remote_tip = ls_remote_ref_sha(&out, &heads_ref(branch));
  }

  let Some(remote_tip) = remote_tip else {
    return unpublished_branch_resolution(repo_path, branch, explicit_remote, origin_configured).await;
  };

  // The remote branch exists: start at the freshly resolved tracking ref, fetching first
  // unless the probe showed the tracking ref already holds exactly the probed tip.
  let local_remote_sha = git_rev_parse_optional(repo_path, &origin_tracking_ref(branch)).await;
  let remote_sha = if Some(&remote_tip) != local_remote_sha.as_ref() {
    let (fetched, fetch_err) = git_fetch(repo_path, "origin", branch).await;
    if !fetched {
      return Err(BaseBranchResolutionError::new(format!("git fetch origin {branch} failed: {fetch_err}")));
    }
    match git_rev_parse_optional(repo_path, &origin_tracking_ref(branch)).await {
      Some(sha) => sha,
      None => {
        return Err(BaseBranchResolutionError::new(format!(
          "origin/{branch} listed by ls-remote but missing after fetch in {}",
          repo_path.display()
        )));
      }
    }
  } else {
    // No fetch ran between the two reads, so the tracking ref still holds exactly the
    // probed tip; re-reading it would return the same SHA.
    local_remote_sha.expect("tracking ref holds the probed tip")
  };

  if !explicit_remote {
    // The local-vs-remote divergence check exists only for the bare-branch form; an
    // explicit origin/<b> request never reads the local branch.
    if let Some(local_sha) = git_rev_parse_optional(repo_path, &heads_ref(branch)).await
      && local_sha != remote_sha
    {
      // A local branch strictly behind origin (an ancestor of the origin tip) carries
      // nothing the worktree would miss, so the shared checkout's stale ref starts from
      // the origin tip untouched. A local commit absent from origin (ahead or diverged)
      // still refuses: is-ancestor answers in exit codes (0 behind, 1 not an ancestor),
      // so the raw process is read directly — a _git_stdout failure folds every non-zero
      // code into one boolean.
      let merge_base = git_proc_bytes(
        repo_path,
        &["merge-base", "--is-ancestor", &local_sha, &remote_sha],
        SUBPROCESS_GIT_READ_TIMEOUT_ASYNC,
      )
      .await;
      let merge_base = match merge_base {
        Ok(proc) => proc,
        Err(_) => {
          return Err(BaseBranchResolutionError::new(format!(
            "git merge-base --is-ancestor {local_sha} {remote_sha} timed out after {}s in {}",
            py_secs(SUBPROCESS_GIT_READ_TIMEOUT_ASYNC),
            repo_path.display()
          )));
        }
      };
      match merge_base.returncode {
        0 => {
          tracing::info!(
            target: "charliebot_tasks",
            repo = %repo_path.display(),
            branch,
            local_sha,
            remote_sha,
            "base_branch_local_behind"
          );
          return Ok(BaseResolution {
            canonical: branch.to_string(),
            start_point: format!("origin/{branch}"),
            detail: format!(
              "branch {branch} at origin tip {} (local {branch} at {} is behind, unused)",
              &remote_sha[..12],
              &local_sha[..12]
            ),
          });
        }
        1 => {
          return Err(BaseBranchResolutionError::new(format!(
            "local {branch} ({}) differs from origin/{branch} ({}). \
             A local base must match its remote exactly: push the local commits, \
             pass origin/{branch} to use the remote explicitly, \
             or pass a full commit SHA to pin the base.",
            &local_sha[..12],
            &remote_sha[..12]
          )));
        }
        code => {
          return Err(BaseBranchResolutionError::new(format!(
            "git merge-base --is-ancestor {local_sha} {remote_sha} failed with exit code \
             {code} in {}: {}",
            repo_path.display(),
            trimmed(&merge_base.stderr)
          )));
        }
      }
    }
  }
  Ok(BaseResolution {
    canonical: branch.to_string(),
    start_point: format!("origin/{branch}"),
    detail: format!("branch {branch} at origin tip {}", &remote_sha[..12]),
  })
}

/// The bare-branch resolution for a branch origin does not publish (or when no origin remote
/// is configured): the branch must exist locally (`src/infra/git.py:337`).
async fn unpublished_branch_resolution(
  repo_path: &Path,
  branch: &str,
  explicit_remote: bool,
  origin_configured: bool,
) -> Result<BaseResolution, BaseBranchResolutionError> {
  if explicit_remote {
    return Err(BaseBranchResolutionError::new(format!(
      "origin/{branch} was requested explicitly but that remote branch does not exist on origin."
    )));
  }
  let local_sha = git_rev_parse_optional(repo_path, &heads_ref(branch)).await.ok_or_else(|| {
    BaseBranchResolutionError::new(format!(
      "base branch {} exists neither locally nor on origin (origin {}).",
      pyfmt::str_repr(branch),
      if origin_configured { "has no such branch" } else { "unreachable or not configured" }
    ))
  })?;
  Ok(BaseResolution {
    canonical: branch.to_string(),
    start_point: branch.to_string(),
    detail: format!("local branch {branch} at {} (no matching origin branch)", &local_sha[..12]),
  })
}

/// Whether *raw* is exactly 40 lowercase hex digits (Python `_FULL_SHA_RE`).
fn is_full_sha(raw: &str) -> bool {
  raw.len() == 40 && raw.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// Whether *commit* matches Python `_COMMIT_REV_RE`: 7 to 40 lowercase hex digits.
fn is_commit_rev(commit: &str) -> bool {
  (7..=40).contains(&commit.len()) && commit.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// The per-repository `git worktree add` locks (`src/infra/git.py:18`): concurrent adds in one
/// repository race on shared git state, and a call that loses the config lock exits non-zero
/// after it created its branch, so no retry recovers that loss — this process runs the adds of
/// one repository one at a time.
static WORKTREE_ADD_LOCKS: Mutex<Option<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> = Mutex::new(None);

fn worktree_add_lock(repo_path: &Path) -> Arc<tokio::sync::Mutex<()>> {
  let resolved = repo_path.canonicalize().unwrap_or_else(|_| repo_path.to_path_buf());
  let mut locks = WORKTREE_ADD_LOCKS.lock().expect("worktree add locks are not poisoned");
  locks.get_or_insert_with(HashMap::new).entry(resolved).or_default().clone()
}

/// The backoff before the retry after *attempt* (1-based), doubled per retry (`src/infra/git.py:29`).
fn worktree_retry_backoff(attempt: usize) -> Duration {
  Duration::from_secs_f64(WORKTREE_RETRY_BACKOFF_S * 2f64.powi(attempt as i32 - 1))
}

/// The production backoff schedule, exposed to the tests that assert it.
#[cfg(feature = "test-hooks")]
pub fn test_worktree_retry_backoff(attempt: usize) -> Duration {
  worktree_retry_backoff(attempt)
}

/// Create a git worktree and fail loudly if git reports an error (`src/infra/git.py:360`).
///
/// Resolves the start-point via [`resolve_base_branch`] so that a stale local copy of
/// `<base_branch>` can never silently become a worker's base; ambiguous input raises
/// [`BaseBranchResolutionError`] before any worktree is created. *remote_tip* forwards there.
pub async fn git_create_worktree(
  repo_path: &Path,
  base_branch: &str,
  branch_name: &str,
  wt_path: &Path,
  remote_tip: Option<&str>,
) -> anyhow::Result<BaseResolution> {
  let resolution = resolve_base_branch(repo_path, base_branch, remote_tip).await?;
  let start_point = resolution.start_point.clone();
  let add_lock = worktree_add_lock(repo_path);
  let mut attempt = 1;
  loop {
    let outcome = {
      let _permit = add_lock.lock().await;
      git_proc_bytes(
        repo_path,
        &["worktree", "add", "-b", branch_name, &wt_path.display().to_string(), &start_point],
        subprocess_git_write_timeout(),
      )
      .await
    };
    let proc = match outcome {
      Ok(proc) => proc,
      Err(GitProcError::TimedOut) => {
        anyhow::bail!("git worktree add timed out after {}s for {branch_name}", py_secs(subprocess_git_write_timeout()))
      }
      Err(GitProcError::Spawn(error)) => anyhow::bail!("git worktree add failed to spawn: {error}"),
    };
    if proc.returncode == 0 {
      break;
    }
    let out = trimmed(&proc.stdout);
    let err = trimmed(&proc.stderr);
    let transient =
      WORKTREE_LOCK_RETRY_MARKERS.iter().any(|marker| format!("{out}\n{err}").to_lowercase().contains(marker));
    if !transient || attempt == WORKTREE_ADD_ATTEMPTS {
      tracing::error!(
        target: "charliebot_tasks",
        repo = %repo_path.display(),
        branch = branch_name,
        worktree = %wt_path.display(),
        base_branch,
        start_point,
        stdout = %out,
        stderr = %err,
        returncode = proc.returncode,
        "worktree_create_failed"
      );
      anyhow::bail!(
        "git worktree add failed for {branch_name}: {}",
        if !err.is_empty() {
          err
        } else if !out.is_empty() {
          out
        } else {
          "unknown error".to_string()
        }
      );
    }
    let backoff = worktree_retry_backoff(attempt);
    tracing::warn!(
      target: "charliebot_tasks",
      repo = %repo_path.display(),
      branch = branch_name,
      attempt,
      backoff_s = backoff.as_secs_f64(),
      stderr = %if err.is_empty() { out } else { err },
      "worktree_add_retry"
    );
    tokio::time::sleep(backoff).await;
    attempt += 1;
  }
  tracing::info!(
    target: "charliebot_tasks",
    repo = %repo_path.display(),
    base = base_branch,
    canonical = %resolution.canonical,
    start_point = %start_point,
    resolution = %resolution.detail,
    worktree = %wt_path.display(),
    "git_worktree_created"
  );
  Ok(resolution)
}

/// `os.path.lexists`: true also for a broken symlink.
fn lexists(path: &Path) -> bool {
  path.symlink_metadata().is_ok()
}

/// A non-strict `Path.resolve`: canonicalize when the path exists, else absolutize it.
fn resolve_path(path: &Path) -> PathBuf {
  path.canonicalize().unwrap_or_else(|_| {
    if path.is_absolute() {
      path.to_path_buf()
    } else {
      std::env::current_dir().expect("current directory is readable").join(path)
    }
  })
}

/// Validate the worktree path before removing anything inside it (`src/infra/git.py:436`).
fn assert_safe_worktree_cleanup_target(
  repo_path: &str,
  wt_path: &Path,
  allowed_parent: &Path,
  expected_residue_name: &str,
) -> anyhow::Result<PathBuf> {
  if wt_path.file_name().map(|name| name != expected_residue_name).unwrap_or(true) {
    anyhow::bail!(
      "refusing to clean worktree at {}: directory name does not match expected {expected_residue_name}",
      wt_path.display()
    );
  }
  if wt_path.is_symlink() {
    anyhow::bail!("refusing to clean worktree symlink: {}", wt_path.display());
  }
  let resolved_wt = resolve_path(wt_path);
  let resolved_allowed_parent = resolve_path(allowed_parent);
  let resolved_repo = resolve_path(Path::new(repo_path));
  let home = resolve_path(Path::new(&std::env::var("HOME").unwrap_or_else(|_| String::from("/home/<user>"))));
  if resolved_wt == Path::new("/") || resolved_wt == home {
    anyhow::bail!("refusing to clean unsafe worktree path: {}", wt_path.display());
  }
  if resolved_wt == resolved_repo {
    anyhow::bail!("refusing to clean repo root as worktree: {}", wt_path.display());
  }
  if resolved_wt == resolved_allowed_parent || !resolved_wt.starts_with(&resolved_allowed_parent) {
    anyhow::bail!(
      "refusing to clean worktree outside allowed parent {}: {}",
      resolved_allowed_parent.display(),
      wt_path.display()
    );
  }
  Ok(resolved_wt)
}

/// True when *path* contains any `.git` marker without following symlinks (`src/infra/git.py:464`). An entry that
/// cannot be read is not a marker.
fn contains_git_marker(path: &Path) -> bool {
  worktree_walk(path).build().flatten().any(|entry| entry.file_name() == ".git")
}

/// Whether `name` is one of the cache, env and build directories a worktree sheds before removal.
fn is_local_artifact_name(name: &OsStr) -> bool {
  WORKTREE_LOCAL_ARTIFACT_NAMES.contains(&name.to_string_lossy().as_ref())
}

/// Build a serial, unfiltered worktree walk that does not follow symlinks.
fn worktree_walk(path: &Path) -> WalkBuilder {
  let mut walker = WalkBuilder::new(path);
  walker.min_depth(Some(1)).follow_links(false).hidden(false).standard_filters(false).parents(true);
  walker
}

/// Yields paths of known cache/env/build artifact dirs inside the worktree (`src/infra/git.py:472`).
/// Walks without following symlinks. An artifact dir is yielded but not read, and a nested git worktree (a dir below
/// the root that holds a `.git` marker) is neither yielded nor read.
fn collect_local_worktree_artifacts(wt_path: &Path, artifacts: &mut Vec<PathBuf>) {
  let root = wt_path.to_path_buf();
  let found_artifacts = Arc::new(Mutex::new(Vec::new()));
  let captured_artifacts = Arc::clone(&found_artifacts);
  let mut walker = worktree_walk(wt_path);
  walker.filter_entry(move |entry| {
    if entry.file_name() == ".git" {
      return false;
    }
    if !entry.file_type().is_some_and(|kind| kind.is_dir()) {
      return true;
    }
    if is_local_artifact_name(entry.file_name()) {
      captured_artifacts.lock().expect("artifact-walk capture is not poisoned").push(entry.path().to_path_buf());
      return false;
    }
    !(entry.path().parent() != Some(root.as_path()) && entry.path().join(".git").symlink_metadata().is_ok())
  });
  for _entry in walker.build().flatten() {}
  let mut found_artifacts = found_artifacts.lock().expect("artifact-walk capture is not poisoned");
  artifacts.extend(std::mem::take(&mut *found_artifacts));
}

/// Pre-clean known local cache/env/build artifacts inside the worktree (`src/infra/git.py:489`).
/// A symlinked artifact is skipped rather than treated as an error: deleting a symlink never
/// touches its target, and `git worktree remove --force` removes the entry on its own.
async fn remove_local_worktree_artifacts(wt_path: &Path, thread_id: &str) {
  let mut artifacts = Vec::new();
  collect_local_worktree_artifacts(wt_path, &mut artifacts);
  for artifact_path in artifacts {
    if artifact_path.is_symlink() {
      tracing::warn!(
        target: "charliebot_tasks",
        thread_id,
        path = %artifact_path.display(),
        "worktree_artifact_symlink_skipped"
      );
      continue;
    }
    let removed = tokio::task::spawn_blocking({
      let artifact_path = artifact_path.clone();
      move || std::fs::remove_dir_all(&artifact_path)
    })
    .await
    .expect("artifact removal task completes");
    if let Err(error) = removed {
      tracing::warn!(
        target: "charliebot_tasks",
        thread_id,
        path = %artifact_path.display(),
        error = %error,
        "worktree_local_artifact_remove_failed"
      );
      continue;
    }
    tracing::info!(
      target: "charliebot_tasks",
      thread_id,
      path = %artifact_path.display(),
      "worktree_local_artifact_removed"
    );
  }
}

/// Delete a git-detached residual worktree directory after explicit safety checks
/// (`src/infra/git.py:506`).
async fn remove_worktree_residue(
  wt_path: &Path,
  thread_id: &str,
  repo_path: &str,
  allowed_parent: &Path,
  expected_residue_name: &str,
) -> anyhow::Result<()> {
  assert_safe_worktree_cleanup_target(repo_path, wt_path, allowed_parent, expected_residue_name)?;
  if !wt_path.is_dir() {
    anyhow::bail!("refusing to remove non-directory worktree residue: {}", wt_path.display());
  }
  if contains_git_marker(wt_path) {
    anyhow::bail!("refusing to remove worktree residue with .git marker: {}", wt_path.display());
  }
  let target = wt_path.to_path_buf();
  tokio::task::spawn_blocking(move || std::fs::remove_dir_all(&target))
    .await
    .expect("residue removal task completes")?;
  if lexists(wt_path) {
    anyhow::bail!("failed to remove worktree residue: {}", wt_path.display());
  }
  tracing::info!(target: "charliebot_tasks", thread_id, path = %wt_path.display(), "worktree_residue_removed");
  Ok(())
}

/// Remove a git worktree and any safe git-detached residual directory (`src/infra/git.py:532`).
/// Returns whether git reported the remove as done.
pub async fn git_worktree_remove(
  repo_path: &str,
  wt_path: &Path,
  thread_id: &str,
  allowed_parent: &Path,
  expected_residue_name: &str,
) -> anyhow::Result<bool> {
  assert_safe_worktree_cleanup_target(repo_path, wt_path, allowed_parent, expected_residue_name)?;
  if lexists(wt_path) {
    if wt_path.is_symlink() {
      anyhow::bail!("refusing to remove worktree symlink: {}", wt_path.display());
    }
    if !wt_path.is_dir() {
      anyhow::bail!("refusing to remove non-directory worktree: {}", wt_path.display());
    }
    remove_local_worktree_artifacts(wt_path, thread_id).await;
  }
  let proc = match git_proc_bytes(
    Path::new(repo_path),
    &["worktree", "remove", "--force", &wt_path.display().to_string()],
    SUBPROCESS_GIT_READ_TIMEOUT_ASYNC,
  )
  .await
  {
    Ok(proc) => proc,
    Err(GitProcError::TimedOut) => {
      tracing::warn!(target: "charliebot_tasks", thread_id, path = %wt_path.display(), "worktree_remove_timeout");
      return Ok(false);
    }
    Err(GitProcError::Spawn(error)) => anyhow::bail!("git worktree remove failed to spawn: {error}"),
  };
  if proc.returncode != 0 {
    tracing::warn!(
      target: "charliebot_tasks",
      thread_id,
      stderr = %trimmed(&proc.stderr),
      "worktree_remove_failed"
    );
    return Ok(false);
  }
  if lexists(wt_path) {
    remove_worktree_residue(wt_path, thread_id, repo_path, allowed_parent, expected_residue_name).await?;
  }
  tracing::info!(target: "charliebot_tasks", thread_id, path = %wt_path.display(), "worktree_removed");
  Ok(true)
}

/// Remove a finished flow's worktree, reporting failure as a message (`src/infra/git.py:575`).
///
/// Returns an error message string when the remove fails so the caller can surface it in its
/// own flow, or `None` on success and when the flow has no worktree to clean. `label`,
/// `log_fields`, and the two event names carry the caller's flow identity; `owner_id` feeds
/// the remove/prune primitives' `thread_id` log field. A worktree without a branch raises
/// instead of reporting: the residue name and the prune both derive from the branch, so a
/// missing one is corrupt state, not a cleanup miss.
#[allow(clippy::too_many_arguments)] // the Python signature has the same nine parameters
pub async fn git_worktree_remove_reporting(
  repo_path: Option<&str>,
  worktree_path: Option<&str>,
  branch_name: Option<&str>,
  owner_id: &str,
  worktree_parent: &Path,
  log_fields: &serde_json::Map<String, serde_json::Value>,
  label: &str,
  fail_event: &str,
  remove_failed_event: &str,
) -> Option<String> {
  let (repo_path, worktree_path) = match (repo_path, worktree_path) {
    (Some(repo_path), Some(worktree_path)) => (repo_path, worktree_path),
    _ => return None,
  };
  let wt = Path::new(worktree_path);
  if !wt.exists() {
    return None;
  }
  let branch_name = match branch_name {
    Some(branch_name) => branch_name,
    None => panic!("owner {owner_id} has worktree_path but no branch_name"),
  };
  let removed =
    git_worktree_remove(repo_path, wt, owner_id, worktree_parent, &git_worktree_dir_name(branch_name)).await;
  match removed {
    Err(error) => {
      tracing::error!(
        target: "charliebot_tasks",
        worktree = %wt.display(),
        error = %error,
        log_fields = ?log_fields,
        "{fail_event}"
      );
      Some(format!("{label} cleanup failed for {}: {error}", wt.display()))
    }
    Ok(false) => {
      tracing::error!(
        target: "charliebot_tasks",
        worktree = %wt.display(),
        log_fields = ?log_fields,
        "{remove_failed_event}"
      );
      Some(format!("{label} cleanup failed for {}: git worktree remove reported failure", wt.display()))
    }
    Ok(true) => {
      git_worktree_prune(repo_path, owner_id).await;
      None
    }
  }
}

/// Prune stale worktree refs (`src/infra/git.py:623`).
pub async fn git_worktree_prune(repo_path: &str, thread_id: &str) {
  match git_proc_bytes(Path::new(repo_path), &["worktree", "prune"], SUBPROCESS_GIT_READ_TIMEOUT_ASYNC).await {
    Ok(_) => {}
    Err(GitProcError::TimedOut) => {
      tracing::warn!(target: "charliebot_tasks", thread_id, "worktree_prune_timeout");
    }
    Err(GitProcError::Spawn(error)) => panic!("git worktree prune failed to spawn: {error}"),
  }
}

/// Run `git fetch <remote> <branch>`; returns `(success, stderr)` (`src/infra/git.py:721`).
async fn git_fetch(repo_path: &Path, remote: &str, branch: &str) -> (bool, String) {
  let (ok, _, err) = git_remote(repo_path, &["fetch", remote, branch], "git fetch").await;
  (ok, err)
}

/// Run `git push origin <branch>`; returns `(success, stderr)` (`src/infra/git.py:727`).
pub async fn git_push_branch(repo_path: &Path, branch: &str) -> (bool, String) {
  let (ok, _, err) = git_remote(repo_path, &["push", "origin", branch], "git push").await;
  (ok, err)
}

/// Run `git push origin <local>:refs/heads/<remote>`; returns `(success, stderr)`
/// (`src/infra/git.py:733`). Git rejects non-fast-forward pushes by default, so this is
/// implicitly an FF-only push.
pub async fn git_push_refspec(repo_path: &Path, local_branch: &str, remote_branch: &str) -> (bool, String) {
  let (ok, _, err) = git_remote(
    repo_path,
    &["push", "origin", &format!("{local_branch}:refs/heads/{remote_branch}")],
    "git push refspec",
  )
  .await;
  (ok, err)
}

/// Whether *commit* exists in *repo_path* and is an ancestor of (or equal to) *branch*'s tip
/// (`src/infra/git.py:683`).
///
/// The completion evidence check behind every `landed:<branch>@<commit>` claim: a commit that
/// does not exist, a real commit the target branch does not contain, or an unresolvable target
/// all fail with an explicit reason — never a silent pass. An `origin/<branch>` target is
/// fetched first, so the judgment reads the published tip; a local branch is judged as-is.
pub async fn git_verify_commit_landed(repo_path: &Path, branch: &str, commit: &str) -> (bool, String) {
  if !is_commit_rev(commit) {
    return (false, format!("commit {} is not a git commit hash", pyfmt::str_repr(commit)));
  }
  if let Some(remote_branch) = branch.strip_prefix("origin/") {
    // Judge the published tip, not a possibly stale remote-tracking ref.
    let (fetched, fetch_detail) = git_fetch(repo_path, "origin", remote_branch).await;
    if !fetched {
      return (false, format!("could not fetch {branch} in {}: {fetch_detail}", repo_path.display()));
    }
  }
  let commit_object = format!("{commit}^{{commit}}");
  let checks: [(&str, Vec<&str>); 2] = [
    ("existence", vec!["cat-file", "-e", commit_object.as_str()]),
    ("ancestry", vec!["merge-base", "--is-ancestor", commit, branch]),
  ];
  for (label, args) in checks {
    match git_c_proc_bytes(repo_path, &args, SUBPROCESS_GIT_READ_TIMEOUT_ASYNC).await {
      Ok(proc) if proc.returncode == 0 => {}
      Ok(proc) => {
        let detail = trimmed(&proc.stderr);
        // Python reports `detail or proc.returncode` when the check fails.
        let detail = if detail.is_empty() { proc.returncode.to_string() } else { detail };
        return (false, format!("{label} check failed in {} ({branch}): {detail}", repo_path.display()));
      }
      Err(error) => return (false, format!("git {label} check for {commit} failed: {}", error.text())),
    }
  }
  (true, String::new())
}

/// Runs a `git` binary against *repo* and returns its output, for the fixtures of this module's tests.
#[cfg(test)]
pub(crate) fn run_git(repo: &Path, args: &[&str]) -> std::process::Output {
  std::process::Command::new("git").arg("-C").arg(repo).args(args).output().expect("git is installed")
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::path::{Path, PathBuf};
  use std::process::Command;

  use super::{collect_local_worktree_artifacts, contains_git_marker, git_verify_commit_landed, run_git};

  fn git(repo: &Path, args: &[&str]) -> std::process::Output {
    run_git(repo, args)
  }

  #[test]
  fn git_marker_search_finds_a_dot_git_entry_and_does_not_follow_links() {
    let tree = tempfile::tempdir().unwrap();
    fs::create_dir_all(tree.path().join("a").join("b")).unwrap();
    fs::write(tree.path().join("a").join("b").join(".git"), "gitdir: synthetic").unwrap();
    assert!(contains_git_marker(tree.path()));

    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join(".git"), "").unwrap();
    let inside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), inside.path().join("linked")).unwrap();
    assert!(!contains_git_marker(inside.path()));
    assert!(!contains_git_marker(&inside.path().join("missing")));
  }

  #[test]
  fn artifact_walk_prunes_artifacts_and_nested_worktrees_and_skips_links() {
    let tree = tempfile::tempdir().unwrap();
    let root = tree.path();
    for dir in
      [".venv/lib", ".git", "build", "nested-wt", "nested-wt/build", "deep/cache/.uv-cache", "deep/nested/build", "src"]
    {
      fs::create_dir_all(root.join(dir)).unwrap();
    }
    fs::write(root.join(".gitignore"), ".venv\nbuild\n").unwrap();
    fs::write(root.join("nested-wt").join(".git"), "gitdir: synthetic").unwrap();
    fs::write(root.join("deep").join("nested").join(".git"), "gitdir: synthetic").unwrap();
    std::os::unix::fs::symlink(root.join(".venv"), root.join("link-to-venv")).unwrap();

    let mut artifacts = Vec::new();
    collect_local_worktree_artifacts(root, &mut artifacts);
    let mut found: Vec<PathBuf> = artifacts.iter().map(|path| path.strip_prefix(root).unwrap().to_path_buf()).collect();
    found.sort();
    // A nested worktree directly under the root is still descended; one deeper down is pruned with its artifacts.
    let expected = [".venv", "build", "deep/cache/.uv-cache", "nested-wt/build"].map(PathBuf::from);
    assert_eq!(found, expected);
  }

  #[tokio::test]
  async fn synthetic_repository_verifies_landed_and_unlanded_commits() {
    // Python: tests/test_task_execution.py::test_landing_verification_negative_cases.
    let root = tempfile::tempdir().expect("temporary directory is available");
    let repo = root.path().join("repo");
    let origin = root.path().join("origin.git");
    fs::create_dir(&repo).expect("repository directory is created");
    assert!(git(&repo, &["init", "-q", "-b", "main"]).status.success());
    assert!(git(&repo, &["config", "user.email", "synthetic@example.test"]).status.success());
    assert!(git(&repo, &["config", "user.name", "Synthetic"]).status.success());
    fs::write(repo.join("tracked.txt"), "base\n").expect("synthetic file is written");
    assert!(git(&repo, &["add", "."]).status.success());
    assert!(git(&repo, &["commit", "-q", "-m", "base"]).status.success());
    let landed =
      String::from_utf8(git(&repo, &["rev-parse", "HEAD"]).stdout).expect("git sha is UTF-8").trim().to_string();
    assert!(
      Command::new("git")
        .args(["init", "-q", "--bare", "--initial-branch=main"])
        .arg(&origin)
        .status()
        .expect("git is installed")
        .success()
    );
    assert!(
      git(&repo, &["remote", "add", "origin", origin.to_str().expect("synthetic path is UTF-8")]).status.success()
    );
    assert!(git(&repo, &["push", "-q", "-u", "origin", "main"]).status.success());

    let (verified, reason) = git_verify_commit_landed(&repo, "origin/main", &landed).await;
    assert!(verified, "{reason}");
    let (verified, reason) = git_verify_commit_landed(&repo, "origin/main", &"f".repeat(40)).await;
    assert!(!verified);
    assert!(reason.starts_with("existence check failed in "));

    assert!(git(&repo, &["switch", "-q", "-c", "feature"]).status.success());
    fs::write(repo.join("unlanded.txt"), "unlanded\n").expect("synthetic file is written");
    assert!(git(&repo, &["add", "."]).status.success());
    assert!(git(&repo, &["commit", "-q", "-m", "unlanded"]).status.success());
    let unlanded =
      String::from_utf8(git(&repo, &["rev-parse", "HEAD"]).stdout).expect("git sha is UTF-8").trim().to_string();
    let (verified, reason) = git_verify_commit_landed(&repo, "origin/main", &unlanded).await;
    assert!(!verified);
    assert!(reason.starts_with("ancestry check failed in "));
    let (verified, reason) = git_verify_commit_landed(&repo, "origin/main", &landed).await;
    assert!(verified, "{reason}");
  }

  #[tokio::test]
  async fn invalid_hash_reports_python_string_repr() {
    // Python: tests/test_task_execution.py::test_landing_verification_negative_cases.
    let root = tempfile::tempdir().expect("temporary directory is available");
    let (verified, reason) = git_verify_commit_landed(root.path(), "main", "not-a-hash").await;
    assert!(!verified);
    assert_eq!(reason, "commit 'not-a-hash' is not a git commit hash");
  }
}
