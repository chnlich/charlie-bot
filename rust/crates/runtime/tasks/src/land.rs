//! The implement landing gate: the engine behind `charliebot land`.
//!
//! This module is the one owner of the implement landing. Every call re-reads git and GitHub state
//! and acts on it, and the landing record (`landing.json`) carries the state the queue's wake resumes,
//! so a review run that a trigger wake resumes and a later run in the task's own session take the same
//! path. The gate applies when the base is the repository's default branch and that branch declares a
//! workflow with a `pull_request` trigger; every other landing records `landed-no-ci`.
//!
//! Before any push and before the PR is created, the call checks the commits' messages and diffs, the decompressed
//! `*.gz` blobs they add or modify, the branch name and the PR's title and body for the exact value of a credential
//! the process holds (`outbound_checks`, the round's scanner). A hit refuses the call before anything leaves the
//! host, and the refusal names the kind, never the value. When the base branch's tree declares
//! `dev/check-skills-host-leak.sh`, the same check also verifies the configured hooks directory and scans the same
//! texts plus the commit identities and designated doc blobs for the local host patterns and the generic leak
//! rules; a repository that never declared the scanner keeps the credential stage alone.
//!
//! Before any push, a diff that changes a CharlieBot web page refuses the call unless its PR body names the
//! approved demo, a `View: <URL>` line naming the full URL, path and query, that opens the changed state, and a
//! screenshot at each window size (`web_screenshots`). The View lines are written into the head commit's trailer
//! block before the push, so the commit that reaches the
//! base branch carries them for the release switch's live capture.
//!
//! Under the gate the call rebases onto `origin/<base>`, pushes the task branch with
//! `--force-with-lease`, keeps one draft PR for the branch, and registers the landing queue's command
//! (`charliebot land queue run --sha`, `land_queue`) with the head's CI pass time, so the record reads
//! `waiting`. That command waits for CI, enters the queue, and, once it holds the turn, rebases onto the
//! base, pushes the task branch, waits for CI on the rebased head, and writes `<sha>:<base>` in one
//! atomic push that also releases the turn and the entry. The command works in a scratch worktree of
//! its own, detached at the queued commit and removed when it ends, so the session's worktree takes no
//! part: its checkout, edits and commits cannot stop a queued landing. The session's landing record names
//! the head that is current, and a newer `land` call of the task replaces the record before it arms
//! its own command. Only a commit that passed CI on itself reaches the base branch. Both clocks — the
//! 10-minute not-started clock and the 90-minute timeout — run per head sha from the time that sha was
//! pushed to the task branch, so a rebase starts both again
//! and a busy main costs `rebase_retries`, never a timeout. Main is never pushed on any state other than
//! a pass, and no state falls back to a direct push.
//!
//! A landing that moves the base branch, gated or not, leaves no open PR of the task branch behind:
//! the command reads the branch's PRs back after the push and closes each one GitHub has not marked
//! merged, so `landed` and `landed-no-ci` both mean the PR reads merged or closed.
//!
//! A refused write of the base starts another round only when the remote base verifiably advanced
//! past the write's leased base; every other refusal — a hook, a dead transport, a timeout, an
//! auth error — ends the call at once with git's stderr verbatim.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::DirBuilder;
use std::future::Future;
use std::io::Read as _;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use charliebot_backend_check::{Hit, Vocabulary, gate};
use charliebot_core::git::ancestry_args;
use charliebot_core::home::CHARLIEBOT_HOME_ENV;
use charliebot_core::text_repr::string_field;
use charliebot_registry::Registry;
use charliebot_registry::pages::WEB_DIR_NAME;
use charliebot_store::layout;
use charliebot_store::metadata::read_session_metadata;
use charliebot_store::run_records::{self, RunRecord};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::signal::unix::{SignalKind, signal};
use uuid::Uuid;

use crate::git;
use crate::github_api::{RestOutcome, rest_get, run_gh};
use crate::land_queue::{self, Cadence};
use crate::landing_worktree::{self, LandingWorktree};
use crate::review_text;
use crate::web_screenshots;

/// The landing record's file beside a task session's `metadata.json` (`LANDING_FILE_NAME`).
pub const LANDING_FILE_NAME: &str = "landing.json";

/// An expected workflow still unregistered this many seconds after its head sha was pushed is `ci-not-started`.
pub const NOT_STARTED_SECS: i64 = 600;
/// Checks still unfinished this many seconds after their head sha was pushed are `ci-timeout`.
pub const TIMEOUT_SECS: i64 = 5400;
/// Rebase rounds one landing call allows before it refuses to keep racing main.
const MAX_LANDING_ROUNDS: usize = 5;
/// The longest a turn holder waits for CI on the head it is about to land, in seconds. A test overrides it.
pub const HOLDER_CI_WAIT_SECS: i64 = 1800;
/// GitHub marks a PR merged about a second after its head sha reaches the base branch; a landing
/// waits this long for that mark on a PR whose head is the landed sha before it closes the PR.
const PR_MERGE_WAIT: Duration = Duration::from_secs(30);
const PR_MERGE_POLL: Duration = Duration::from_secs(2);

pub const STATE_LANDED: &str = "landed";
pub const STATE_LANDED_NO_CI: &str = "landed-no-ci";
pub const STATE_WAITING: &str = "waiting";
pub const STATE_REBASE_CONFLICT: &str = "rebase-conflict";
pub const STATE_CI_FAILED: &str = "ci-failed";
pub const STATE_CI_NOT_STARTED: &str = "ci-not-started";
pub const STATE_CI_TIMEOUT: &str = "ci-timeout";
pub const STATE_PR_MERGE_REQUIRED: &str = "pr-merge-required";

/// PR text supplied to `charliebot land`. On a gated path an omitted value derives from the task branch's single
/// commit; a branch of several commits needs both values. `evidence_dir` holds the demo and the screenshots that a
/// web page change's body names by file name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LandOptions {
  pub title: Option<String>,
  pub body: Option<String>,
  pub evidence_dir: Option<PathBuf>,
}

impl LandOptions {
  /// Explicit titles must contain non-whitespace text, and an explicit evidence directory must exist, before any
  /// landing work starts.
  pub fn validate(&self) -> anyhow::Result<()> {
    if self.title.as_deref().is_some_and(|title| title.trim().is_empty()) {
      anyhow::bail!("--title must not be empty");
    }
    if let Some(dir) = &self.evidence_dir
      && !dir.is_dir()
    {
      anyhow::bail!("--evidence-dir {} is not a directory", dir.display());
    }
    Ok(())
  }
}

/// One landed commit's check row: a check run or a commit status on the head sha.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckRow {
  pub name: String,
  pub conclusion: Option<String>,
  pub url: Option<String>,
  pub completed_at: Option<String>,
}

/// The landing record the plan's schema pins: one JSON object on stdout, the same content in the
/// task session directory's `landing.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LandingRecord {
  pub state: String,
  pub repo: String,
  pub base: String,
  pub sha: String,
  pub pr_url: Option<String>,
  pub checks: Vec<CheckRow>,
  pub head_pushed_at: Option<String>,
  pub ci_passed_at: Option<String>,
  pub pushed_at: Option<String>,
  pub rebase_retries: i64,
  /// The waiting entry of the landing queue this landing is in (`refs/land-queue/...`), while it waits.
  pub queue_ref: Option<String>,
}

/// What one landing call decided, carried to the CLI for the per-state exit code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LandState {
  Landed,
  LandedNoCi,
  Waiting,
  RebaseConflict,
  CiFailed,
  CiNotStarted,
  CiTimeout,
  PrMergeRequired,
}

impl LandState {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Landed => STATE_LANDED,
      Self::LandedNoCi => STATE_LANDED_NO_CI,
      Self::Waiting => STATE_WAITING,
      Self::RebaseConflict => STATE_REBASE_CONFLICT,
      Self::CiFailed => STATE_CI_FAILED,
      Self::CiNotStarted => STATE_CI_NOT_STARTED,
      Self::CiTimeout => STATE_CI_TIMEOUT,
      Self::PrMergeRequired => STATE_PR_MERGE_REQUIRED,
    }
  }

  /// `landed` and `landed-no-ci` exit 0; every other state carries its own code, so the caller can
  /// branch on the exit code alone. 1 is the CLI's error code and 2 its usage-error code: a failed
  /// call that printed no state exits 1, and `waiting` must not read as that.
  pub fn exit_code(self) -> i32 {
    match self {
      Self::Landed | Self::LandedNoCi => 0,
      Self::Waiting => 8,
      Self::RebaseConflict => 3,
      Self::CiFailed => 4,
      Self::CiNotStarted => 5,
      Self::CiTimeout => 6,
      Self::PrMergeRequired => 7,
    }
  }
}

impl TryFrom<&str> for LandState {
  type Error = String;

  fn try_from(state: &str) -> Result<Self, Self::Error> {
    match state {
      STATE_LANDED => Ok(Self::Landed),
      STATE_LANDED_NO_CI => Ok(Self::LandedNoCi),
      STATE_WAITING => Ok(Self::Waiting),
      STATE_REBASE_CONFLICT => Ok(Self::RebaseConflict),
      STATE_CI_FAILED => Ok(Self::CiFailed),
      STATE_CI_NOT_STARTED => Ok(Self::CiNotStarted),
      STATE_CI_TIMEOUT => Ok(Self::CiTimeout),
      STATE_PR_MERGE_REQUIRED => Ok(Self::PrMergeRequired),
      other => Err(format!("unknown landing state {other}")),
    }
  }
}

/// The repo, base, branch, worktree and session directory one landing acts on, all from the task record. The worktree
/// is where the landing's git runs: the session's own, or a queued command's scratch worktree.
#[derive(Clone)]
pub(crate) struct TaskLanding {
  session_id: String,
  repo: String,
  worktree: PathBuf,
  repo_path: PathBuf,
  base_branch: String,
  branch_name: String,
  session_dir: PathBuf,
  /// The program that registers and cancels the queued command's wake.
  queue_program: PathBuf,
}

impl TaskLanding {
  /// The queue's git side: the task's worktree, whose `origin` holds the queue refs.
  fn queue_site(&self) -> land_queue::Site {
    land_queue::Site { worktree: self.worktree.clone() }
  }

  /// The queue's session side: the waiting entries carry this session, and its wakes are its triggers.
  fn queue_owner(&self) -> land_queue::Owner {
    land_queue::Owner {
      session_id: self.session_id.clone(),
      session_dir: self.session_dir.clone(),
      worktree: self.worktree.clone(),
      program: self.queue_program.clone(),
    }
  }

  /// The newest run record that carries a branch and worktree names the landing, plus the task
  /// spec's repo and base. A run record inherits its predecessor's repo fields, so the newest
  /// record with a branch always names the live landing venue.
  async fn load(home: &Path, task_session_id: &str) -> anyhow::Result<Self> {
    let session_dir = charliebot_core::config::sessions_dir_in(home).join(task_session_id);
    if !session_dir.is_dir() {
      anyhow::bail!("task session {task_session_id} does not exist in {}", home.display());
    }
    let meta_path = layout::metadata_path(&session_dir);
    let meta = read_session_metadata(&meta_path)
      .map_err(|error| anyhow::anyhow!("{}: {error}", meta_path.display()))?
      .ok_or_else(|| anyhow::anyhow!("{} has no session metadata", meta_path.display()))?;
    let task = meta.task.as_ref().ok_or_else(|| anyhow::anyhow!("task {task_session_id} records no task spec"))?;
    let runs_dir = layout::runs_dir(&session_dir);
    let mut runs: Vec<RunRecord> = Vec::new();
    for entry in std::fs::read_dir(&runs_dir).map_err(|error| anyhow::anyhow!("{}: {error}", runs_dir.display()))? {
      let entry = entry.map_err(|error| anyhow::anyhow!("{}: {error}", runs_dir.display()))?;
      let path = entry.path().join(run_records::RUN_METADATA_NAME);
      match run_records::read_run_record_file(&path) {
        Ok(run) => runs.push(run),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(anyhow::anyhow!("{}: {error}", path.display())),
      }
    }
    runs.sort_by_key(|run| (run.started_at, run.id.clone()));
    let latest = runs
      .iter()
      .rev()
      .find(|run| {
        run.branch_name.as_deref().is_some_and(|branch| !branch.is_empty())
          && run.worktree_path.as_deref().is_some_and(|path| !path.is_empty())
      })
      .ok_or_else(|| anyhow::anyhow!("task {task_session_id} has no run record with a branch and worktree"))?;
    let repo_path = task
      .repo_path
      .clone()
      .or_else(|| latest.repo_path.clone())
      .filter(|value| !value.is_empty())
      .ok_or_else(|| anyhow::anyhow!("task {task_session_id} records no repository"))?;
    let base_branch = task
      .base_branch
      .clone()
      .or_else(|| latest.base_branch.clone())
      .filter(|value| !value.is_empty())
      .ok_or_else(|| anyhow::anyhow!("task {task_session_id} records no base branch"))?;
    let worktree = latest.worktree_path.clone().expect("the filter keeps a worktree path");
    let branch_name = latest.branch_name.clone().expect("the filter keeps a branch name");
    let (ok, url, err) =
      git::git_stdout(Path::new(&repo_path), &["remote", "get-url", "origin"], GIT_READ_TIMEOUT, "git remote get-url")
        .await;
    if !ok {
      anyhow::bail!("git remote get-url origin failed in {repo_path}: {err}");
    }
    let repo = git::repo_from_remote_url(&url)
      .ok_or_else(|| anyhow::anyhow!("origin's URL {url} in {repo_path} names no owner/repo path"))?;
    Ok(Self {
      session_id: task_session_id.to_string(),
      repo,
      worktree: PathBuf::from(worktree),
      repo_path: PathBuf::from(repo_path),
      base_branch,
      branch_name,
      session_dir,
      queue_program: PathBuf::from("charliebot"),
    })
  }
}

/// The scanner that the credential check runs: the same file the improve round's writes run
/// (`dev/check-skills-host-leak.sh`), with the pattern loader it sources. The release binary carries both files, so the
/// landing needs no install step and holds no second matcher.
const LEAK_SCANNER: &str = include_str!("../../../../../dev/check-skills-host-leak.sh");
const LEAK_PATTERNS: &str = include_str!("../../../../../dev/leak-patterns.sh");
/// The scanner's exit code for a credential hit (`dev/check-skills-host-leak.sh`).
const CREDENTIAL_HIT_EXIT: i32 = 3;
const LEAK_SCAN_TIMEOUT: Duration = Duration::from_secs(60);

/// A private directory that holds the scanner, its pattern loader and the values file of one check. The directory is
/// removed when the check ends, so the values never outlive the landing call.
struct LeakStage {
  dir: PathBuf,
  home: PathBuf,
}

impl LeakStage {
  /// Creates the directory, mode 0700, and writes both scanner files into it. Any failure names the path it failed on.
  fn create(home: &Path) -> anyhow::Result<Self> {
    let dir = std::env::temp_dir().join(format!(
      "charliebot-land-check-{}-{}",
      std::process::id(),
      OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    DirBuilder::new()
      .mode(0o700)
      .create(&dir)
      .map_err(|error| anyhow::anyhow!("cannot create the leak check directory {}: {error}", dir.display()))?;
    let stage = LeakStage { dir, home: home.to_path_buf() };
    for (name, text) in [("check-skills-host-leak.sh", LEAK_SCANNER), ("leak-patterns.sh", LEAK_PATTERNS)] {
      let path = stage.dir.join(name);
      std::fs::write(&path, text)
        .map_err(|error| anyhow::anyhow!("cannot write the leak scanner {}: {error}", path.display()))?;
    }
    Ok(stage)
  }

  fn scanner(&self) -> PathBuf {
    self.dir.join("check-skills-host-leak.sh")
  }

  fn values(&self) -> PathBuf {
    self.dir.join("credential-values")
  }
}

impl Drop for LeakStage {
  fn drop(&mut self) {
    if let Err(error) = std::fs::remove_dir_all(&self.dir) {
      tracing::warn!(target: "charliebot_tasks", dir = %self.dir.display(), error = %error, "leak_check_stage_remove_failed");
    }
  }
}

/// Runs the scanner under bash with the profile home as `CHARLIEBOT_HOME`, its standard input read from *input* when
/// given. Returns the exit code, the standard output and the standard error. A scanner that cannot start is an error
/// that names it, and so is one that does not finish in time.
async fn run_scanner(stage: &LeakStage, args: &[&str], input: Option<&Path>) -> anyhow::Result<(i32, Vec<u8>, String)> {
  let scanner = stage.scanner();
  let mut command = tokio::process::Command::new("bash");
  command.arg(&scanner).args(args).env(CHARLIEBOT_HOME_ENV, &stage.home).kill_on_drop(true);
  if let Some(path) = input {
    let file = std::fs::File::open(path)
      .map_err(|error| anyhow::anyhow!("cannot open the text for the leak check {}: {error}", path.display()))?;
    command.stdin(Stdio::from(file));
  }
  let output = tokio::time::timeout(LEAK_SCAN_TIMEOUT, command.output())
    .await
    .map_err(|_| {
      anyhow::anyhow!("the leak scanner {} timed out after {}s", scanner.display(), LEAK_SCAN_TIMEOUT.as_secs())
    })?
    .map_err(|error| anyhow::anyhow!("cannot run the leak scanner {} with bash: {error}", scanner.display()))?;
  let code = output
    .status
    .code()
    .ok_or_else(|| anyhow::anyhow!("the leak scanner {} was killed by a signal", scanner.display()))?;
  Ok((code, output.stdout, String::from_utf8_lossy(&output.stderr).trim().to_string()))
}

/// One text of the landing that a check scans, with the field its hit would name. The text is bytes so a
/// decompressed `*.gz` blob checks the same way as a commit message.
struct CheckedText {
  field: String,
  text: Vec<u8>,
}

/// Writes each text into the stage and returns its `field<TAB>path` list file there.
fn write_check_list(stage: &LeakStage, name: &str, texts: &[CheckedText]) -> anyhow::Result<PathBuf> {
  let mut list = String::new();
  for (index, checked) in texts.iter().enumerate() {
    let path = stage.dir.join(format!("{name}-text-{index}"));
    std::fs::write(&path, &checked.text)
      .map_err(|error| anyhow::anyhow!("cannot write the text for the leak check {}: {error}", path.display()))?;
    list.push_str(&format!("{}\t{}\n", checked.field, path.display()));
  }
  let list_path = stage.dir.join(format!("{name}-list"));
  std::fs::write(&list_path, list)
    .map_err(|error| anyhow::anyhow!("cannot write the leak check list {}: {error}", list_path.display()))?;
  Ok(list_path)
}

/// Checks the landing's texts for the exact value of a credential this process holds, in one scanner call: the
/// scanner's `--credential-check-list` reads each field's text from a file of the private stage. A hit refuses the
/// landing, and the message names the kind and the field that the scanner printed; the value never appears in it.
async fn check_texts(stage: &LeakStage, texts: &[CheckedText]) -> anyhow::Result<()> {
  let list_path = write_check_list(stage, "cred", texts)?;
  let values = stage.values().display().to_string();
  let list_path = list_path.display().to_string();
  let (code, stdout, stderr) = run_scanner(stage, &["--credential-check-list", &values, &list_path], None).await?;
  match code {
    0 => Ok(()),
    CREDENTIAL_HIT_EXIT => anyhow::bail!(
      "landing refused before any push or PR create: {}; the kind named is the reason",
      String::from_utf8_lossy(&stdout).trim()
    ),
    _ => anyhow::bail!("the leak scanner refused the check of {} texts (exit {code}): {stderr}", texts.len()),
  }
}

/// Checks the landing's texts with the scanner's leak rules in one `--check-list` call: the local host patterns,
/// plus the built-in rules unless *host_patterns_only*. `--require-local-patterns` makes the scan fail closed when
/// the profile home's pattern file is missing, unreadable or empty. A pattern hit refuses the landing with the
/// scanner's own `Leak hit in <field>` report; a setup refusal (exit 2) refuses it the same way.
async fn check_leak_texts(
  stage: &LeakStage,
  name: &str,
  texts: &[CheckedText],
  host_patterns_only: bool,
) -> anyhow::Result<()> {
  let list_path = write_check_list(stage, name, texts)?.display().to_string();
  let mut args = vec!["--require-local-patterns"];
  if host_patterns_only {
    args.push("--host-patterns-only");
  }
  args.extend(["--check-list", list_path.as_str()]);
  let (code, stdout, stderr) = run_scanner(stage, &args, None).await?;
  match code {
    0 => Ok(()),
    1 => anyhow::bail!("landing refused before any push or PR create: {}", String::from_utf8_lossy(&stdout).trim()),
    _ => anyhow::bail!("the leak scanner refused the {name} check (exit {code}): {stderr}"),
  }
}

/// Whether the landing's base ref declares the repository leak scanner: `git cat-file -e
/// <base_ref>:dev/check-skills-host-leak.sh`. The declaration lives in the base's tree, not the task branch's, so
/// deleting the scanner on the branch does not switch the host-pattern, doc and hook checks off — while a
/// repository that never declared it (a private one, which its own hostnames describe) keeps the credential
/// checks alone.
async fn base_declares_scanner(worktree: &Path, base_ref: &str) -> bool {
  let spec = format!("{base_ref}:dev/check-skills-host-leak.sh");
  git::git_stdout(worktree, &["cat-file", "-e", &spec], GIT_READ_TIMEOUT, "git cat-file").await.0
}

/// The text a commit's patch adds to the remote: its added lines and file headers. A removed line adds no bytes to the
/// remote, and a cleanup that deletes a leaked literal must not be refused for the literal it deletes.
fn added_text(patch: &str) -> String {
  let mut text = String::new();
  for line in patch.lines() {
    if line.starts_with("+++ ") || line.starts_with("diff --git ") {
      text.push_str(line);
    } else if let Some(added) = line.strip_prefix('+') {
      text.push_str(added);
    } else {
      continue;
    }
    text.push('\n');
  }
  text
}

/// When the checkout pins `core.hooksPath`, every commit and push of this repository runs its hooks — a stale
/// directory or a missing hook would skip them silently, so the landing verifies the configured directory before
/// trusting the push. An unset `core.hooksPath` is not an error: a checkout without hooks pins nothing.
async fn check_hooks_path(repo_path: &Path) -> anyhow::Result<()> {
  let (ok, configured, _) =
    git::git_stdout(repo_path, &["config", "--get", "core.hooksPath"], GIT_READ_TIMEOUT, "git config").await;
  if !ok || configured.is_empty() {
    return Ok(());
  }
  let dir = PathBuf::from(&configured);
  let dir = if dir.is_absolute() { dir } else { repo_path.join(dir) };
  if !dir.is_dir() {
    anyhow::bail!(
      "core.hooksPath of {} names {configured}, which resolves to no directory; the push hooks would not run",
      repo_path.display()
    );
  }
  for hook in ["pre-commit", "commit-msg", "pre-push"] {
    let executable = std::fs::metadata(dir.join(hook))
      .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
      .unwrap_or(false);
    if !executable {
      anyhow::bail!(
        "core.hooksPath of {} names {configured}, which lacks an executable {hook}; the push hooks would not run",
        repo_path.display()
      );
    }
  }
  Ok(())
}

/// The decompressed content of a gzip blob; an empty or corrupt member is a refusal, so a push never skips its
/// content. `MultiGzDecoder` reads concatenated members so trailing data cannot hide past the first member.
fn gunzip(name: &str, blob: &[u8]) -> anyhow::Result<Vec<u8>> {
  if blob.is_empty() {
    anyhow::bail!("cannot decompress {name} for the leak check: unexpected end of file");
  }
  let mut plain = Vec::new();
  flate2::read::MultiGzDecoder::new(blob)
    .read_to_end(&mut plain)
    .map_err(|error| anyhow::anyhow!("cannot decompress {name} for the leak check: {error}"))?;
  Ok(plain)
}

/// Whether *path* is a designated doc blob of the commit: `skills/**/*.md`, `prompts/**/*.md`, or a path the
/// commit's `web/features/*/leak_scan_files.txt` manifests name.
fn designated_doc(path: &str, manifest_paths: &std::collections::BTreeSet<String>) -> bool {
  ((path.starts_with("skills/") || path.starts_with("prompts/")) && path.ends_with(".md"))
    || manifest_paths.contains(path)
}

/// Reads the blob of *path* at *sha*; a read failure is an error that names both.
async fn blob_at(worktree: &Path, sha: &str, path: &str) -> anyhow::Result<Vec<u8>> {
  let spec = format!("{sha}:{path}");
  let (ok, blob, err) = git::git_bytes(worktree, &["show", &spec], GIT_READ_TIMEOUT, "git show").await;
  if !ok {
    anyhow::bail!("git show {spec} failed in {}: {err}", worktree.display());
  }
  Ok(blob)
}

/// The outbound checks of a landing, run before each push and before the pull request is created. The credential
/// stage checks every repository: the message and diff of each commit ahead of `base_ref`, the decompressed
/// `*.gz` blobs those commits add or modify, the task branch name, and the PR title and body when given. When the
/// base ref declares `dev/check-skills-host-leak.sh`, the same stage also verifies the configured hooks path and
/// runs the scanner's leak rules — the local host patterns over every text and identity, and the generic rules
/// over the messages, PR text and designated doc blobs. Any refusal stops the call before a byte leaves the host.
async fn outbound_checks(
  home: &Path,
  worktree: &Path,
  repo_path: &Path,
  branch_name: &str,
  base_ref: &str,
  pr_text: Option<&ResolvedPrText>,
) -> anyhow::Result<()> {
  let stage = LeakStage::create(home)?;
  let (code, values, stderr) = run_scanner(&stage, &["--credential-values"], None).await?;
  if code != 0 {
    anyhow::bail!("the leak scanner could not read the credential values under {}: {stderr}", home.display());
  }
  std::fs::write(stage.values(), values)
    .map_err(|error| anyhow::anyhow!("cannot write the credential values {}: {error}", stage.values().display()))?;

  let declared = base_declares_scanner(worktree, base_ref).await;
  let mut cred_texts: Vec<CheckedText> = Vec::new();
  let mut all_texts: Vec<CheckedText> = Vec::new();
  let mut doc_texts: Vec<CheckedText> = Vec::new();
  let range = format!("{base_ref}..HEAD");
  let (ok, shas, err) = git::git_stdout(worktree, &["rev-list", &range], GIT_READ_TIMEOUT, "git rev-list").await;
  if !ok {
    anyhow::bail!("git rev-list {range} failed in {}: {err}", worktree.display());
  }
  for sha in shas.lines().filter(|line| !line.is_empty()) {
    let (ok, message, err) =
      git::git_stdout(worktree, &["show", "-s", "--format=%B", sha], GIT_READ_TIMEOUT, "git show").await;
    if !ok {
      anyhow::bail!("git show {sha} failed in {}: {err}", worktree.display());
    }
    let field = format!("commit {sha} message");
    cred_texts.push(CheckedText { field: field.clone(), text: message.clone().into_bytes() });
    if declared {
      all_texts.push(CheckedText { field: field.clone(), text: message.clone().into_bytes() });
      doc_texts.push(CheckedText { field, text: message.into_bytes() });
      let (ok, identity, err) =
        git::git_stdout(worktree, &["show", "-s", "--format=%an <%ae>%n%cn <%ce>", sha], GIT_READ_TIMEOUT, "git show")
          .await;
      if !ok {
        anyhow::bail!("git show identity {sha} failed in {}: {err}", worktree.display());
      }
      all_texts.push(CheckedText { field: format!("commit {sha} identity"), text: identity.into_bytes() });
    }
    let (ok, diff, err) = git::git_stdout(
      worktree,
      &["show", "--format=", "--patch", "--no-color", "--no-ext-diff", sha],
      GIT_READ_TIMEOUT,
      "git show",
    )
    .await;
    if !ok {
      anyhow::bail!("git show --patch {sha} failed in {}: {err}", worktree.display());
    }
    let field = format!("commit {sha} diff");
    if declared {
      all_texts.push(CheckedText { field: field.clone(), text: added_text(&diff).into_bytes() });
    }
    cred_texts.push(CheckedText { field, text: diff.into_bytes() });

    // The commit's designated doc paths: the skills and prompts glob plus the paths its own
    // web/features/*/leak_scan_files.txt manifests name.
    let mut manifest_paths = std::collections::BTreeSet::new();
    if declared {
      let manifest_glob = format!("{WEB_DIR_NAME}/features/*/leak_scan_files.txt");
      let (ok, manifests, err) = git::git_stdout(
        worktree,
        &["ls-tree", "-r", "--name-only", sha, "--", manifest_glob.as_str()],
        GIT_READ_TIMEOUT,
        "git ls-tree",
      )
      .await;
      if !ok {
        anyhow::bail!("git ls-tree {sha} failed in {}: {err}", worktree.display());
      }
      for manifest in manifests.lines().filter(|line| !line.is_empty()) {
        let text = String::from_utf8(blob_at(worktree, sha, manifest).await?)
          .map_err(|_| anyhow::anyhow!("the doc manifest {sha}:{manifest} is not UTF-8"))?;
        manifest_paths.extend(text.lines().map(str::trim).filter(|line| !line.is_empty()).map(str::to_string));
      }
    }

    let (ok, touched, err) = git::git_stdout(
      worktree,
      &["diff-tree", "--root", "--no-commit-id", "--name-only", "-r", "--diff-filter=ACMR", sha],
      GIT_READ_TIMEOUT,
      "git diff-tree",
    )
    .await;
    if !ok {
      anyhow::bail!("git diff-tree {sha} failed in {}: {err}", worktree.display());
    }
    for path in touched.lines().filter(|line| !line.is_empty()) {
      if path.ends_with(".gz") {
        let blob = blob_at(worktree, sha, path).await?;
        let plain = gunzip(&format!("{sha}:{path}"), &blob)?;
        let field = format!("commit {sha} blob {path}");
        cred_texts.push(CheckedText { field: field.clone(), text: plain.clone() });
        if declared {
          all_texts.push(CheckedText { field, text: plain });
        }
      }
      if declared && designated_doc(path, &manifest_paths) {
        let blob = blob_at(worktree, sha, path).await?;
        doc_texts.push(CheckedText { field: format!("commit {sha} doc {path}"), text: blob });
      }
    }
  }
  cred_texts.push(CheckedText { field: "branch name".to_string(), text: branch_name.as_bytes().to_vec() });
  if declared {
    all_texts.push(CheckedText { field: "branch name".to_string(), text: branch_name.as_bytes().to_vec() });
  }
  if let Some(text) = pr_text {
    cred_texts.push(CheckedText { field: "PR title".to_string(), text: text.title.clone().into_bytes() });
    cred_texts.push(CheckedText { field: "PR body".to_string(), text: text.body.clone().into_bytes() });
    if declared {
      all_texts.push(CheckedText { field: "PR title".to_string(), text: text.title.clone().into_bytes() });
      all_texts.push(CheckedText { field: "PR body".to_string(), text: text.body.clone().into_bytes() });
      doc_texts.push(CheckedText { field: "PR title".to_string(), text: text.title.clone().into_bytes() });
      doc_texts.push(CheckedText { field: "PR body".to_string(), text: text.body.clone().into_bytes() });
    }
  }

  check_texts(&stage, &cred_texts).await?;
  if declared {
    check_hooks_path(repo_path).await?;
    check_leak_texts(&stage, "all", &all_texts, true).await?;
    check_leak_texts(&stage, "doc", &doc_texts, false).await?;
  }
  Ok(())
}

/// The paths the backend-literal check reads for a landing: each changed file that still exists, and for a changed Rust
/// file its crate. A `cfg(test)` module is test code by the declaration in its parent file, and that parent may be
/// unchanged, so the crate's sources are read together. A file the diff removed is left out; any other failure to stat
/// a changed path is an error, so a file the check cannot see never passes.
fn literal_check_paths(worktree: &Path, changed: &[String]) -> anyhow::Result<Vec<PathBuf>> {
  let mut paths = BTreeSet::new();
  for relative in changed {
    let file = worktree.join(relative);
    match std::fs::metadata(&file) {
      Ok(meta) if !meta.is_file() => continue,
      Ok(_) => {}
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
      Err(error) => return Err(error).with_context(|| format!("cannot read the changed path {}", file.display())),
    }
    let is_rust = file.extension().is_some_and(|ext| ext == "rs");
    let path = if is_rust { crate_of(&file, worktree)?.unwrap_or(file) } else { file };
    paths.insert(path);
  }
  Ok(paths.into_iter().collect())
}

/// The directory of the nearest crate above `file` inside `worktree`: the first ancestor that holds a `Cargo.toml`.
fn crate_of(file: &Path, worktree: &Path) -> anyhow::Result<Option<PathBuf>> {
  for dir in file.ancestors().skip(1).take_while(|dir| dir.starts_with(worktree)) {
    let manifest = dir.join("Cargo.toml");
    if manifest.try_exists().with_context(|| format!("cannot read {}", manifest.display()))? {
      return Ok(Some(dir.to_path_buf()));
    }
  }
  Ok(None)
}

/// The lines a landing adds, per file: the new-side line numbers that a diff marks as added, keyed by the path relative
/// to the worktree.
type AddedLines = BTreeMap<PathBuf, BTreeSet<usize>>;

/// Refuses a landing whose `paths` hold a hardcoded backend choice that the check does not exempt on a line the landing
/// adds, and names each site. `added` holds those lines. The check reads the host's vocabulary: the registered packages
/// and the profile's `config.yaml`.
fn refuse_backend_literals(
  registry: &Registry,
  home: &Path,
  worktree: &Path,
  paths: &[PathBuf],
  added: &AddedLines,
) -> anyhow::Result<()> {
  let roots = gate::roots(Some(worktree), home)?;
  let vocab = Vocabulary::load(registry, home, None)?;
  let hits = keep_added_sites(gate::open_hits(&vocab, &roots, paths)?, worktree, added)?;
  if hits.is_empty() {
    return Ok(());
  }
  anyhow::bail!(
    "landing refused before any push or PR create: a hardcoded backend choice on a line this landing adds:\n{}",
    gate::report(&hits, roots.repo.as_deref())
  )
}

/// The hits that sit on a line the landing adds. The scan canonicalizes the paths it reads, so each hit is matched
/// under the canonical worktree; a hit outside the worktree is no line of the landing.
fn keep_added_sites(hits: Vec<Hit>, worktree: &Path, added: &AddedLines) -> anyhow::Result<Vec<Hit>> {
  let root =
    std::fs::canonicalize(worktree).with_context(|| format!("cannot read the worktree {}", worktree.display()))?;
  let is_added = |hit: &Hit| {
    let lines = hit.path.strip_prefix(&root).ok().and_then(|relative| added.get(relative));
    lines.is_some_and(|lines| lines.contains(&hit.line))
  };
  Ok(hits.into_iter().filter(is_added).collect())
}

/// The backend-literal check of a landing. It scans the files the task branch changes against the base, whole, and
/// refuses only a literal on a line the landing adds: a line that the task base or the default branch already holds is
/// not a site of this landing.
async fn backend_literal_check(
  registry: &Registry,
  home: &Path,
  worktree: &Path,
  base_ref: &str,
) -> anyhow::Result<()> {
  let changed = changed_paths(worktree, base_ref).await?;
  let default_ref = default_branch_ref(worktree).await?;
  let added = landing_added_lines(worktree, base_ref, &default_ref).await?;
  refuse_backend_literals(registry, home, worktree, &literal_check_paths(worktree, &changed)?, &added)
}

/// The default branch's remote-tracking ref, fetched so that the comparison reads its current tip. A default branch
/// that cannot be read, fetched or resolved is an error: the check never narrows to the task base alone.
async fn default_branch_ref(worktree: &Path) -> anyhow::Result<String> {
  let (branch, _) = git::git_remote_default_branch_and_tip(worktree)
    .await
    .map_err(|error| anyhow::anyhow!("cannot read the default branch of {}: {error}", worktree.display()))?;
  fetch_base(worktree, &branch).await?;
  let reference = format!("origin/{branch}");
  git::git_rev_parse(worktree, &reference)
    .await
    .ok_or_else(|| anyhow::anyhow!("{reference} does not resolve in {}", worktree.display()))?;
  Ok(reference)
}

/// The lines a landing adds: those that the three-dot diff against the task base and the three-dot diff against the
/// default branch both mark as added. Each diff numbers the head's lines, so the two sets compare line for line.
async fn landing_added_lines(worktree: &Path, base_ref: &str, default_ref: &str) -> anyhow::Result<AddedLines> {
  let against_base = added_lines(worktree, &format!("{base_ref}...HEAD")).await?;
  let against_default = added_lines(worktree, &format!("{default_ref}...HEAD")).await?;
  let mut both = AddedLines::new();
  for (path, lines) in against_base {
    let Some(default_lines) = against_default.get(&path) else { continue };
    let shared: BTreeSet<usize> = lines.intersection(default_lines).copied().collect();
    if !shared.is_empty() {
      both.insert(path, shared);
    }
  }
  Ok(both)
}

/// The new-side lines that `git diff <range>` marks as added, per file. A rename reads as a delete and an add, so the
/// renamed file keeps all its lines, and `--text` reads a file that git marks binary as text.
async fn added_lines(worktree: &Path, range: &str) -> anyhow::Result<AddedLines> {
  let (ok, out, err) = git::git_stdout(
    worktree,
    &[
      "-c",
      "core.quotePath=false",
      "diff",
      "--text",
      "--no-renames",
      "--no-color",
      "--no-ext-diff",
      "--no-textconv",
      "--src-prefix=a/",
      "--dst-prefix=b/",
      "-U0",
      range,
    ],
    GIT_READ_TIMEOUT,
    "git diff",
  )
  .await;
  if !ok {
    anyhow::bail!("git diff {range} failed in {}: {err}", worktree.display());
  }
  parse_added_lines(&out).with_context(|| format!("reading git diff {range} in {}", worktree.display()))
}

/// Reads the added lines of a `-U0` diff text. A `diff --git` line opens a file's header, and the header's `+++` line
/// names the file; a `@@` line opens a hunk, whose new-side range lists the added lines. A hunk's body lines begin with
/// `+`, `-` or a space, so a body line never reads as a header.
fn parse_added_lines(diff: &str) -> anyhow::Result<AddedLines> {
  let mut added = AddedLines::new();
  let mut file: Option<PathBuf> = None;
  let mut in_header = false;
  for line in diff.lines() {
    if line.starts_with("diff --git ") {
      file = None;
      in_header = true;
    } else if let Some(path) = line.strip_prefix("+++ ").filter(|_| in_header) {
      file = header_path(path)?;
    } else if let Some(hunk) = line.strip_prefix("@@ ") {
      in_header = false;
      let new_side = hunk
        .split_whitespace()
        .find_map(|token| token.strip_prefix('+'))
        .ok_or_else(|| anyhow::anyhow!("a hunk header without a new-side range: {line}"))?;
      let (start, count) = new_side.split_once(',').unwrap_or((new_side, "1"));
      let start: usize = start.parse().with_context(|| format!("the hunk header {line}"))?;
      let count: usize = count.parse().with_context(|| format!("the hunk header {line}"))?;
      if count == 0 {
        continue;
      }
      let path = file.as_ref().ok_or_else(|| anyhow::anyhow!("the hunk {line} has no file header"))?;
      added.entry(path.clone()).or_default().extend(start..start + count);
    }
  }
  Ok(added)
}

/// The path that a `+++` header names: `b/<path>`, or none for `/dev/null`, a deleted file. git quotes a path with
/// unusual characters, and the check reads only plain paths, so a quoted one is an error.
fn header_path(path: &str) -> anyhow::Result<Option<PathBuf>> {
  if path == "/dev/null" {
    return Ok(None);
  }
  // git ends a name that holds a space with a tab, so the tab is not part of the path.
  let plain = path.trim_end_matches('\t');
  anyhow::ensure!(
    !plain.starts_with('"'),
    "git quoted the diff path {plain}; the backend-literal check reads plain paths"
  );
  let relative = plain.strip_prefix("b/").ok_or_else(|| anyhow::anyhow!("the file header {path} has no b/ path"))?;
  Ok(Some(PathBuf::from(relative)))
}

/// Run one landing call. A gated head that passes its checks, and every ungated head, goes to the landing queue: the
/// command `charliebot land queue run --sha` takes the turn and writes `main`, and this call records `waiting` with the
/// queue's wake registered. A repeat call for a head that already landed returns its record. A repeat call for a head
/// whose queued command is still pending returns the waiting record unchanged: that command owns the rest of the
/// landing, and its entry keeps its key while it waits.
pub async fn land(
  registry: &Registry,
  home: &Path,
  task_session_id: &str,
  options: &LandOptions,
) -> anyhow::Result<LandingRecord> {
  options.validate()?;
  let ctx = TaskLanding::load(home, task_session_id).await?;
  land_in(&ctx, registry, home, options).await
}

/// The body of `land` on a task venue that the caller has loaded. `land` reads the venue from `home` and calls this.
async fn land_in(
  ctx: &TaskLanding,
  registry: &Registry,
  home: &Path,
  options: &LandOptions,
) -> anyhow::Result<LandingRecord> {
  // The session's own call works in the session's worktree; a queued command never reads it.
  if !ctx.worktree.is_dir() {
    anyhow::bail!("worktree {} of task {} does not exist", ctx.worktree.display(), ctx.session_id);
  }
  let published = review_text::review_published_branch(&ctx.base_branch);
  let base_ref = format!("origin/{published}");
  fetch_base(&ctx.worktree, published).await?;
  let landed_head = head_sha(&ctx.worktree).await?;
  let prev = read_landing_record(&ctx.session_dir)?;
  // A queued landing rebases its head onto a moved main, so a landed head can hold the landed patch under its old sha:
  // the head's commits that main lacks decide whether anything is left to land.
  if let Some(record) = prev.as_ref().filter(|record| is_landed(&record.state))
    && (record.sha == landed_head || !has_unlanded_commits(&ctx.worktree, &base_ref).await?)
  {
    return Ok(record.clone());
  }
  if let Some(record) = prev.as_ref().filter(|record| record.sha == landed_head && record.state == STATE_WAITING) {
    let wakes = ctx.queue_owner().pending().await?;
    if land_queue::queued_command_pending(&wakes, &record.sha) {
      return Ok(record.clone());
    }
  }
  let changed = changed_paths(&ctx.worktree, &base_ref).await?;
  // The release switch captures each View line that a pushed commit carries, whatever the diff changes, so a session
  // view the live server cannot resolve is refused here.
  let commits = commits_ahead(&ctx.worktree, published).await?;
  let commit_views: Vec<String> =
    commits.iter().flat_map(|commit| web_screenshots::view_lines_of_message(&commit.body)).collect();
  web_screenshots::check_landing_views(&commit_views)?;
  let mut views: Vec<String> = Vec::new();
  if web_screenshots::changes_web_page(&changed) {
    let body = report_body(&ctx.worktree, published, options).await?;
    views = web_screenshots::check_pr_body(body.as_deref(), options.evidence_dir.as_deref())?.views;
  }

  let gated = matches!(gate_applicability(&ctx.repo_path, published).await?, GateApplicability::Applies);
  let prev = prev.filter(|record| !is_landed(&record.state));
  let mut retries = prev.as_ref().map_or(0, |record| record.rebase_retries);
  match rebase_onto_base(&ctx.worktree, &base_ref).await? {
    RebaseStep::Conflict => {
      abort_rebase(&ctx.worktree).await;
      let record = LandingRecord {
        state: STATE_REBASE_CONFLICT.to_string(),
        repo: ctx.repo.clone(),
        base: published.to_string(),
        sha: head_sha(&ctx.worktree).await?,
        pr_url: None,
        checks: Vec::new(),
        head_pushed_at: None,
        ci_passed_at: None,
        pushed_at: None,
        rebase_retries: retries,
        queue_ref: None,
      };
      write_landing_record(&ctx.session_dir, &record)?;
      return Ok(record);
    }
    RebaseStep::UpToDate => {}
    RebaseStep::Rewrote => {
      if rewrite_counts_as_retry(prev.as_ref(), true) {
        retries += 1;
      }
    }
  }

  carry_view_trailers(&ctx.worktree, &views).await?;
  backend_literal_check(registry, home, &ctx.worktree, &base_ref).await?;
  let sha = head_sha(&ctx.worktree).await?;
  let head_pushed_at = carried_pushed_at(prev.as_ref(), &sha, &now_rfc3339());
  let queue_ref = prev.as_ref().filter(|record| record.sha == sha).and_then(|record| record.queue_ref.clone());
  let (pr_url, pr_number, checks, ci_passed_at) = if gated {
    let pr_text = resolve_draft_pr_text(&ctx.worktree, published, options).await?;
    outbound_checks(home, &ctx.worktree, &ctx.repo_path, &ctx.branch_name, &base_ref, Some(&pr_text)).await?;
    push_task_branch(ctx, &ctx.branch_name).await?;
    let (pr_url, pr_number) = ensure_draft_pr(ctx, &ctx.repo, published, &pr_text).await?;
    let expected = expected_workflow_paths(&workflow_files_at(&ctx.worktree, &sha).await?)?;
    let (evaluation, verdict) = match read_ci_state(&ctx.worktree, &ctx.repo, &sha).await? {
      CiRead::Evaluated(evaluation) => {
        let elapsed = match &head_pushed_at {
          Some(pushed_at) => elapsed_secs(pushed_at)?,
          None => 0,
        };
        let verdict = judge_ci(&expected, &evaluation.runs, &evaluation.check_runs, &evaluation.statuses, elapsed);
        (*evaluation, verdict)
      }
      // The queued command reads the head sha again once the quota resets.
      CiRead::RateLimited { reset } => {
        tracing::info!(target: "charliebot_tasks", branch = %ctx.branch_name, reset, "land_ci_read_rate_limited");
        (CiEvaluation::unread(), CiVerdict::Waiting)
      }
    };
    let checks = evaluation.check_rows();
    let ci_passed_at = evaluation.ci_passed_at();
    let stop = match verdict {
      CiVerdict::Pass | CiVerdict::Waiting => None,
      CiVerdict::Failed { .. } => Some(STATE_CI_FAILED),
      CiVerdict::NotStarted { .. } => Some(STATE_CI_NOT_STARTED),
      CiVerdict::Timeout => Some(STATE_CI_TIMEOUT),
    };
    if let Some(state) = stop {
      let record = LandingRecord {
        state: state.to_string(),
        repo: ctx.repo.clone(),
        base: published.to_string(),
        sha: sha.clone(),
        pr_url: Some(pr_url),
        checks,
        head_pushed_at: head_pushed_at.clone(),
        ci_passed_at,
        pushed_at: None,
        rebase_retries: retries,
        queue_ref: None,
      };
      write_landing_record(&ctx.session_dir, &record)?;
      return Ok(record);
    }
    (Some(pr_url), pr_number, checks, ci_passed_at)
  } else {
    outbound_checks(home, &ctx.worktree, &ctx.repo_path, &ctx.branch_name, &base_ref, None).await?;
    (None, 0, Vec::new(), None)
  };
  let record = LandingRecord {
    state: STATE_WAITING.to_string(),
    repo: ctx.repo.clone(),
    base: published.to_string(),
    sha,
    pr_url,
    checks,
    head_pushed_at,
    ci_passed_at,
    pushed_at: None,
    rebase_retries: retries,
    queue_ref,
  };
  // The record comes first: the queued command reads it before it waits for CI, and a repeat call re-arms from it when
  // the registration failed.
  write_landing_record(&ctx.session_dir, &record)?;
  land_queue::arm_landing(&ctx.queue_site(), &ctx.queue_owner(), u64::try_from(pr_number)?, &record.sha).await?;
  Ok(record)
}

/// Whether a landing state means the head is on the base branch.
fn is_landed(state: &str) -> bool {
  state == STATE_LANDED || state == STATE_LANDED_NO_CI
}

/// Whether `HEAD` holds a commit whose patch `base_ref` lacks. `git cherry` marks a commit whose patch the base already
/// holds with `-`, so a commit that landed under another sha reads as landed.
async fn has_unlanded_commits(worktree: &Path, base_ref: &str) -> anyhow::Result<bool> {
  let (ok, out, err) = git::git_stdout(worktree, &["cherry", base_ref, "HEAD"], GIT_READ_TIMEOUT, "git cherry").await;
  if !ok {
    anyhow::bail!("git cherry {base_ref} HEAD failed in {}: {err}", worktree.display());
  }
  Ok(out.lines().any(|line| line.starts_with('+')))
}

/// One CI read of a head sha: the verdict, the check rows and the latest completed check time.
#[derive(Clone, Debug)]
pub(crate) struct CiSnapshot {
  pub verdict: CiVerdict,
  pub checks: Vec<CheckRow>,
  pub ci_passed_at: Option<String>,
}

impl CiSnapshot {
  /// The read of a landing that has no CI read, such as an ungated head or a conflicted rebase.
  fn unread() -> Self {
    Self { verdict: CiVerdict::Waiting, checks: Vec::new(), ci_passed_at: None }
  }
}

/// The GitHub reads of a queued landing: the CI verdict of a head sha, and the close of a landed branch's PRs. The
/// queued command takes its GitHub through this seam, so its queue protocol runs against a bare repository in tests.
pub(crate) trait LandRemote {
  fn ci_snapshot(
    &self,
    ctx: &TaskLanding,
    sha: &str,
    head_pushed_at: Option<&str>,
  ) -> impl Future<Output = anyhow::Result<CiSnapshot>>;
  fn close_task_prs(&self, ctx: &TaskLanding, published: &str, sha: &str) -> impl Future<Output = anyhow::Result<()>>;
}

/// The GitHub remote of production.
pub(crate) struct GitHub;

impl LandRemote for GitHub {
  async fn ci_snapshot(
    &self,
    ctx: &TaskLanding,
    sha: &str,
    head_pushed_at: Option<&str>,
  ) -> anyhow::Result<CiSnapshot> {
    let expected = expected_workflow_paths(&workflow_files_at(&ctx.worktree, sha).await?)?;
    match read_ci_state(&ctx.worktree, &ctx.repo, sha).await? {
      CiRead::Evaluated(evaluation) => {
        let elapsed = match head_pushed_at {
          Some(pushed_at) => elapsed_secs(pushed_at)?,
          None => 0,
        };
        let verdict = judge_ci(&expected, &evaluation.runs, &evaluation.check_runs, &evaluation.statuses, elapsed);
        Ok(CiSnapshot { verdict, checks: evaluation.check_rows(), ci_passed_at: evaluation.ci_passed_at() })
      }
      CiRead::RateLimited { reset } => {
        tracing::info!(target: "charliebot_tasks", branch = %ctx.branch_name, reset, "land_ci_read_rate_limited");
        Ok(CiSnapshot { verdict: CiVerdict::Waiting, checks: Vec::new(), ci_passed_at: None })
      }
    }
  }

  async fn close_task_prs(&self, ctx: &TaskLanding, published: &str, sha: &str) -> anyhow::Result<()> {
    close_task_prs(ctx, published, sha).await
  }
}

/// How a queued landing ended: its record, or no record because a newer landing call of the task owns the landing.
pub enum QueueEnd {
  Record(Box<LandingRecord>),
  Superseded,
}

/// The command `charliebot land queue run --sha` runs: waits for CI on the head, enters the queue, takes the turn when
/// it is the earliest entry, rebases, pushes the task branch, waits for CI on the rebased head, and writes `main` in
/// one atomic push that also releases the turn and the entry.
pub async fn run_queued(
  registry: &Registry,
  home: &Path,
  task_session_id: &str,
  sha: &str,
  cadence: Cadence,
  ci_wait: Duration,
) -> anyhow::Result<QueueEnd> {
  let ctx = TaskLanding::load(home, task_session_id).await?;
  queued_landing(&ctx, registry, home, sha, &GitHub, cadence, ci_wait).await
}

/// The context one queued landing shares across its phases.
struct Lane<'a> {
  ctx: &'a TaskLanding,
  registry: &'a Registry,
  home: &'a Path,
  /// The landing record `land` wrote for the head: the repository, base and PR of the task.
  prev: &'a LandingRecord,
  gated: bool,
  cadence: Cadence,
  /// The head sha the command was queued for, which the session's landing record must keep naming.
  queued: &'a str,
}

/// One head sha the turn holder works on: the sha, when its branch was pushed, its rebase retries and its CI read.
struct Round {
  sha: String,
  head_pushed_at: Option<String>,
  retries: i64,
  snapshot: Option<CiSnapshot>,
}

/// The queued landing of one task, over the remote `R`, in a scratch worktree of its own at `start_sha`. A SIGTERM or
/// SIGINT ends it with an error. The scratch worktree is removed when this returns, on every exit path.
async fn queued_landing<R: LandRemote>(
  ctx: &TaskLanding,
  registry: &Registry,
  home: &Path,
  start_sha: &str,
  remote: &R,
  cadence: Cadence,
  ci_wait: Duration,
) -> anyhow::Result<QueueEnd> {
  // The listeners exist before the worktree does, so a stop that arrives while it is made is still seen.
  let mut terminate = signal(SignalKind::terminate()).context("could not listen for SIGTERM")?;
  let mut interrupt = signal(SignalKind::interrupt()).context("could not listen for SIGINT")?;
  let scratch = LandingWorktree::add(&ctx.repo_path, &landing_worktree::root_in(home), start_sha).await?;
  let scratch_ctx = TaskLanding { worktree: scratch.path().to_path_buf(), ..ctx.clone() };
  let end = tokio::select! {
    end = queued_landing_in(&scratch_ctx, registry, home, start_sha, remote, cadence, ci_wait) => end,
    _ = terminate.recv() => Err(anyhow::anyhow!("the queued landing of {start_sha} was stopped by SIGTERM")),
    _ = interrupt.recv() => Err(anyhow::anyhow!("the queued landing of {start_sha} was stopped by SIGINT")),
  };
  drop(scratch);
  end
}

/// The body of a queued landing, which runs in `ctx.worktree`: the scratch worktree that `queued_landing` made.
async fn queued_landing_in<R: LandRemote>(
  ctx: &TaskLanding,
  registry: &Registry,
  home: &Path,
  start_sha: &str,
  remote: &R,
  cadence: Cadence,
  ci_wait: Duration,
) -> anyhow::Result<QueueEnd> {
  let published = review_text::review_published_branch(&ctx.base_branch).to_string();
  let prev = read_landing_record(&ctx.session_dir)?.ok_or_else(|| {
    anyhow::anyhow!(
      "{} holds no landing record; `charliebot land` writes it before the queue runs",
      ctx.session_dir.display()
    )
  })?;
  let pr = match prev.pr_url.as_deref() {
    Some(url) => pr_number_of(url)?,
    None => 0,
  };
  let gated = matches!(gate_applicability(&ctx.repo_path, &published).await?, GateApplicability::Applies);
  let lane = Lane { ctx, registry, home, prev: &prev, gated, cadence, queued: start_sha };
  let site = ctx.queue_site();
  let owner = ctx.queue_owner();
  let mut round = Round {
    sha: start_sha.to_string(),
    head_pushed_at: carried_pushed_at(Some(&prev), start_sha, &now_rfc3339()),
    retries: prev.rebase_retries,
    snapshot: None,
  };

  if gated {
    loop {
      if superseded(ctx, start_sha)? {
        return Ok(QueueEnd::Superseded);
      }
      let read = remote.ci_snapshot(ctx, &round.sha, round.head_pushed_at.as_deref()).await?;
      let state = match read.verdict {
        CiVerdict::Pass => {
          round.snapshot = Some(read);
          break;
        }
        CiVerdict::Waiting => {
          tokio::time::sleep(cadence.poll).await;
          continue;
        }
        CiVerdict::Failed { .. } => STATE_CI_FAILED,
        CiVerdict::NotStarted { .. } => STATE_CI_NOT_STARTED,
        CiVerdict::Timeout => STATE_CI_TIMEOUT,
      };
      let record = stopped_record(&lane, &round, state, &read);
      if !write_queued_record(ctx, start_sha, &record)? {
        return Ok(QueueEnd::Superseded);
      }
      return Ok(QueueEnd::Record(Box::new(record)));
    }
  }

  let ci_passed_ms = match round.snapshot.as_ref().and_then(|read| read.ci_passed_at.as_deref()) {
    Some(at) => rfc3339_millis(at)?,
    None => u64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000)?,
  };
  let name = land_queue::EntryName::new(ci_passed_ms, pr, &owner.token()?);
  let mut claim = land_queue::Claim::enter_or_resume(&site, &name, &round.sha).await?;
  let read = round.snapshot.take().unwrap_or_else(CiSnapshot::unread);
  let mut waiting = stopped_record(&lane, &round, STATE_WAITING, &read);
  waiting.queue_ref = Some(claim.holder.clone());
  match write_queued_record(ctx, start_sha, &waiting) {
    Ok(true) => {}
    Ok(false) => {
      claim.release().await?;
      return Ok(QueueEnd::Superseded);
    }
    Err(error) => {
      claim.release().await?;
      return Err(error);
    }
  }
  round.snapshot = Some(read);

  loop {
    match superseded(ctx, start_sha) {
      Ok(false) => {}
      Ok(true) => {
        claim.release().await?;
        return Ok(QueueEnd::Superseded);
      }
      Err(error) => {
        claim.release().await?;
        return Err(error);
      }
    }
    match land_queue::turn_step(&mut claim, cadence).await {
      Ok(land_queue::Step::Held) => break,
      Ok(land_queue::Step::Waiting) => tokio::time::sleep(cadence.poll).await,
      Err(error) => {
        // A newer landing call may have replaced the entry between the record check and the heartbeat.
        let released = claim.release().await;
        if superseded(ctx, start_sha)? {
          return Ok(QueueEnd::Superseded);
        }
        released?;
        return Err(error);
      }
    }
  }

  match hold_and_publish(&lane, remote, &mut claim, &mut round, ci_wait).await {
    Ok(end) => Ok(end),
    Err(error) => {
      if let Err(release) = claim.release().await {
        tracing::error!(target: "charliebot_tasks", holder = %claim.holder, error = %release, "land_queue_release_failed");
      }
      Err(error)
    }
  }
}

/// The record of a landing that stops at `state` on the round's head, carrying the CI read that stopped it.
fn stopped_record(lane: &Lane<'_>, round: &Round, state: &str, read: &CiSnapshot) -> LandingRecord {
  LandingRecord {
    state: state.to_string(),
    repo: lane.ctx.repo.clone(),
    base: lane.prev.base.clone(),
    sha: round.sha.clone(),
    pr_url: lane.prev.pr_url.clone(),
    checks: read.checks.clone(),
    head_pushed_at: round.head_pushed_at.clone(),
    ci_passed_at: read.ci_passed_at.clone(),
    pushed_at: None,
    rebase_retries: round.retries,
    queue_ref: None,
  }
}

/// The rounds of a turn holder: rebase onto main, push the task branch, wait for CI on the rebased head, and write
/// `main` in one push that also releases the turn and the entry. A refused write that moved main starts another round.
async fn hold_and_publish<R: LandRemote>(
  lane: &Lane<'_>,
  remote: &R,
  claim: &mut land_queue::Claim,
  round: &mut Round,
  ci_wait: Duration,
) -> anyhow::Result<QueueEnd> {
  let ctx = lane.ctx;
  let published = review_text::review_published_branch(&ctx.base_branch).to_string();
  let base_ref = format!("origin/{published}");
  let site = ctx.queue_site();
  for _ in 0..MAX_LANDING_ROUNDS {
    if superseded(ctx, lane.queued)? {
      claim.release().await?;
      return Ok(QueueEnd::Superseded);
    }
    fetch_base(&ctx.worktree, &published).await?;
    match rebase_onto_base(&ctx.worktree, &base_ref).await? {
      RebaseStep::Conflict => {
        abort_rebase(&ctx.worktree).await;
        claim.release().await?;
        let record = stopped_record(lane, round, STATE_REBASE_CONFLICT, &CiSnapshot::unread());
        if !write_queued_record(ctx, lane.queued, &record)? {
          return Ok(QueueEnd::Superseded);
        }
        return Ok(QueueEnd::Record(Box::new(record)));
      }
      RebaseStep::UpToDate => {}
      RebaseStep::Rewrote => round.retries += 1,
    }
    let head = head_sha(&ctx.worktree).await?;
    if head != round.sha {
      push_task_branch(ctx, &ctx.branch_name).await?;
      round.sha = head;
      round.head_pushed_at = Some(now_rfc3339());
      round.snapshot = None;
      if claim.beat(&round.sha).await? != land_queue::Pushed::Done {
        // A newer landing call replaced this head's entry; the worktree's branch holds the head it serves.
        claim.release().await?;
        return Ok(QueueEnd::Superseded);
      }
    }
    backend_literal_check(lane.registry, lane.home, &ctx.worktree, &base_ref).await?;
    outbound_checks(lane.home, &ctx.worktree, &ctx.repo_path, &ctx.branch_name, &base_ref, None).await?;
    let tested_base = git::git_rev_parse(&ctx.worktree, &base_ref)
      .await
      .ok_or_else(|| anyhow::anyhow!("{base_ref} does not resolve in {}", ctx.worktree.display()))?;

    if lane.gated {
      let started = Instant::now();
      let read = loop {
        if superseded(ctx, lane.queued)? {
          claim.release().await?;
          return Ok(QueueEnd::Superseded);
        }
        if let Some(end) = keep_turn_alive(lane, claim, &round.sha).await? {
          return Ok(end);
        }
        if started.elapsed() >= ci_wait {
          claim.release().await?;
          anyhow::bail!(
            "CI on {} did not pass within {}s of the turn; the turn and its entry were released",
            round.sha,
            ci_wait.as_secs()
          );
        }
        let read = remote.ci_snapshot(ctx, &round.sha, round.head_pushed_at.as_deref()).await?;
        let state = match read.verdict {
          CiVerdict::Pass => break read,
          CiVerdict::Waiting => {
            tokio::time::sleep(lane.cadence.poll).await;
            continue;
          }
          CiVerdict::Failed { .. } => STATE_CI_FAILED,
          CiVerdict::NotStarted { .. } => STATE_CI_NOT_STARTED,
          CiVerdict::Timeout => STATE_CI_TIMEOUT,
        };
        claim.release().await?;
        let record = stopped_record(lane, round, state, &read);
        if !write_queued_record(ctx, lane.queued, &record)? {
          return Ok(QueueEnd::Superseded);
        }
        return Ok(QueueEnd::Record(Box::new(record)));
      };
      round.snapshot = Some(read);
    }

    let read = round.snapshot.take().unwrap_or_else(CiSnapshot::unread);
    let publish = claim.publish_changes(&format!("refs/heads/{published}"), &tested_base, &round.sha);
    match site.push(&publish).await? {
      land_queue::Pushed::Done => {
        claim.landed();
        let state = if lane.gated { STATE_LANDED } else { STATE_LANDED_NO_CI };
        let record = LandingRecord { pushed_at: Some(now_rfc3339()), ..stopped_record(lane, round, state, &read) };
        // main holds the commit whether or not a newer call replaced the record, which that call keeps.
        write_queued_record(ctx, lane.queued, &record)?;
        remote.close_task_prs(ctx, &published, &record.sha).await.map_err(|error| {
          anyhow::anyhow!("{published} holds {} and landing.json reads {}, but {error:#}", record.sha, record.state)
        })?;
        return Ok(QueueEnd::Record(Box::new(record)));
      }
      land_queue::Pushed::Refused(err) => {
        if push_rejection_kind(&err) == PushRejection::RepositoryRules {
          claim.release().await?;
          let record = stopped_record(lane, round, STATE_PR_MERGE_REQUIRED, &read);
          if !write_queued_record(ctx, lane.queued, &record)? {
            return Ok(QueueEnd::Superseded);
          }
          return Ok(QueueEnd::Record(Box::new(record)));
        }
        let tip = remote_main_tip(&ctx.worktree, &published).await?;
        if tip == tested_base {
          claim.release().await?;
          anyhow::bail!("the landing queue refused the write of {} to {published}: {err}", round.sha);
        }
        tracing::info!(target: "charliebot_tasks", base_tip = %tip, "land_queue_main_moved_rebase");
      }
    }
  }
  claim.release().await?;
  anyhow::bail!(
    "main kept moving past {MAX_LANDING_ROUNDS} landing rounds; run `charliebot land` again when main settles"
  )
}

/// Renews the turn's heartbeat when it is due. A refused renewal ends the holder: when a newer landing call replaced
/// the queued landing, the landing is superseded and the turn is released; otherwise the refs were removed under the
/// holder and that is an error.
async fn keep_turn_alive(
  lane: &Lane<'_>,
  claim: &mut land_queue::Claim,
  sha: &str,
) -> anyhow::Result<Option<QueueEnd>> {
  if !claim.beat_due(lane.cadence) {
    return Ok(None);
  }
  let parent = claim.parent.clone();
  let land_queue::Pushed::Refused(err) = claim.beat(&parent).await? else {
    return Ok(None);
  };
  claim.release().await?;
  if superseded(lane.ctx, lane.queued)? {
    return Ok(Some(QueueEnd::Superseded));
  }
  anyhow::bail!("the landing queue refs of {} were removed while {sha} held the turn: {err}", claim.holder)
}

/// Whether a newer `land` call of the task replaced the landing that `queued` serves. `land` writes the session's
/// landing record for each head before it arms that head's command, so the record names the current head. The check
/// reads the record and never the session's worktree, which may hold any checkout or commit.
fn superseded(ctx: &TaskLanding, queued: &str) -> anyhow::Result<bool> {
  let record = read_landing_record(&ctx.session_dir)?
    .ok_or_else(|| anyhow::anyhow!("{} holds no landing record", ctx.session_dir.display()))?;
  Ok(record.sha != queued)
}

/// Writes `record`, a state of the queued landing of `queued`, only while the session's record still names `queued`, so
/// a command that a newer call superseded never overwrites that call's record. Returns whether the record was written.
fn write_queued_record(ctx: &TaskLanding, queued: &str, record: &LandingRecord) -> anyhow::Result<bool> {
  if superseded(ctx, queued)? {
    return Ok(false);
  }
  write_landing_record(&ctx.session_dir, record)?;
  Ok(true)
}

/// The sha that `refs/heads/<published>` holds on origin.
async fn remote_main_tip(worktree: &Path, published: &str) -> anyhow::Result<String> {
  let refname = format!("refs/heads/{published}");
  let (ok, out, err) =
    git::git_stdout(worktree, &["ls-remote", "origin", &refname], GIT_READ_TIMEOUT, "git ls-remote").await;
  if !ok {
    anyhow::bail!("git ls-remote origin {refname} failed: {err}");
  }
  out.split_whitespace().next().map(str::to_string).ok_or_else(|| anyhow::anyhow!("origin has no {refname}"))
}

/// The pull request number a PR URL ends with.
fn pr_number_of(url: &str) -> anyhow::Result<u64> {
  url
    .rsplit('/')
    .next()
    .and_then(|number| number.parse().ok())
    .ok_or_else(|| anyhow::anyhow!("PR URL {url} does not end with a number"))
}

/// The millisecond time of an RFC 3339 instant.
fn rfc3339_millis(at: &str) -> anyhow::Result<u64> {
  let parsed = OffsetDateTime::parse(at, &Rfc3339).with_context(|| format!("{at} is not RFC 3339"))?;
  u64::try_from(parsed.unix_timestamp_nanos() / 1_000_000).with_context(|| format!("{at} is before 1970"))
}

const GIT_READ_TIMEOUT: Duration = Duration::from_secs(30);
const GIT_WRITE_TIMEOUT: Duration = Duration::from_secs(120);
const GIT_PUSH_TIMEOUT: Duration = Duration::from_secs(120);

/// The paths the task branch changes against the base, from the three-dot range that a review reads.
async fn changed_paths(worktree: &Path, base_ref: &str) -> anyhow::Result<Vec<String>> {
  let range = format!("{base_ref}...HEAD");
  let (ok, out, err) = git::git_stdout(worktree, &["diff", "--name-only", &range], GIT_READ_TIMEOUT, "git diff").await;
  if !ok {
    anyhow::bail!("git diff --name-only {range} failed in {}: {err}", worktree.display());
  }
  Ok(out.lines().map(str::to_string).collect())
}

/// The PR body a landing is judged on: the explicit body, else the body of the branch's single commit. A branch of
/// several commits without an explicit body has none (`report_body`).
async fn report_body(worktree: &Path, published_base: &str, options: &LandOptions) -> anyhow::Result<Option<String>> {
  if let Some(body) = &options.body {
    return Ok(Some(body.clone()));
  }
  let commits = commits_ahead(worktree, published_base).await?;
  Ok(match commits.as_slice() {
    [single] => Some(single.body.clone()),
    _ => None,
  })
}

/// Appends each View line to the head commit's trailer block. A landing pushes the task branch's commits rather
/// than squashing them, so the trailer must be in the commit before the push, and a later release switch reads the
/// `View:` trailers back from the commits it moved across.
async fn carry_view_trailers(worktree: &Path, views: &[String]) -> anyhow::Result<()> {
  if views.is_empty() {
    return Ok(());
  }
  let (ok, message, err) = git::git_stdout(worktree, &["log", "-1", "--format=%B"], GIT_READ_TIMEOUT, "git log").await;
  if !ok {
    anyhow::bail!("reading the head commit message failed in {}: {err}", worktree.display());
  }
  let missing: Vec<&String> =
    views.iter().filter(|view| !message.lines().any(|line| line.trim() == format!("View: {view}"))).collect();
  if missing.is_empty() {
    return Ok(());
  }
  let mut amended = message.trim_end().to_string();
  amended.push_str("\n\n");
  for view in missing {
    amended.push_str(&format!("View: {view}\n"));
  }
  let args = ["commit", "--amend", "-q", "-m", amended.as_str()];
  let (ok, _, err) = git::git_stdout(worktree, &args, GIT_WRITE_TIMEOUT, "git commit --amend").await;
  if !ok {
    anyhow::bail!("writing the View trailers into the head commit failed in {}: {err}", worktree.display());
  }
  Ok(())
}

async fn fetch_base(worktree: &Path, published: &str) -> anyhow::Result<()> {
  let (ok, _, err) = git::git_stdout(worktree, &["fetch", "origin", published], GIT_WRITE_TIMEOUT, "git fetch").await;
  if !ok {
    anyhow::bail!("git fetch origin {published} failed in {}: {err}", worktree.display());
  }
  Ok(())
}

async fn head_sha(worktree: &Path) -> anyhow::Result<String> {
  git::git_rev_parse(worktree, "HEAD")
    .await
    .ok_or_else(|| anyhow::anyhow!("HEAD does not resolve in {}", worktree.display()))
}

async fn abort_rebase(worktree: &Path) {
  let (ok, _, err) = git::git_stdout(worktree, &["rebase", "--abort"], GIT_WRITE_TIMEOUT, "git rebase --abort").await;
  if !ok {
    tracing::warn!(target: "charliebot_tasks", worktree = %worktree.display(), error = %err, "land_rebase_abort_failed");
  }
}

/// The one rebase step: a rebase onto the base tip only when the tip is not already an ancestor,
/// so an unchanged head keeps its sha and its clocks.
async fn rebase_onto_base(worktree: &Path, base_ref: &str) -> anyhow::Result<RebaseStep> {
  let ancestor = git::git_stdout(worktree, &ancestry_args(base_ref, "HEAD"), GIT_READ_TIMEOUT, "git merge-base").await;
  match ancestor {
    (true, _, _) => return Ok(RebaseStep::UpToDate),
    (false, _, err) if merge_base_unknown(&err) => {}
    (false, _, err) => anyhow::bail!("git merge-base --is-ancestor {base_ref} HEAD failed: {err}"),
  }
  let (ok, out, err) = git::git_stdout(worktree, &["rebase", base_ref], GIT_WRITE_TIMEOUT, "git rebase").await;
  if ok {
    return Ok(RebaseStep::Rewrote);
  }
  let detail = format!("{out}\n{err}");
  if detail.contains("CONFLICT") || detail.contains("could not apply") {
    return Ok(RebaseStep::Conflict);
  }
  anyhow::bail!("git rebase {base_ref} failed in {}: {err}", worktree.display());
}

/// `git merge-base --is-ancestor` reports "not an ancestor" as exit 1 without stderr; any other
/// output on a non-zero exit is a failure of the check itself.
fn merge_base_unknown(err: &str) -> bool {
  err.trim().is_empty()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RebaseStep {
  Rewrote,
  UpToDate,
  Conflict,
}

/// The task branch push under `--force-with-lease`, so a rebase never clobbers another push. It pushes the worktree's
/// HEAD, not the local branch: a queued landing rebases in its scratch worktree, which leaves the session's local
/// branch where it was.
///
/// With the branch on origin, the push fetches it first and leases against the fetched tip. Without it
/// — a merged branch GitHub deleted — the push leases against an explicitly absent ref,
/// `--force-with-lease=refs/heads/<branch>:`. The empty expected value means the ref must not exist on
/// origin; without it, git falls back to the local `refs/remotes/origin/<branch>`, which still holds the
/// merged tip and makes the push fail as `stale info`.
async fn push_task_branch(ctx: &TaskLanding, branch: &str) -> anyhow::Result<()> {
  let lease = if origin_has_branch(ctx, branch).await? {
    let (fetched, _, fetch_err) =
      git::git_stdout(&ctx.worktree, &["fetch", "origin", branch], GIT_WRITE_TIMEOUT, "git fetch task branch").await;
    if !fetched {
      anyhow::bail!("git fetch origin {branch} failed: {fetch_err}");
    }
    "--force-with-lease".to_string()
  } else {
    format!("--force-with-lease=refs/heads/{branch}:")
  };
  let refspec = format!("HEAD:refs/heads/{branch}");
  let (ok, _, err) =
    git::git_stdout(&ctx.worktree, &["push", &lease, "origin", &refspec], GIT_PUSH_TIMEOUT, "git push task branch")
      .await;
  if !ok {
    anyhow::bail!("pushing task branch {branch} failed: {err}");
  }
  Ok(())
}

async fn origin_has_branch(ctx: &TaskLanding, branch: &str) -> anyhow::Result<bool> {
  let (ok, out, err) = git::git_stdout(
    &ctx.worktree,
    &["ls-remote", "origin", &format!("refs/heads/{branch}")],
    GIT_READ_TIMEOUT,
    "git ls-remote",
  )
  .await;
  if !ok {
    anyhow::bail!("git ls-remote origin {branch} failed: {err}");
  }
  Ok(!out.trim().is_empty())
}

/// The push-to-base rejection class: repository rules (GH013) answer `pr-merge-required` with the
/// green PR kept open. Only git's own non-fast-forward report can start a verification; every other
/// refusal — a local or remote hook included — ends the landing at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PushRejection {
  NonFastForward,
  RepositoryRules,
  Timeout,
  Transport,
  Auth,
  Refused,
}

/// stderr fragments git prints when the remote's own check rejects the pushed sha as not a
/// descendant of its tip.
const NON_FAST_FORWARD_MARKERS: [&str; 3] = ["non-fast-forward", "fetch first", "stale info"];

/// The two fragments receive-pack prints when the ref moved between the push's advertisement and
/// its update transaction — the lease check of a lost push race. Both must appear, so a hook or a
/// local lock failure that echoes only `cannot lock ref` stays a plain refusal.
const LEASE_CHECK_MARKERS: [&str; 2] = ["cannot lock ref", "but expected"];

/// stderr fragments of timed-out remote operations, including an SSH connection timeout.
const TIMEOUT_MARKERS: [&str; 2] = ["timed out", "timeout"];

/// stderr fragments of a dead or unreachable connection: handshake resets, DNS and TCP failures,
/// mid-transfer failures, and git's "could not read from remote repository" closer.
const TRANSPORT_MARKERS: [&str; 12] = [
  "connection reset",
  "connection refused",
  "could not resolve hostname",
  "temporary failure in name resolution",
  "no route to host",
  "network is unreachable",
  "kex_exchange_identification",
  "could not read from remote repository",
  "the remote end hung up unexpectedly",
  "early eof",
  "rpc failed",
  "broken pipe",
];

/// stderr fragments of a refused login: ssh publickey and https credential rejections.
const AUTH_MARKERS: [&str; 2] = ["permission denied", "authentication failed"];

fn push_rejection_kind(stderr: &str) -> PushRejection {
  let lowered = stderr.to_lowercase();
  if lowered.contains("gh013") || lowered.contains("repository rule") || lowered.contains("rule violations") {
    PushRejection::RepositoryRules
  } else if NON_FAST_FORWARD_MARKERS.iter().any(|marker| lowered.contains(marker))
    || LEASE_CHECK_MARKERS.iter().all(|marker| lowered.contains(marker))
  {
    PushRejection::NonFastForward
  } else if TIMEOUT_MARKERS.iter().any(|marker| lowered.contains(marker)) {
    PushRejection::Timeout
  } else if AUTH_MARKERS.iter().any(|marker| lowered.contains(marker)) {
    PushRejection::Auth
  } else if TRANSPORT_MARKERS.iter().any(|marker| lowered.contains(marker)) {
    PushRejection::Transport
  } else {
    PushRejection::Refused
  }
}

/// Whether the CI gate applies: the base must be the repository's default branch and that branch
/// must declare a workflow with a `pull_request` trigger. A probe failure is an error, not a
/// silently skipped gate.
async fn gate_applicability(repo_path: &Path, published_base: &str) -> anyhow::Result<GateApplicability> {
  let (default_branch, _) = git::git_remote_default_branch_and_tip(repo_path)
    .await
    .map_err(|error| anyhow::anyhow!("cannot read the default branch of {}: {error}", repo_path.display()))?;
  if default_branch != published_base {
    return Ok(GateApplicability::NotApplicable);
  }
  let workflows = workflow_files_at(repo_path, &format!("origin/{published_base}")).await?;
  for (path, content) in &workflows {
    if pull_request_trigger_of(path, content)?.is_some() {
      return Ok(GateApplicability::Applies);
    }
  }
  Ok(GateApplicability::NotApplicable)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GateApplicability {
  Applies,
  NotApplicable,
}

/// The delivery-side gate check (execution.rs): when the gate applies, a work commit that reached the
/// base branch without a `landing.json` in state `landed` blocks the delivery, so a hand push that
/// bypasses the gate cannot be reported as landed. The caller has already shown that the work adds
/// the commit, so a branch that adds nothing never reaches this check.
pub async fn landed_without_gate_blocker(
  repo_path: &Path,
  base_branch: &str,
  session_dir: &Path,
) -> anyhow::Result<Option<String>> {
  let published = review_text::review_published_branch(base_branch);
  if !matches!(gate_applicability(repo_path, published).await?, GateApplicability::Applies) {
    return Ok(None);
  }
  match read_landing_record(session_dir)? {
    Some(record) if record.state == STATE_LANDED => Ok(None),
    Some(record) => {
      Ok(Some(format!("landed without the CI gate: the task's landing record holds state {}", record.state)))
    }
    None => Ok(Some("landed without the CI gate".to_string())),
  }
}

/// The workflow files declared at *rev*: `(path, content)` pairs for `.github/workflows/*.{yml,yaml}`.
/// A repository without that directory has none — the no-CI repository case, not an error.
async fn workflow_files_at(workdir: &Path, rev: &str) -> anyhow::Result<Vec<(String, String)>> {
  let (ok, out, _) = git::git_stdout(
    workdir,
    &["ls-tree", "--name-only", &format!("{rev}:.github/workflows")],
    GIT_READ_TIMEOUT,
    "git ls-tree",
  )
  .await;
  if !ok {
    return Ok(Vec::new());
  }
  let mut files = Vec::new();
  for name in out.lines().map(str::trim).filter(|name| !name.is_empty()) {
    if !(name.ends_with(".yml") || name.ends_with(".yaml")) {
      continue;
    }
    let (ok, content, _) =
      git::git_stdout(workdir, &["show", &format!("{rev}:.github/workflows/{name}")], GIT_READ_TIMEOUT, "git show")
        .await;
    if ok {
      files.push((format!(".github/workflows/{name}"), content));
    }
  }
  Ok(files)
}

/// The `pull_request` trigger's key as YAML 1.1 and 1.2 both spell it: the boolean `true` key when
/// the parser resolved the bare word `on`, the string key otherwise.
fn trigger_section(workflow: &serde_yaml::Value) -> Option<&serde_yaml::Value> {
  let mapping = workflow.as_mapping()?;
  mapping.get("on").or_else(|| mapping.get(serde_yaml::Value::Bool(true)))
}

/// Whether the workflow declares a `pull_request` trigger, and whether that trigger carries a path
/// or branch filter (`Some(true)` means filtered).
fn pull_request_trigger(workflow: &serde_yaml::Value) -> Option<bool> {
  let trigger = trigger_section(workflow)?;
  match trigger {
    serde_yaml::Value::String(event) if event == "pull_request" => Some(false),
    serde_yaml::Value::Sequence(events) => {
      events.iter().any(|event| event.as_str() == Some("pull_request")).then_some(false)
    }
    serde_yaml::Value::Mapping(events) => {
      let pr = events.get("pull_request")?;
      match pr {
        serde_yaml::Value::Null => Some(false),
        serde_yaml::Value::Mapping(filters) => {
          Some(["paths", "paths-ignore", "branches", "branches-ignore"].iter().any(|key| filters.get(*key).is_some()))
        }
        _ => Some(false),
      }
    }
    _ => None,
  }
}

/// One workflow file's `pull_request` trigger (`pull_request_trigger`). A file that does not parse is an
/// error: a broken workflow must not drop out of the gate as if it declared no CI.
fn pull_request_trigger_of(path: &str, content: &str) -> anyhow::Result<Option<bool>> {
  let doc = serde_yaml::from_str::<serde_yaml::Value>(content)
    .map_err(|error| anyhow::anyhow!("{path} is not valid YAML: {error}"))?;
  Ok(pull_request_trigger(&doc))
}

/// The expected set: the workflows declared with a `pull_request` trigger and no path or branch
/// filter. A filtered workflow counts once it registers, so it is not expected to register.
pub fn expected_workflow_paths(workflows: &[(String, String)]) -> anyhow::Result<Vec<String>> {
  let mut expected = Vec::new();
  for (path, content) in workflows {
    if pull_request_trigger_of(path, content)? == Some(false) {
      expected.push(path.clone());
    }
  }
  expected.sort();
  Ok(expected)
}

/// The GitHub-facing CI state of one head sha, read fresh from the check runs, commit statuses and
/// workflow runs of that sha.
struct CiEvaluation {
  runs: Value,
  check_runs: Value,
  statuses: Value,
}

impl CiEvaluation {
  /// The evaluation of a read the rate limit refused: no runs, no check runs and no statuses.
  fn unread() -> Self {
    Self { runs: Value::Array(Vec::new()), check_runs: Value::Array(Vec::new()), statuses: Value::Array(Vec::new()) }
  }

  fn check_rows(&self) -> Vec<CheckRow> {
    let mut rows = Vec::new();
    if let Some(check_runs) = self.check_runs.as_array() {
      for check in check_runs {
        rows.push(CheckRow {
          name: string_field(check, "name").unwrap_or_default(),
          conclusion: string_field(check, "conclusion"),
          url: string_field(check, "html_url"),
          completed_at: string_field(check, "completed_at"),
        });
      }
    }
    if let Some(statuses) = self.statuses.as_array() {
      for status in statuses {
        rows.push(CheckRow {
          name: string_field(status, "context").unwrap_or_default(),
          conclusion: string_field(status, "state"),
          url: string_field(status, "target_url"),
          completed_at: None,
        });
      }
    }
    rows
  }

  /// The time of the last completed check, the pass evidence the acceptance flow reads back.
  fn ci_passed_at(&self) -> Option<String> {
    self
      .check_rows()
      .iter()
      .filter_map(|row| row.completed_at.clone())
      .filter(|completed_at| !completed_at.is_empty())
      .max()
  }
}

/// One read of a head sha's CI state. A rate-limited read holds no evaluation: the landing waits for the
/// quota instead of judging a state it did not read.
enum CiRead {
  Evaluated(Box<CiEvaluation>),
  RateLimited { reset: i64 },
}

async fn read_ci_state(workdir: &Path, repo: &str, sha: &str) -> anyhow::Result<CiRead> {
  let run = |args: Vec<String>| async move { run_gh(workdir, &args).await };
  let paths = [
    format!("repos/{repo}/actions/runs?head_sha={sha}&per_page=100"),
    format!("repos/{repo}/commits/{sha}/check-runs?per_page=100"),
    format!("repos/{repo}/commits/{sha}/status"),
  ];
  let mut bodies = Vec::with_capacity(paths.len());
  for path in &paths {
    match rest_get(&run, path, None).await? {
      RestOutcome::Body { value, .. } => bodies.push(value),
      RestOutcome::RateLimited { reset } => return Ok(CiRead::RateLimited { reset }),
      RestOutcome::NotModified => anyhow::bail!("gh api {path} answered 304 to a GET without an ETag"),
    }
  }
  let [runs, check_runs, statuses]: [Value; 3] = bodies.try_into().expect("one body per path");
  Ok(CiRead::Evaluated(Box::new(CiEvaluation {
    runs: runs.get("workflow_runs").cloned().unwrap_or(Value::Array(Vec::new())),
    check_runs: check_runs.get("check_runs").cloned().unwrap_or(Value::Array(Vec::new())),
    statuses: statuses.get("statuses").cloned().unwrap_or(Value::Array(Vec::new())),
  })))
}

/// The one CI verdict over a head sha. Expected workflows are matched by workflow file path;
/// `event` must name `pull_request`, because only the PR runs sit on the landing sha.
pub fn judge_ci(
  expected: &[String],
  runs: &Value,
  check_runs: &Value,
  statuses: &Value,
  elapsed_secs: i64,
) -> CiVerdict {
  let mut failing = Vec::new();
  let mut incomplete = false;
  let mut expected_missing = Vec::new();
  let mut expected_pending = false;

  if let Some(checks) = check_runs.as_array() {
    for check in checks {
      match string_field(check, "conclusion") {
        Some(conclusion) => {
          if !matches!(conclusion.as_str(), "success" | "skipped" | "neutral") {
            failing.push(CheckRow {
              name: string_field(check, "name").unwrap_or_default(),
              conclusion: Some(conclusion),
              url: string_field(check, "html_url"),
              completed_at: string_field(check, "completed_at"),
            });
          }
        }
        None => incomplete = true,
      }
    }
  }
  if let Some(list) = statuses.as_array() {
    for status in list {
      let Some(state) = string_field(status, "state") else { continue };
      match state.as_str() {
        "success" | "expected" | "neutral" => {}
        "pending" => incomplete = true,
        _ => failing.push(CheckRow {
          name: string_field(status, "context").unwrap_or_default(),
          conclusion: Some(state),
          url: string_field(status, "target_url"),
          completed_at: None,
        }),
      }
    }
  }
  let runs = runs.as_array().map(Vec::as_slice).unwrap_or(&[]);
  for path in expected {
    let sha_runs: Vec<&Value> = runs
      .iter()
      .filter(|run| string_field(run, "path").as_deref() == Some(path.as_str()))
      .filter(|run| string_field(run, "event").as_deref() == Some("pull_request"))
      .collect();
    if sha_runs.is_empty() {
      expected_missing.push(path.clone());
      continue;
    }
    let covered = sha_runs.iter().any(|run| {
      string_field(run, "status").as_deref() == Some("completed")
        && matches!(string_field(run, "conclusion").as_deref(), Some("success") | Some("skipped") | Some("neutral"))
    });
    if !covered {
      expected_pending = true;
    }
  }
  for run in runs.iter().filter(|run| string_field(run, "event").as_deref() == Some("pull_request")) {
    match string_field(run, "conclusion") {
      Some(conclusion) if matches!(conclusion.as_str(), "success" | "skipped" | "neutral") => {}
      Some(conclusion) => failing.push(CheckRow {
        name: string_field(run, "name").unwrap_or_default(),
        conclusion: Some(conclusion),
        url: string_field(run, "html_url"),
        completed_at: string_field(run, "completed_at"),
      }),
      None => incomplete = true,
    }
  }

  if !failing.is_empty() {
    return CiVerdict::Failed { failing };
  }
  if !expected_missing.is_empty() {
    return if elapsed_secs > NOT_STARTED_SECS {
      CiVerdict::NotStarted { workflow: expected_missing[0].clone() }
    } else {
      CiVerdict::Waiting
    };
  }
  if incomplete || expected_pending {
    return if elapsed_secs > TIMEOUT_SECS { CiVerdict::Timeout } else { CiVerdict::Waiting };
  }
  CiVerdict::Pass
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CiVerdict {
  Pass,
  Waiting,
  NotStarted { workflow: String },
  Timeout,
  Failed { failing: Vec<CheckRow> },
}

/// Whether a rebase that rewrote the head counts as a retry: the landing's first rebase of a task
/// without a previous round is the initial base, not a loss against main; every later rewrite —
/// a push rejection's re-rebase, or a rebase after main moved — counts one.
fn rewrite_counts_as_retry(prev: Option<&LandingRecord>, first_round: bool) -> bool {
  !(first_round && prev.is_none())
}

/// The clock start a round carries: the same head sha keeps the previous round's push time, every
/// new sha — a rebase's rewrite or a reviewer's fix commit — starts both clocks again.
fn carried_pushed_at(prev: Option<&LandingRecord>, sha: &str, now: &str) -> Option<String> {
  match prev.filter(|record| record.sha == sha) {
    Some(record) => record.head_pushed_at.clone().or_else(|| Some(now.to_string())),
    None => Some(now.to_string()),
  }
}

fn now_rfc3339() -> String {
  OffsetDateTime::now_utc().format(&Rfc3339).expect("the RFC 3339 format accepts now")
}

fn elapsed_secs(pushed_at: &str) -> anyhow::Result<i64> {
  let pushed = OffsetDateTime::parse(pushed_at, &Rfc3339)
    .map_err(|error| anyhow::anyhow!("landing record's head_pushed_at is not RFC 3339 ({pushed_at}): {error}"))?;
  Ok((OffsetDateTime::now_utc() - pushed).whole_seconds())
}

/// Read the task session directory's `landing.json`. A malformed record is an error, never a
/// silently fresh landing.
pub fn read_landing_record(session_dir: &Path) -> anyhow::Result<Option<LandingRecord>> {
  let path = session_dir.join(LANDING_FILE_NAME);
  let text = match std::fs::read_to_string(&path) {
    Ok(text) => text,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
    Err(error) => return Err(anyhow::anyhow!("{}: {error}", path.display())),
  };
  serde_json::from_str(&text).map(Some).map_err(|error| anyhow::anyhow!("{}: {error}", path.display()))
}

/// Writes the landing record whole: a queued command reads it while `land` may write it, so the text is staged under a
/// fresh name and renamed over the record in one step.
fn write_landing_record(session_dir: &Path, record: &LandingRecord) -> anyhow::Result<()> {
  let path = session_dir.join(LANDING_FILE_NAME);
  let text = serde_json::to_string_pretty(record).expect("the landing record serializes") + "\n";
  let staged = session_dir.join(format!("{LANDING_FILE_NAME}.{}.tmp", Uuid::new_v4()));
  std::fs::write(&staged, text).map_err(|error| anyhow::anyhow!("{}: {error}", staged.display()))?;
  std::fs::rename(&staged, &path).map_err(|error| anyhow::anyhow!("{}: {error}", path.display()))
}

async fn gh_json(workdir: &Path, args: &[&str]) -> anyhow::Result<Value> {
  let stdout = gh_stdout(workdir, args).await?;
  serde_json::from_slice(&stdout).map_err(|error| anyhow::anyhow!("gh {} printed no JSON: {error}", args.join(" ")))
}

async fn gh_stdout(workdir: &Path, args: &[&str]) -> anyhow::Result<Vec<u8>> {
  let owned: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
  let output = run_gh(workdir, &owned).await?;
  if !output.success {
    anyhow::bail!("gh {} failed: {}", owned.join(" "), output.stderr.trim());
  }
  Ok(output.stdout)
}

/// One commit's subject and body from the branch range used to derive a gated PR description.
#[derive(Clone, Debug, PartialEq, Eq)]
struct CommitMessage {
  subject: String,
  body: String,
}

/// The genre file path used when a branch holds several commits.
fn pr_text_genre_path() -> String {
  format!("~/{}/skills/writing-style/genres/commits-and-prs.md", charliebot_core::home::CLAUDE_CONFIG_DIR_NAME)
}

/// PR text after applying explicit values to the commit-derived defaults.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ResolvedPrText {
  title: String,
  body: String,
}

async fn resolve_draft_pr_text(
  worktree: &Path,
  published_base: &str,
  options: &LandOptions,
) -> anyhow::Result<ResolvedPrText> {
  options.validate()?;
  let commits = if options.title.is_none() || options.body.is_none() {
    commits_ahead(worktree, published_base).await?
  } else {
    Vec::new()
  };
  resolve_pr_text(&commits, options)
}

/// The commits the task branch holds ahead of the published base, each as its subject and body (`commits_ahead`).
async fn commits_ahead(worktree: &Path, published_base: &str) -> anyhow::Result<Vec<CommitMessage>> {
  let range = format!("origin/{published_base}..HEAD");
  let (ok, out, err) =
    git::git_stdout(worktree, &["log", "-z", "--format=%s%n%b", &range], GIT_READ_TIMEOUT, "git log").await;
  if !ok {
    anyhow::bail!("git log {range} failed: {err}");
  }
  Ok(parse_commit_messages(&out))
}

fn parse_commit_messages(output: &str) -> Vec<CommitMessage> {
  output
    .split('\0')
    .filter(|record| !record.trim().is_empty())
    .map(|record| {
      let (subject, body) = record.split_once('\n').unwrap_or((record, ""));
      CommitMessage { subject: subject.to_string(), body: body.trim().to_string() }
    })
    .collect()
}

/// The PR text of a gated landing: explicit values win, and an omitted value derives from the single
/// commit. A branch of several commits has no one commit to derive a description from, so it takes
/// both values from the caller.
fn resolve_pr_text(commits: &[CommitMessage], options: &LandOptions) -> anyhow::Result<ResolvedPrText> {
  options.validate()?;
  if commits.len() > 1 {
    return match (&options.title, &options.body) {
      (Some(title), Some(body)) => Ok(ResolvedPrText { title: title.clone(), body: body.clone() }),
      _ => anyhow::bail!(
        "the branch has {} commits ahead of origin: a gated landing of several commits needs --title and --body \
         or --body-file, written by {}",
        commits.len(),
        pr_text_genre_path()
      ),
    };
  }
  let commit = commits.first();
  let title = match (&options.title, commit) {
    (Some(title), _) => title.clone(),
    (None, Some(commit)) => {
      if commit.subject.trim().is_empty() {
        anyhow::bail!("cannot derive a PR title from an empty commit subject");
      }
      commit.subject.clone()
    }
    (None, None) => anyhow::bail!("cannot derive a PR title: the branch has no commits ahead of origin"),
  };
  let body = match (&options.body, commit) {
    (Some(body), _) => body.clone(),
    (None, Some(commit)) => commit.body.trim().to_string(),
    (None, None) => String::new(),
  };
  Ok(ResolvedPrText { title, body })
}

/// The one draft PR the landing keeps: reused when one is open for the task branch, created
/// otherwise. Returns `(url, number)`.
async fn ensure_draft_pr(
  ctx: &TaskLanding,
  repo: &str,
  published_base: &str,
  text: &ResolvedPrText,
) -> anyhow::Result<(String, i64)> {
  if open_task_prs(ctx, repo).await?.is_empty() {
    let created = gh_stdout(
      &ctx.worktree,
      &[
        "pr",
        "create",
        "--repo",
        repo,
        "--draft",
        "--head",
        &ctx.branch_name,
        "--base",
        published_base,
        "--title",
        &text.title,
        "--body",
        &text.body,
      ],
    )
    .await;
    if let Err(create_error) = created {
      let concurrent = open_task_prs(ctx, repo).await.map_err(|list_error| {
        anyhow::anyhow!("gh pr create failed: {create_error:#}; checking for an open PR failed: {list_error:#}")
      })?;
      if concurrent.is_empty() {
        return Err(create_error);
      }
      tracing::info!(target: "charliebot_tasks", branch = %ctx.branch_name, error = %create_error, "land_pr_create_raced");
    }
  }

  let pr =
    gh_json(&ctx.worktree, &["pr", "view", &ctx.branch_name, "--repo", repo, "--json", "number,url,state,title,body"])
      .await?;
  if string_field(&pr, "state").as_deref() != Some("OPEN") {
    anyhow::bail!("gh pr view {} did not return an open PR", ctx.branch_name);
  }
  let current_title = string_field(&pr, "title").ok_or_else(|| anyhow::anyhow!("gh pr view printed no title"))?;
  let current_body = match pr.get("body") {
    Some(Value::String(body)) => body.as_str(),
    Some(Value::Null) => "",
    _ => anyhow::bail!("gh pr view printed no body"),
  };
  if current_title != text.title || current_body != text.body {
    let number =
      pr.get("number").and_then(Value::as_i64).ok_or_else(|| anyhow::anyhow!("gh pr view printed no number"))?;
    // `gh pr edit` fails on gh 2.101 before its write (the Projects (classic) lookup), so an open PR's
    // title and body go through the REST pull endpoint.
    gh_stdout(
      &ctx.worktree,
      &[
        "api",
        &format!("repos/{repo}/pulls/{number}"),
        "-X",
        "PATCH",
        "-f",
        &format!("title={}", text.title),
        "-f",
        &format!("body={}", text.body),
      ],
    )
    .await?;
  }
  pr_url_and_number(&pr)
}

fn pr_url_and_number(pr: &Value) -> anyhow::Result<(String, i64)> {
  let url = string_field(pr, "url").ok_or_else(|| anyhow::anyhow!("gh pr view printed no url"))?;
  let number =
    pr.get("number").and_then(Value::as_i64).ok_or_else(|| anyhow::anyhow!("gh pr view printed no number"))?;
  Ok((url, number))
}

/// One open PR of the task branch.
struct OpenPr {
  number: i64,
  head_sha: String,
}

/// Leave no open PR of the task branch once the base branch holds *sha*.
///
/// GitHub marks a PR merged when its head sha reaches the base branch. The gated path pushes the
/// task branch at *sha* before the base push, so its PR merges on GitHub's side, and the call waits
/// `PR_MERGE_WAIT` for that mark. The ungated path never pushes the task branch, so a PR opened on
/// it keeps an older head; after a rebase that head holds a patch-equivalent copy of the landed
/// commits under another sha, and GitHub never marks the PR merged. Each PR still open is closed
/// with a comment naming the landed sha. GitHub's own merge mark can overtake a close and refuse
/// it, so the read-back after the closes is the verdict. A task branch absent from origin has no
/// open PR, because GitHub closes a PR whose head branch is deleted, so that landing calls no `gh`.
async fn close_task_prs(ctx: &TaskLanding, published: &str, sha: &str) -> anyhow::Result<()> {
  if !origin_has_branch(ctx, &ctx.branch_name).await? {
    return Ok(());
  }
  let deadline = tokio::time::Instant::now() + PR_MERGE_WAIT;
  let mut open = open_task_prs(ctx, &ctx.repo).await?;
  while open.iter().any(|pr| pr.head_sha == sha) && tokio::time::Instant::now() < deadline {
    tokio::time::sleep(PR_MERGE_POLL).await;
    open = open_task_prs(ctx, &ctx.repo).await?;
  }
  let comment = format!("Landed on `{published}` as {sha} by `charliebot land`.");
  let mut refusals = Vec::new();
  for pr in &open {
    let number = pr.number.to_string();
    let closed = gh_stdout(&ctx.worktree, &["pr", "close", &number, "--repo", &ctx.repo, "--comment", &comment]).await;
    if let Err(error) = closed {
      // GitHub can mark the PR merged while the close runs, which refuses the close; the read-back decides.
      refusals.push(format!("#{number}: {error:#}"));
    }
  }
  let left = open_task_prs(ctx, &ctx.repo).await?;
  if !left.is_empty() {
    let numbers = left.iter().map(|pr| format!("#{}", pr.number)).collect::<Vec<_>>().join(", ");
    anyhow::bail!("PR {numbers} of {} still reads open after the close ({})", ctx.branch_name, refusals.join("; "));
  }
  for refusal in &refusals {
    tracing::info!(target: "charliebot_tasks", branch = %ctx.branch_name, refusal = %refusal, "land_pr_close_refused");
  }
  Ok(())
}

async fn open_task_prs(ctx: &TaskLanding, repo: &str) -> anyhow::Result<Vec<OpenPr>> {
  let listed = gh_json(
    &ctx.worktree,
    &["pr", "list", "--repo", repo, "--head", &ctx.branch_name, "--state", "open", "--json", "number,headRefOid"],
  )
  .await?;
  let rows = listed.as_array().ok_or_else(|| anyhow::anyhow!("gh pr list printed no array"))?;
  rows
    .iter()
    .map(|row| {
      let number = row
        .get("number")
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow::anyhow!("gh pr list printed a PR without a number"))?;
      let head_sha = string_field(row, "headRefOid")
        .ok_or_else(|| anyhow::anyhow!("gh pr list printed PR #{number} without a head sha"))?;
      Ok(OpenPr { number, head_sha })
    })
    .collect()
}

#[cfg(test)]
mod tests;

// Every test in this module launches git, and each credential check also launches the bash leak scanner. A test that
// launches a process is an integration test (dev/check-integration-ignores.mjs), so each carries
// #[ignore = "integration"]: the unit gate skips it, and the Integration job runs it.
#[cfg(test)]
mod synthetic_repo_tests;
