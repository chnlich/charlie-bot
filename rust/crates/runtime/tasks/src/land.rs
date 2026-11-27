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
//! atomic push that also releases the turn and the entry. Only a commit that passed CI on itself
//! reaches the base branch. Both clocks — the 10-minute not-started clock and the 90-minute timeout —
//! run per head sha from the time that sha was pushed to the task branch, so a rebase starts both again
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

use crate::git;
use crate::github_api::{RestOutcome, rest_get, run_gh};
use crate::land_queue::{self, Cadence};
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

/// The repo, base, branch, worktree and session directory one landing acts on, all from the task record.
pub(crate) struct TaskLanding {
  session_id: String,
  repo: String,
  worktree: PathBuf,
  repo_path: PathBuf,
  base_branch: String,
  branch_name: String,
  session_dir: PathBuf,
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
      program: PathBuf::from("charliebot"),
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
    if !Path::new(&worktree).is_dir() {
      anyhow::bail!("worktree {worktree} of task {task_session_id} does not exist");
    }
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
/// queue's wake registered. A repeat call for a head that already landed returns its record.
pub async fn land(
  registry: &Registry,
  home: &Path,
  task_session_id: &str,
  options: &LandOptions,
) -> anyhow::Result<LandingRecord> {
  options.validate()?;
  let ctx = TaskLanding::load(home, task_session_id).await?;
  let published = review_text::review_published_branch(&ctx.base_branch);
  let base_ref = format!("origin/{published}");
  fetch_base(&ctx.worktree, published).await?;
  let landed_head = head_sha(&ctx.worktree).await?;
  let prev = read_landing_record(&ctx.session_dir)?;
  if let Some(record) = prev.as_ref().filter(|record| record.sha == landed_head && is_landed(&record.state)) {
    return Ok(record.clone());
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
    push_task_branch(&ctx, &ctx.branch_name).await?;
    let (pr_url, pr_number) = ensure_draft_pr(&ctx, &ctx.repo, published, &pr_text).await?;
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
}

/// One head sha the turn holder works on: the sha, when its branch was pushed, its rebase retries and its CI read.
struct Round {
  sha: String,
  head_pushed_at: Option<String>,
  retries: i64,
  snapshot: Option<CiSnapshot>,
}

/// The queued landing of one task, over the remote `R`.
async fn queued_landing<R: LandRemote>(
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
  let lane = Lane { ctx, registry, home, prev: &prev, gated, cadence };
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
      if head_sha(&ctx.worktree).await? != round.sha {
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
      write_landing_record(&ctx.session_dir, &record)?;
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
  if let Err(error) = write_landing_record(&ctx.session_dir, &waiting) {
    claim.release().await?;
    return Err(error);
  }
  round.snapshot = Some(read);

  loop {
    match head_sha(&ctx.worktree).await {
      Ok(head) if head != round.sha => {
        claim.release().await?;
        return Ok(QueueEnd::Superseded);
      }
      Ok(_) => {}
      Err(error) => {
        claim.release().await?;
        return Err(error);
      }
    }
    match land_queue::turn_step(&mut claim, cadence).await {
      Ok(land_queue::Step::Held) => break,
      Ok(land_queue::Step::Waiting) => tokio::time::sleep(cadence.poll).await,
      Err(error) => {
        // A newer landing call may have replaced the entry between the head check and the heartbeat.
        let released = claim.release().await;
        if head_sha(&ctx.worktree).await? != round.sha {
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
    if head_sha(&ctx.worktree).await? != round.sha {
      claim.release().await?;
      return Ok(QueueEnd::Superseded);
    }
    fetch_base(&ctx.worktree, &published).await?;
    match rebase_onto_base(&ctx.worktree, &base_ref).await? {
      RebaseStep::Conflict => {
        abort_rebase(&ctx.worktree).await;
        claim.release().await?;
        let record = stopped_record(lane, round, STATE_REBASE_CONFLICT, &CiSnapshot::unread());
        write_landing_record(&ctx.session_dir, &record)?;
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
        if head_sha(&ctx.worktree).await? != round.sha {
          claim.release().await?;
          return Ok(QueueEnd::Superseded);
        }
        if let Some(end) = keep_turn_alive(ctx, claim, &round.sha, lane.cadence).await? {
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
        write_landing_record(&ctx.session_dir, &record)?;
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
        write_landing_record(&ctx.session_dir, &record)?;
        remote.close_task_prs(ctx, &published, &record.sha).await.map_err(|error| {
          anyhow::anyhow!("{published} holds {} and landing.json reads {}, but {error:#}", record.sha, record.state)
        })?;
        return Ok(QueueEnd::Record(Box::new(record)));
      }
      land_queue::Pushed::Refused(err) => {
        if push_rejection_kind(&err) == PushRejection::RepositoryRules {
          claim.release().await?;
          let record = stopped_record(lane, round, STATE_PR_MERGE_REQUIRED, &read);
          write_landing_record(&ctx.session_dir, &record)?;
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

/// Renews the turn's heartbeat when it is due. A refused renewal ends the holder: when the head moved on, the landing
/// is superseded and the turn is released; otherwise the refs were removed under the holder and that is an error.
async fn keep_turn_alive(
  ctx: &TaskLanding,
  claim: &mut land_queue::Claim,
  sha: &str,
  cadence: Cadence,
) -> anyhow::Result<Option<QueueEnd>> {
  if !claim.beat_due(cadence) {
    return Ok(None);
  }
  let parent = claim.parent.clone();
  if claim.beat(&parent).await? == land_queue::Pushed::Done {
    return Ok(None);
  }
  claim.release().await?;
  if head_sha(&ctx.worktree).await? != sha {
    return Ok(Some(QueueEnd::Superseded));
  }
  anyhow::bail!("the landing queue refs of {} were removed while {sha} held the turn", claim.holder)
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

/// The task branch push under `--force-with-lease`, so a rebase never clobbers another push.
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
  let (ok, _, err) =
    git::git_stdout(&ctx.worktree, &["push", &lease, "origin", branch], GIT_PUSH_TIMEOUT, "git push task branch").await;
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

fn write_landing_record(session_dir: &Path, record: &LandingRecord) -> anyhow::Result<()> {
  let path = session_dir.join(LANDING_FILE_NAME);
  let text = serde_json::to_string_pretty(record).expect("the landing record serializes") + "\n";
  std::fs::write(&path, text).map_err(|error| anyhow::anyhow!("{}: {error}", path.display()))
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
mod tests {
  use std::collections::HashSet;

  use super::*;
  use serde_json::json;

  fn workflow(path: &str, body: &str) -> (String, String) {
    (path.to_string(), body.to_string())
  }

  fn run(path: &str, status: &str, conclusion: Option<&str>) -> Value {
    let mut run = json!({ "path": path, "event": "pull_request", "status": status, "name": "test" });
    if let Some(conclusion) = conclusion {
      run["conclusion"] = json!(conclusion);
    }
    run
  }

  #[test]
  fn a_removed_line_is_left_out_of_the_added_text() {
    let patch = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old fictionalmarker\n+new text\n";
    assert_eq!(added_text(patch), "diff --git a/x b/x\n+++ b/x\nnew text\n");
  }

  #[test]
  fn one_commit_supplies_its_subject_and_trimmed_body() {
    let commits = parse_commit_messages("Add the route\n\nExplain the contract.\n\0");
    let text = resolve_pr_text(&commits, &LandOptions::default()).expect("one commit resolves");
    assert_eq!(text.title, "Add the route");
    assert_eq!(text.body, "Explain the contract.");
  }

  #[test]
  fn several_commits_need_both_an_explicit_title_and_body() {
    let commits =
      parse_commit_messages("Add the route\n\nExplain the contract.\n\0Handle errors\n\nName the failure.\n\0");
    let refusal = resolve_pr_text(&commits, &LandOptions::default()).expect_err("two commits need explicit text");
    let refusal = format!("{refusal:#}");
    assert!(refusal.contains("2 commits"), "{refusal}");
    assert!(refusal.contains("--title"), "{refusal}");
    assert!(refusal.contains(&pr_text_genre_path()), "{refusal}");
    let title_only = LandOptions { title: Some("Routing".to_string()), body: None, evidence_dir: None };
    assert!(resolve_pr_text(&commits, &title_only).is_err());
    let body_only = LandOptions { title: None, body: Some("Why.".to_string()), evidence_dir: None };
    assert!(resolve_pr_text(&commits, &body_only).is_err());
    let explicit =
      LandOptions { title: Some("Routing".to_string()), body: Some("Why.".to_string()), evidence_dir: None };
    assert_eq!(
      resolve_pr_text(&commits, &explicit).expect("explicit text resolves"),
      ResolvedPrText { title: "Routing".to_string(), body: "Why.".to_string() }
    );
  }

  #[test]
  fn explicit_pr_text_replaces_commit_derived_values() {
    let options = LandOptions {
      title: Some("Resolved title".to_string()),
      body: Some("Resolved body".to_string()),
      evidence_dir: None,
    };
    let text = resolve_pr_text(&[], &options).expect("explicit values need no commit range");
    assert_eq!(text, ResolvedPrText { title: "Resolved title".to_string(), body: "Resolved body".to_string() });
  }

  #[test]
  fn an_empty_explicit_title_fails_validation() {
    let options = LandOptions { title: Some(" \t\n".to_string()), body: None, evidence_dir: None };
    assert!(options.validate().is_err());
    assert!(resolve_pr_text(&[], &options).is_err());
  }

  #[test]
  fn an_evidence_directory_that_is_not_a_directory_fails_validation() {
    let dir = tempfile::tempdir().expect("temporary directory is available");
    let missing = LandOptions { evidence_dir: Some(dir.path().join("absent")), ..LandOptions::default() };
    assert!(missing.validate().is_err());
    let present = LandOptions { evidence_dir: Some(dir.path().to_path_buf()), ..LandOptions::default() };
    present.validate().expect("an existing directory passes");
  }

  #[test]
  fn a_missing_commit_title_fails_when_the_range_is_empty() {
    assert!(resolve_pr_text(&[], &LandOptions::default()).is_err());
  }

  #[test]
  fn a_green_sha_passes_when_every_expected_workflow_completed_successfully() {
    let expected = [".github/workflows/ci.yml".to_string(), ".github/workflows/integration.yml".to_string()];
    let runs = json!([
      run(".github/workflows/ci.yml", "completed", Some("success")),
      run(".github/workflows/integration.yml", "completed", Some("success")),
    ]);
    let check_runs = json!([
      { "name": "rust-test", "status": "completed", "conclusion": "success", "completed_at": "2026-11-10T03:00:00Z" },
      { "name": "lint", "status": "completed", "conclusion": "skipped", "completed_at": "2026-11-10T03:01:00Z" },
    ]);
    let statuses = json!([{ "context": "docs", "state": "success" }]);
    let verdict = judge_ci(&expected, &runs, &check_runs, &statuses, 300);
    assert_eq!(verdict, CiVerdict::Pass);
  }

  #[test]
  fn a_failed_check_names_itself_and_its_url() {
    let expected = [".github/workflows/ci.yml".to_string()];
    let runs = json!([run(".github/workflows/ci.yml", "completed", Some("success"))]);
    let check_runs = json!([
      { "name": "rust-test", "status": "completed", "conclusion": "failure", "html_url": "https://runs/1" },
    ]);
    let statuses = json!([]);
    let verdict = judge_ci(&expected, &runs, &check_runs, &statuses, 300);
    assert_eq!(
      verdict,
      CiVerdict::Failed {
        failing: vec![CheckRow {
          name: "rust-test".to_string(),
          conclusion: Some("failure".to_string()),
          url: Some("https://runs/1".to_string()),
          completed_at: None,
        }]
      }
    );
  }

  #[test]
  fn a_running_check_waits_and_times_out_after_ninety_minutes() {
    let expected = [".github/workflows/ci.yml".to_string()];
    let runs = json!([run(".github/workflows/ci.yml", "in_progress", None)]);
    let check_runs = json!([]);
    let statuses = json!([]);
    assert_eq!(judge_ci(&expected, &runs, &check_runs, &statuses, 60), CiVerdict::Waiting);
    assert_eq!(judge_ci(&expected, &runs, &check_runs, &statuses, TIMEOUT_SECS + 1), CiVerdict::Timeout);
  }

  #[test]
  fn no_check_at_all_waits_until_the_not_started_clock_expires() {
    let expected = [".github/workflows/ci.yml".to_string()];
    let runs = json!([]);
    let check_runs = json!([]);
    let statuses = json!([]);
    assert_eq!(judge_ci(&expected, &runs, &check_runs, &statuses, 60), CiVerdict::Waiting);
    assert_eq!(
      judge_ci(&expected, &runs, &check_runs, &statuses, NOT_STARTED_SECS + 1),
      CiVerdict::NotStarted { workflow: ".github/workflows/ci.yml".to_string() }
    );
  }

  #[test]
  fn one_expected_workflow_not_yet_registered_is_not_started_past_ten_minutes() {
    let expected = [".github/workflows/ci.yml".to_string(), ".github/workflows/integration.yml".to_string()];
    let runs = json!([
      run(".github/workflows/ci.yml", "completed", Some("success")),
      { "path": ".github/workflows/ci.yml", "event": "push", "status": "completed", "conclusion": "success" },
    ]);
    let check_runs = json!([{ "name": "rust-test", "status": "completed", "conclusion": "success" }]);
    let statuses = json!([]);
    assert_eq!(judge_ci(&expected, &runs, &check_runs, &statuses, 60), CiVerdict::Waiting);
    assert_eq!(
      judge_ci(&expected, &runs, &check_runs, &statuses, NOT_STARTED_SECS + 1),
      CiVerdict::NotStarted { workflow: ".github/workflows/integration.yml".to_string() }
    );
  }

  #[test]
  fn a_failing_commit_status_fails_the_sha() {
    let expected = [".github/workflows/ci.yml".to_string()];
    let runs = json!([run(".github/workflows/ci.yml", "completed", Some("success"))]);
    let check_runs = json!([]);
    let statuses = json!([{ "context": "coverage", "state": "error", "target_url": "https://ci/coverage" }]);
    let verdict = judge_ci(&expected, &runs, &check_runs, &statuses, 60);
    assert_eq!(
      verdict,
      CiVerdict::Failed {
        failing: vec![CheckRow {
          name: "coverage".to_string(),
          conclusion: Some("error".to_string()),
          url: Some("https://ci/coverage".to_string()),
          completed_at: None,
        }]
      }
    );
  }

  #[test]
  fn only_unfiltered_pull_request_workflows_are_expected() {
    let workflows = [
      workflow(
        ".github/workflows/ci.yml",
        "name: CI\non:\n  pull_request:\n  push:\n    branches:\n      - main\njobs: {}\n",
      ),
      workflow(
        ".github/workflows/docs.yml",
        "name: Docs\non:\n  pull_request:\n    paths:\n      - 'docs/**'\njobs: {}\n",
      ),
      workflow(".github/workflows/nightly.yml", "name: Nightly\non:\n  schedule:\n    - cron: '0 3 * * *'\njobs: {}\n"),
      workflow(".github/workflows/triage.yml", "name: Triage\non: [push, workflow_dispatch]\njobs: {}\n"),
      workflow(".github/workflows/legacy.yml", "name: Legacy\ntriggers: [push]\njobs: {}\n"),
    ];
    assert_eq!(expected_workflow_paths(&workflows).unwrap(), vec![".github/workflows/ci.yml".to_string()]);
    // A filtered pull_request trigger still declares the trigger; a workflow without one does not.
    for (path, content) in &workflows[..2] {
      assert!(pull_request_trigger_of(path, content).unwrap().is_some());
    }
    for (path, content) in &workflows[2..] {
      assert!(pull_request_trigger_of(path, content).unwrap().is_none());
    }
  }

  #[test]
  fn a_workflow_that_does_not_parse_fails_the_gate_instead_of_dropping_out() {
    let broken = [workflow(".github/workflows/ci.yml", "on: [pull_request\n")];
    assert!(expected_workflow_paths(&broken).is_err());
    assert!(pull_request_trigger_of(".github/workflows/ci.yml", "on: [pull_request\n").is_err());
  }

  #[test]
  fn a_pending_commit_status_waits_instead_of_failing_the_sha() {
    let expected = [".github/workflows/ci.yml".to_string()];
    let runs = json!([run(".github/workflows/ci.yml", "completed", Some("success"))]);
    let statuses = json!([{ "context": "coverage", "state": "pending" }]);
    assert_eq!(judge_ci(&expected, &runs, &json!([]), &statuses, 60), CiVerdict::Waiting);
    assert_eq!(judge_ci(&expected, &runs, &json!([]), &statuses, TIMEOUT_SECS + 1), CiVerdict::Timeout);
  }

  #[test]
  fn an_unfiltered_null_or_listed_pull_request_trigger_is_not_filtered() {
    assert_eq!(
      pull_request_trigger(&serde_yaml::from_str::<serde_yaml::Value>("on: pull_request").unwrap()),
      Some(false)
    );
    assert_eq!(
      pull_request_trigger(&serde_yaml::from_str::<serde_yaml::Value>("on:\n  pull_request:\njobs: {}").unwrap()),
      Some(false)
    );
    assert_eq!(
      pull_request_trigger(&serde_yaml::from_str::<serde_yaml::Value>("on: [push, pull_request]").unwrap()),
      Some(false)
    );
    assert_eq!(
      pull_request_trigger(
        &serde_yaml::from_str::<serde_yaml::Value>("on:\n  pull_request:\n    branches-ignore:\n      - wip-*")
          .unwrap()
      ),
      Some(true)
    );
    assert_eq!(pull_request_trigger(&serde_yaml::from_str::<serde_yaml::Value>("on: push").unwrap()), None);
  }

  fn record(state: &str, sha: &str, head_pushed_at: &str, rebase_retries: i64) -> LandingRecord {
    LandingRecord {
      state: state.to_string(),
      repo: "owner/repo".to_string(),
      base: "main".to_string(),
      sha: sha.to_string(),
      pr_url: None,
      checks: Vec::new(),
      head_pushed_at: Some(head_pushed_at.to_string()),
      ci_passed_at: None,
      pushed_at: None,
      rebase_retries,
      queue_ref: None,
    }
  }

  #[test]
  fn a_rebase_that_rewrote_the_head_restarts_both_clocks_and_counts_a_retry() {
    let prev = record(STATE_WAITING, "a".repeat(40).as_str(), "2026-11-10T01:00:00Z", 2);
    // The same head sha keeps the previous round's push time and retry count.
    let sha = "a".repeat(40);
    assert_eq!(carried_pushed_at(Some(&prev), &sha, "2026-11-10T02:00:00Z"), Some("2026-11-10T01:00:00Z".to_string()));
    // A rebase's rewrite starts both clocks again and counts one retry; the caller asks only
    // about a rebase that rewrote the head, so a round without one never reaches the question.
    let new_sha = "b".repeat(40);
    assert_eq!(
      carried_pushed_at(Some(&prev), &new_sha, "2026-11-10T02:00:00Z"),
      Some("2026-11-10T02:00:00Z".to_string())
    );
    assert!(rewrite_counts_as_retry(Some(&prev), true));
    // The first rebase of a task without a previous round is the initial base, not a retry.
    assert!(!rewrite_counts_as_retry(None, true));
    // A terminal record is dropped before this point, but a fresh round over one keeps its retries.
    let landed = record(STATE_LANDED, new_sha.as_str(), "2026-11-10T01:30:00Z", 3);
    assert_eq!(
      carried_pushed_at(Some(&landed), new_sha.as_str(), "2026-11-10T04:00:00Z"),
      Some("2026-11-10T01:30:00Z".to_string())
    );
  }

  #[test]
  fn push_rejections_split_a_verified_move_from_the_terminal_classes() {
    // Repository rules keep their pr-merge-required answer.
    assert_eq!(
      push_rejection_kind("remote: error: GH013: Repository rule violations found for refs/heads/main"),
      PushRejection::RepositoryRules
    );
    assert_eq!(
      push_rejection_kind("! [remote rejected] main -> main (repository rules prohibit push)"),
      PushRejection::RepositoryRules
    );
    // Only git's own report of a stale remote tip classifies as a possible base move.
    assert_eq!(
      push_rejection_kind(
        "To github.com:owner/repo.git\n ! [rejected] abc123 -> main (non-fast-forward)\nerror: failed to push some refs"
      ),
      PushRejection::NonFastForward
    );
    assert_eq!(
      push_rejection_kind(
        "To github.com:owner/repo.git\n ! [rejected] abc123 -> main (fetch first)\nerror: failed to push some refs"
      ),
      PushRejection::NonFastForward
    );
    // A lease check that found the ref already moved names the same possible base move.
    assert_eq!(
      push_rejection_kind(
        "remote: error: cannot lock ref 'refs/heads/main': is at 1111111 but expected 2222222\n\
         To https://github.com/owner/repo.git\n ! [remote rejected] abc123 -> main (cannot lock ref)\n\
         error: failed to push some refs"
      ),
      PushRejection::NonFastForward
    );
    // A hook refusal, an SSH reset, a timeout and an auth error each carry their own class.
    let hook = "pre-push: push to dev is restricted; use merge_to_dev.sh <branch>\n\
                error: failed to push some refs to 'gitee.com:owner/repo.git'";
    assert_eq!(push_rejection_kind(hook), PushRejection::Refused);
    assert_eq!(
      push_rejection_kind(
        "remote: push to main is restricted; use merge_to_dev.sh <branch>\nTo ../origin.git\n\
         ! [remote rejected] abc123 -> main (pre-receive hook declined)\n\
         error: failed to push some refs to '../origin.git'"
      ),
      PushRejection::Refused
    );
    // A ref-lock failure without the lease wording is a local or hook lock conflict, not a move.
    assert_eq!(
      push_rejection_kind(
        "remote: error: cannot lock ref 'refs/heads/main': Unable to create 'refs/heads/main.lock': File exists"
      ),
      PushRejection::Refused
    );
    assert_eq!(
      push_rejection_kind(
        "kex_exchange_identification: read: Connection reset by peer\nfatal: Could not read from remote repository."
      ),
      PushRejection::Transport
    );
    assert_eq!(push_rejection_kind("git push timed out after 120.0s"), PushRejection::Timeout);
    assert_eq!(
      push_rejection_kind("ssh: connect to host gitee.com port 22: Connection timed out"),
      PushRejection::Timeout
    );
    assert_eq!(
      push_rejection_kind(
        "git@gitee.com: Permission denied (publickey).\nfatal: Could not read from remote repository."
      ),
      PushRejection::Auth
    );
  }

  #[test]
  fn landing_states_carry_distinct_exit_codes_with_the_landings_at_zero() {
    let states = [
      (STATE_LANDED, 0),
      (STATE_LANDED_NO_CI, 0),
      (STATE_WAITING, 8),
      (STATE_REBASE_CONFLICT, 3),
      (STATE_CI_FAILED, 4),
      (STATE_CI_NOT_STARTED, 5),
      (STATE_CI_TIMEOUT, 6),
      (STATE_PR_MERGE_REQUIRED, 7),
    ];
    let mut codes = Vec::new();
    for (state, expected) in states {
      let code = LandState::try_from(state).expect("state parses").exit_code();
      assert_eq!(code, expected, "state {state}");
      codes.push(code);
    }
    let nonzero: HashSet<i32> = codes.into_iter().filter(|code| *code != 0).collect();
    assert_eq!(nonzero.len(), 6);
  }

  #[test]
  fn the_landing_record_round_trips_through_landing_json() {
    let session = tempfile::tempdir().expect("temporary directory is available");
    let record = LandingRecord {
      state: STATE_LANDED.to_string(),
      repo: "owner/repo".to_string(),
      base: "main".to_string(),
      sha: "a".repeat(40),
      pr_url: Some("https://github.com/owner/repo/pull/7".to_string()),
      checks: vec![CheckRow {
        name: "rust-test".to_string(),
        conclusion: Some("success".to_string()),
        url: Some("https://runs/1".to_string()),
        completed_at: Some("2026-11-10T03:00:00Z".to_string()),
      }],
      head_pushed_at: Some("2026-11-10T02:00:00Z".to_string()),
      ci_passed_at: Some("2026-11-10T03:01:00Z".to_string()),
      pushed_at: Some("2026-11-10T03:02:00Z".to_string()),
      rebase_retries: 1,
      queue_ref: None,
    };
    write_landing_record(session.path(), &record).expect("the record is written");
    assert_eq!(read_landing_record(session.path()).expect("the record is read"), Some(record));
  }

  #[test]
  fn a_malformed_landing_record_is_an_error_and_a_missing_one_is_none() {
    let session = tempfile::tempdir().expect("temporary directory is available");
    assert_eq!(read_landing_record(session.path()).expect("no record reads as none"), None);
    std::fs::write(session.path().join(LANDING_FILE_NAME), "{not json").expect("the malformed record is written");
    assert!(read_landing_record(session.path()).is_err());
  }

  #[test]
  fn the_ci_passed_at_is_the_last_completed_check() {
    let evaluation = CiEvaluation {
      runs: Value::Array(Vec::new()),
      check_runs: json!([
        { "name": "a", "status": "completed", "conclusion": "success", "completed_at": "2026-11-10T03:00:00Z" },
        { "name": "b", "status": "completed", "conclusion": "success", "completed_at": "2026-11-10T03:04:00Z" },
      ]),
      statuses: json!([]),
    };
    assert_eq!(evaluation.ci_passed_at(), Some("2026-11-10T03:04:00Z".to_string()));
  }

  #[test]
  fn the_remote_url_names_the_owner_repo_path() {
    assert_eq!(git::repo_from_remote_url("https://github.com/owner/repo.git"), Some("owner/repo".to_string()));
    assert_eq!(git::repo_from_remote_url("git@github.com:owner/repo.git"), Some("owner/repo".to_string()));
    assert_eq!(git::repo_from_remote_url("ssh://git@github.com/owner/repo"), Some("owner/repo".to_string()));
    assert_eq!(git::repo_from_remote_url("ssh://git@github.com:22/owner/repo.git"), Some("owner/repo".to_string()));
    assert_eq!(git::repo_from_remote_url("https://github.com/owner/repo/"), Some("owner/repo".to_string()));
    assert_eq!(git::repo_from_remote_url("not-a-remote"), None);
  }

  /// The check reads each changed file that still exists, and a changed Rust file's whole crate, so a `cfg(test)`
  /// module declared in an unchanged parent is test code. A file the diff removed is left out.
  #[test]
  fn the_literal_check_reads_the_changed_files_and_the_crates_of_changed_rust_files() {
    use std::fs;
    let root = tempfile::tempdir().unwrap();
    let worktree = root.path();
    let crate_dir = worktree.join("rust/crates/x/y");
    fs::create_dir_all(crate_dir.join("src")).unwrap();
    fs::write(crate_dir.join("Cargo.toml"), "[package]\n").unwrap();
    fs::write(crate_dir.join("src/lib.rs"), "").unwrap();
    fs::write(crate_dir.join("src/extra.rs"), "").unwrap();
    fs::create_dir_all(worktree.join("scripts")).unwrap();
    fs::write(worktree.join("scripts/run.sh"), "").unwrap();
    fs::write(worktree.join("scripts/notes.txt"), "").unwrap();
    let changed =
      ["rust/crates/x/y/src/extra.rs".to_string(), "scripts/run.sh".to_string(), "scripts/removed.sh".to_string()];
    assert_eq!(literal_check_paths(worktree, &changed).unwrap(), vec![crate_dir, worktree.join("scripts/run.sh")]);
    // A path under a regular file fails to stat with something other than "not found": that is an error, not a skip.
    assert!(literal_check_paths(worktree, &["scripts/notes.txt/run.sh".to_string()]).is_err());
  }

  /// A changed file that holds a backend literal refuses the landing and names its file and line; once the literal is
  /// gone the same file passes.
  #[test]
  fn a_backend_literal_in_a_changed_file_refuses_the_landing_and_a_clean_file_passes() {
    use std::fs;
    let root = tempfile::tempdir().unwrap();
    let worktree = root.path();
    fs::create_dir_all(worktree.join("scripts")).unwrap();
    fs::write(worktree.join("scripts/run.sh"), "echo ok\nmodel=claude-opus-5-5\n").unwrap();
    let paths = [worktree.join("scripts/run.sh")];
    let added = AddedLines::from([(PathBuf::from("scripts/run.sh"), BTreeSet::from([2]))]);
    let text = format!(
      "{:#}",
      refuse_backend_literals(&Registry::new(), &worktree.join("home"), worktree, &paths, &added).unwrap_err()
    );
    assert!(text.contains("scripts/run.sh:2") && text.contains("claude-opus-5-5"), "{text}");

    fs::write(worktree.join("scripts/run.sh"), "echo ok\n").unwrap();
    refuse_backend_literals(&Registry::new(), &worktree.join("home"), worktree, &paths, &added)
      .expect("a clean file passes");
  }
}

// Every test in this module launches git, and each credential check also launches the bash leak scanner. A test that
// launches a process is an integration test (dev/check-integration-ignores.mjs), so each carries
// #[ignore = "integration"]: the unit gate skips it, and the Integration job runs it.
#[cfg(test)]
mod synthetic_repo_tests {
  use std::fs;
  use std::path::Path;
  use std::process::Command;

  use super::*;

  /// One synthetic repository with a bare `origin.git` beside it, on branch *branch*.
  fn synthetic_repo(root: &Path, branch: &str) -> std::path::PathBuf {
    synthetic_repo_inner(root, branch, false)
  }

  fn synthetic_queue_repo(root: &Path, branch: &str) -> std::path::PathBuf {
    synthetic_repo_inner(root, branch, true)
  }

  fn synthetic_repo_inner(root: &Path, branch: &str, scan_pushes: bool) -> std::path::PathBuf {
    let repo = root.join("repo");
    fs::create_dir(&repo).expect("repository directory is created");
    assert!(
      Command::new("git").args(["init", "-q", "-b", "main"]).arg(&repo).status().expect("git is installed").success()
    );
    assert!(
      Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["config", "user.email", "synthetic@example.test"])
        .status()
        .unwrap()
        .success()
    );
    assert!(
      Command::new("git").arg("-C").arg(&repo).args(["config", "user.name", "Synthetic"]).status().unwrap().success()
    );
    let origin = root.join("origin.git");
    assert!(
      Command::new("git")
        .args(["init", "-q", "--bare", "--initial-branch=main"])
        .arg(&origin)
        .status()
        .unwrap()
        .success()
    );
    if scan_pushes {
      charliebot_shell_harness::install_pre_receive_leak_scan(&origin, root)
        .expect("the origin leak scanner is installed");
    }
    fs::write(repo.join("tracked.txt"), "base\n").expect("the base file is written");
    assert!(Command::new("git").arg("-C").arg(&repo).args(["add", "."]).status().unwrap().success());
    assert!(Command::new("git").arg("-C").arg(&repo).args(["commit", "-q", "-m", "base"]).status().unwrap().success());
    assert!(
      Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["remote", "add", "origin", origin.to_str().expect("the path is UTF-8")])
        .status()
        .unwrap()
        .success()
    );
    assert!(
      Command::new("git").arg("-C").arg(&repo).args(["push", "-q", "-u", "origin", "main"]).status().unwrap().success()
    );
    if branch != "main" {
      assert!(
        Command::new("git").arg("-C").arg(&repo).args(["switch", "-q", "-c", branch]).status().unwrap().success()
      );
    }
    repo
  }

  /// Commits with the identity passed on the command line: a clone has no `user.*` config of its own,
  /// and a CI runner has no identity git can auto-detect.
  fn commit(repo: &Path, file: &str, content: &str) {
    fs::write(repo.join(file), content).expect("the file is written");
    assert!(Command::new("git").arg("-C").arg(repo).args(["add", "."]).status().unwrap().success());
    assert!(
      Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "user.name=Synthetic", "-c", "user.email=synthetic@example.test"])
        .args(["commit", "-q", "-m", file])
        .status()
        .unwrap()
        .success()
    );
  }

  fn head(repo: &Path) -> String {
    String::from_utf8(Command::new("git").arg("-C").arg(repo).args(["rev-parse", "HEAD"]).output().unwrap().stdout)
      .expect("the sha is UTF-8")
      .trim()
      .to_string()
  }

  /// A profile home whose credentials file holds `secret` as the `planted.api_key` leaf.
  fn planted_home(root: &Path, secret: &str) -> PathBuf {
    let home = root.join("home");
    fs::create_dir(&home).expect("the profile home is created");
    fs::write(home.join("credentials.yaml"), format!("planted:\n  api_key: {secret}\n"))
      .expect("the credentials file is written");
    home
  }

  /// One commit on the checked-out branch whose message is *message*.
  fn commit_with_message(repo: &Path, file: &str, content: &str, message: &str) {
    fs::write(repo.join(file), content).expect("the file is written");
    assert!(Command::new("git").arg("-C").arg(repo).args(["add", "."]).status().unwrap().success());
    assert!(
      Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "user.name=Synthetic", "-c", "user.email=synthetic@example.test"])
        .args(["commit", "-q", "-m", message])
        .status()
        .unwrap()
        .success()
    );
  }

  async fn fetch_main(repo: &Path) {
    assert!(git::git_stdout(repo, &["fetch", "origin", "main"], GIT_WRITE_TIMEOUT, "git fetch").await.0);
  }

  /// A landing venue over *worktree* on *branch*: the push reads only the worktree.
  fn landing_ctx(worktree: &Path, branch: &str) -> TaskLanding {
    TaskLanding {
      session_id: "synthetic-task".to_string(),
      repo: "synthetic/synthetic".to_string(),
      worktree: worktree.to_path_buf(),
      repo_path: worktree.to_path_buf(),
      base_branch: "main".to_string(),
      branch_name: branch.to_string(),
      session_dir: worktree.join("session"),
    }
  }

  fn git_ok(repo: &Path, args: &[&str]) {
    let output = Command::new("git").arg("-C").arg(repo).args(args).output().expect("git runs");
    assert!(output.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&output.stderr));
  }

  /// GitHub deletes a merged task branch on origin, but the landing's clone keeps
  /// `refs/remotes/origin/<branch>` at the merged tip. The push leases against an explicitly absent ref,
  /// so the rewritten branch publishes as a new branch; the implicit lease of the tracking ref would refuse
  /// it as `stale info`.
  #[tokio::test]
  #[ignore = "integration"]
  async fn a_merged_branch_absent_from_origin_publishes_its_rewrite_as_a_new_branch() {
    let root = tempfile::tempdir().expect("temporary directory is available");
    synthetic_repo(root.path(), "main");
    let origin = root.path().join("origin.git");
    let work = root.path().join("work");
    git_ok(root.path(), &["clone", "-q", origin.to_str().unwrap(), work.to_str().unwrap()]);
    git_ok(&work, &["switch", "-q", "-c", "task1"]);
    commit(&work, "one.txt", "one\n");
    git_ok(&work, &["push", "-q", "-u", "origin", "task1"]);
    let merged_tip = head(&work);

    // GitHub deletes the branch on origin; the clone's tracking ref stays at the merged tip.
    git_ok(&origin, &["update-ref", "-d", "refs/heads/task1"]);
    let tracking = git::git_rev_parse(&work, "refs/remotes/origin/task1").await.expect("the tracking ref survives");
    assert_eq!(tracking, merged_tip);

    // A rebase rewrote the branch, as the landing's round before the push does.
    git_ok(
      &work,
      &[
        "-c",
        "user.name=Synthetic",
        "-c",
        "user.email=synthetic@example.test",
        "commit",
        "-q",
        "--amend",
        "-m",
        "rewritten",
      ],
    );
    let rewritten = head(&work);
    assert_ne!(rewritten, merged_tip);

    push_task_branch(&landing_ctx(&work, "task1"), "task1").await.expect("the absent branch publishes as a new one");
    let published = git::git_rev_parse(&origin, "refs/heads/task1").await.expect("origin holds the new branch");
    assert_eq!(published, rewritten);
  }

  /// With the branch still on origin, the push leases against the tip it fetched. A second landing that
  /// pushes between that fetch and this push's update transaction therefore keeps its tip, and this push
  /// refuses instead of overwriting it.
  #[tokio::test]
  #[ignore = "integration"]
  async fn a_branch_moved_on_origin_after_the_fetch_still_refuses_the_push() {
    let root = tempfile::tempdir().expect("temporary directory is available");
    let repo = synthetic_repo(root.path(), "task1");
    let origin = root.path().join("origin.git");
    commit(&repo, "one.txt", "one\n");
    git_ok(&repo, &["push", "-q", "-u", "origin", "task1"]);

    // A second landing clone holds its own commit for the same branch.
    let other = root.path().join("other");
    git_ok(root.path(), &["clone", "-q", origin.to_str().unwrap(), other.to_str().unwrap()]);
    git_ok(&other, &["switch", "-q", "-c", "task1", "origin/task1"]);
    commit(&other, "two.txt", "two\n");
    let other_tip = head(&other);

    // The race: the other push lands after this push advertised the remote refs and before its update
    // transaction, so the fetched tip the lease names is no longer the remote tip.
    let hook = repo.join(".git/hooks/pre-push");
    fs::write(
      &hook,
      format!("#!/bin/sh\nunset GIT_DIR GIT_WORK_TREE\ngit -C '{}' push -q origin task1\n", other.display()),
    )
    .expect("the pre-push hook is written");
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).expect("the hook is executable");

    git_ok(&repo, &["commit", "-q", "--amend", "-m", "rewritten"]);
    let error = push_task_branch(&landing_ctx(&repo, "task1"), "task1").await.expect_err("the other push stands");
    let text = format!("{error:#}");
    assert!(text.contains("cannot lock ref") && text.contains("but expected"), "{text}");
    assert_eq!(git::git_rev_parse(&origin, "refs/heads/task1").await.expect("origin holds the branch"), other_tip);
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_credential_value_in_the_diff_refuses_the_landing() {
    const SECRET: &str = "fake-landing-diff-value-0001";
    let root = tempfile::tempdir().expect("temporary directory is available");
    let home = planted_home(root.path(), SECRET);
    let repo = synthetic_repo(root.path(), "feature");
    commit(&repo, "notes.txt", &format!("the key is {SECRET}\n"));
    fetch_main(&repo).await;
    let error =
      outbound_checks(&home, &repo, &repo, "feature", "origin/main", None).await.expect_err("the diff holds the value");
    let text = format!("{error:#}");
    assert!(text.contains("credentials.yaml leaf planted.api_key"), "the kind is named: {text}");
    assert!(text.contains("commit ") && text.contains(" diff"), "the field is named: {text}");
    assert!(!text.contains(SECRET), "the value never appears: {text}");
  }

  /// The literal check reads the files the branch changes. A literal that main already holds is not a site of this
  /// landing; one that the branch adds is, and the refusal names its file and line.
  #[tokio::test]
  #[ignore = "integration"]
  async fn a_backend_literal_the_branch_adds_refuses_the_landing_and_one_main_holds_does_not() {
    let root = tempfile::tempdir().expect("temporary directory is available");
    let home = root.path().join("home");
    let repo = synthetic_repo(root.path(), "main");
    fs::create_dir(repo.join("scripts")).expect("the scripts directory is created");
    commit(&repo, "scripts/old.sh", "model=claude-opus-5-5\n");
    assert!(
      Command::new("git").arg("-C").arg(&repo).args(["push", "-q", "origin", "main"]).status().unwrap().success()
    );
    assert!(
      Command::new("git").arg("-C").arg(&repo).args(["switch", "-q", "-c", "feature"]).status().unwrap().success()
    );
    commit(&repo, "notes.txt", "clean\n");
    fetch_main(&repo).await;
    backend_literal_check(&Registry::new(), &home, &repo, "origin/main").await.expect("the branch adds no literal");

    commit(&repo, "scripts/new.sh", "model=claude-opus-5-5\n");
    let error = backend_literal_check(&Registry::new(), &home, &repo, "origin/main")
      .await
      .expect_err("the branch adds a literal");
    let text = format!("{error:#}");
    assert!(text.contains("scripts/new.sh:1") && text.contains("claude-opus-5-5"), "{text}");
    assert!(!text.contains("old.sh"), "{text}");
  }

  /// A task branch on an older feature base, `origin/older`, that main has since passed. Main holds a model id in
  /// `scripts/old.sh`; the task branch merged main into its own work, so that file reaches the branch with main's line.
  fn merged_older_feature(root: &Path) -> std::path::PathBuf {
    let repo = synthetic_repo(root, "main");
    git_ok(&repo, &["switch", "-q", "-c", "older"]);
    commit(&repo, "older.txt", "older\n");
    git_ok(&repo, &["push", "-q", "-u", "origin", "older"]);
    git_ok(&repo, &["switch", "-q", "main"]);
    fs::create_dir(repo.join("scripts")).expect("the scripts directory is created");
    commit(&repo, "scripts/old.sh", "model=claude-opus-5-5\n");
    git_ok(&repo, &["push", "-q", "origin", "main"]);
    git_ok(&repo, &["switch", "-q", "-c", "feature", "origin/older"]);
    git_ok(&repo, &["merge", "-q", "--no-edit", "origin/main"]);
    commit(&repo, "notes.txt", "clean\n");
    repo
  }

  /// The literal main holds reaches the branch only through the merge, and the branch leaves that file as main has it:
  /// the literal is no site of this landing, so the clean branch passes.
  #[tokio::test]
  #[ignore = "integration"]
  async fn a_literal_the_default_branch_holds_is_no_site_of_a_merged_feature_landing() {
    let root = tempfile::tempdir().expect("temporary directory is available");
    let repo = merged_older_feature(root.path());
    backend_literal_check(&Registry::new(), &root.path().join("home"), &repo, "origin/older")
      .await
      .expect("main's literal is not a site of this landing");
  }

  /// A literal that the branch adds on a new line is refused on that line, and main's literal is not named.
  #[tokio::test]
  #[ignore = "integration"]
  async fn a_literal_the_branch_adds_on_a_merged_feature_landing_is_refused_on_its_line() {
    let root = tempfile::tempdir().expect("temporary directory is available");
    let repo = merged_older_feature(root.path());
    commit(&repo, "scripts/new run.sh", "echo ok\nmodel=claude-opus-5-5\n");
    let error = backend_literal_check(&Registry::new(), &root.path().join("home"), &repo, "origin/older")
      .await
      .expect_err("the branch adds a literal");
    let text = format!("{error:#}");
    assert!(text.contains("new run.sh:2") && text.contains("claude-opus-5-5"), "{text}");
    assert!(!text.contains("old.sh"), "{text}");
  }

  /// The branch edits main's file without touching main's literal line, which passes. A second literal in that file is
  /// refused on its own line only.
  #[tokio::test]
  #[ignore = "integration"]
  async fn a_second_literal_in_a_file_main_holds_is_refused_on_its_own_line() {
    let root = tempfile::tempdir().expect("temporary directory is available");
    let repo = merged_older_feature(root.path());
    let home = root.path().join("home");
    commit(&repo, "scripts/old.sh", "model=claude-opus-5-5\necho done\n");
    backend_literal_check(&Registry::new(), &home, &repo, "origin/older")
      .await
      .expect("main's literal line is not a site of this landing");

    commit(&repo, "scripts/old.sh", "model=claude-opus-5-5\necho done\nmodel=claude-haiku-5-5\n");
    let error = backend_literal_check(&Registry::new(), &home, &repo, "origin/older")
      .await
      .expect_err("the branch adds a second literal");
    let text = format!("{error:#}");
    assert!(text.contains("old.sh:3") && text.contains("claude-haiku-5-5"), "{text}");
    assert!(!text.contains("old.sh:1"), "{text}");
  }

  /// The comparison base is the remote's default branch, so a remote whose default branch cannot be read fails the
  /// check instead of narrowing it to the task base.
  #[tokio::test]
  #[ignore = "integration"]
  async fn a_backend_literal_check_whose_default_branch_cannot_be_read_fails() {
    let root = tempfile::tempdir().expect("temporary directory is available");
    let repo = synthetic_repo(root.path(), "feature");
    let missing = root.path().join("missing.git");
    git_ok(&repo, &["remote", "set-url", "origin", missing.to_str().expect("the path is UTF-8")]);
    let error = backend_literal_check(&Registry::new(), &root.path().join("home"), &repo, "origin/main")
      .await
      .expect_err("the default branch cannot be read");
    assert!(format!("{error:#}").contains("default branch"), "{error:#}");
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_credential_value_in_a_commit_message_refuses_the_landing() {
    const SECRET: &str = "fake-landing-message-value-0002";
    let root = tempfile::tempdir().expect("temporary directory is available");
    let home = planted_home(root.path(), SECRET);
    let repo = synthetic_repo(root.path(), "feature");
    commit_with_message(&repo, "notes.txt", "clean\n", &format!("notes {SECRET}"));
    fetch_main(&repo).await;
    let error = outbound_checks(&home, &repo, &repo, "feature", "origin/main", None)
      .await
      .expect_err("the message holds the value");
    let text = format!("{error:#}");
    assert!(text.contains("credentials.yaml leaf planted.api_key"), "the kind is named: {text}");
    assert!(text.contains(" message"), "the field is named: {text}");
    assert!(!text.contains(SECRET), "the value never appears: {text}");
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_credential_value_in_the_pr_body_refuses_before_the_pr_is_created() {
    const SECRET: &str = "fake-landing-body-value-0003";
    let root = tempfile::tempdir().expect("temporary directory is available");
    let home = planted_home(root.path(), SECRET);
    let repo = synthetic_repo(root.path(), "feature");
    commit(&repo, "notes.txt", "clean\n");
    fetch_main(&repo).await;
    let text = ResolvedPrText { title: "Clean title".to_string(), body: format!("Evidence with {SECRET}\n") };
    let error = outbound_checks(&home, &repo, &repo, "feature", "origin/main", Some(&text))
      .await
      .expect_err("the body holds the value");
    let message = format!("{error:#}");
    assert!(message.contains("PR body"), "the field is named: {message}");
    assert!(!message.contains(SECRET), "the value never appears: {message}");
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_clean_branch_passes_and_a_value_under_the_minimum_length_passes() {
    // Eleven characters: below the scanner's minimum, so the same value in the diff is not a credential.
    const SHORT: &str = "fake-short1";
    let root = tempfile::tempdir().expect("temporary directory is available");
    let home = planted_home(root.path(), SHORT);
    let repo = synthetic_repo(root.path(), "feature");
    commit(&repo, "notes.txt", &format!("the short value {SHORT} is not a credential\n"));
    fetch_main(&repo).await;
    let text = ResolvedPrText { title: "Clean title".to_string(), body: "Evidence.\n".to_string() };
    outbound_checks(&home, &repo, &repo, "feature", "origin/main", Some(&text)).await.expect("the check passes");
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_pr_body_with_a_session_view_line_passes_the_landing_check() {
    // A fake session id: the landing's check reads only credential values, so the View line of a session page passes.
    const SESSION_ID: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
    let root = tempfile::tempdir().expect("temporary directory is available");
    let home = planted_home(root.path(), "fake-landing-view-value-0006");
    let repo = synthetic_repo(root.path(), "feature");
    commit(&repo, "notes.txt", "clean\n");
    fetch_main(&repo).await;
    let text = ResolvedPrText {
      title: "Clean title".to_string(),
      body: format!("## Evidence\nbefore and after.\n\nView: /?session={SESSION_ID}\n"),
    };
    outbound_checks(&home, &repo, &repo, "feature", "origin/main", Some(&text)).await.expect("the check passes");
  }

  /// A profile home whose credentials file holds both a non-secret leaf and a secret leaf, so each side of the skip
  /// runs against the same file.
  fn home_with_leaves(root: &Path, model: &str, secret: &str) -> PathBuf {
    let home = root.join("home");
    fs::create_dir(&home).expect("the profile home is created");
    fs::write(home.join("credentials.yaml"), format!("gemini:\n  model: {model}\nplanted:\n  api_key: {secret}\n"))
      .expect("the credentials file is written");
    home
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_diff_that_quotes_a_non_secret_leaf_passes_the_credential_check() {
    const MODEL: &str = "fake-model-name-0005";
    let root = tempfile::tempdir().expect("temporary directory is available");
    let home = home_with_leaves(root.path(), MODEL, "fake-landing-secret-0005");
    let repo = synthetic_repo(root.path(), "feature");
    commit(&repo, "notes.txt", &format!("the model is {MODEL}\n"));
    fetch_main(&repo).await;
    outbound_checks(&home, &repo, &repo, "feature", "origin/main", None)
      .await
      .expect("the model name is not a credential");
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_secret_leaf_beside_a_non_secret_leaf_refuses_the_landing() {
    const SECRET: &str = "fake-landing-secret-0006";
    let root = tempfile::tempdir().expect("temporary directory is available");
    let home = home_with_leaves(root.path(), "fake-model-name-0006", SECRET);
    let repo = synthetic_repo(root.path(), "feature");
    commit(&repo, "notes.txt", &format!("the key is {SECRET}\n"));
    fetch_main(&repo).await;
    let error =
      outbound_checks(&home, &repo, &repo, "feature", "origin/main", None).await.expect_err("the secret leaf refuses");
    let text = format!("{error:#}");
    assert!(text.contains("credentials.yaml leaf planted.api_key"), "the kind is named: {text}");
    assert!(!text.contains(SECRET), "the value never appears: {text}");
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_credentials_file_that_cannot_be_parsed_refuses_the_landing() {
    let root = tempfile::tempdir().expect("temporary directory is available");
    let home = root.path().join("home");
    fs::create_dir(&home).expect("the profile home is created");
    fs::write(home.join("credentials.yaml"), "planted:\n  - listed-value-not-readable\n")
      .expect("the credentials file is written");
    let repo = synthetic_repo(root.path(), "feature");
    commit(&repo, "notes.txt", "clean\n");
    fetch_main(&repo).await;
    let error =
      outbound_checks(&home, &repo, &repo, "feature", "origin/main", None).await.expect_err("the file does not parse");
    let text = format!("{error:#}");
    assert!(text.contains("could not read the credential values"), "the refusal is named: {text}");
    assert!(!text.contains("listed-value-not-readable"), "the content never appears: {text}");
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_rebase_conflict_aborts_and_leaves_the_branch_untouched() {
    let root = tempfile::tempdir().expect("temporary directory is available");
    let repo = synthetic_repo(root.path(), "feature");
    commit(&repo, "shared.txt", "feature side\n");
    assert!(
      Command::new("git").arg("-C").arg(&repo).args(["push", "-q", "origin", "feature"]).status().unwrap().success()
    );
    // Main moves onto the same file on origin, so the branch's rebase cannot apply.
    let main_work = root.path().join("main-work");
    let clone = Command::new("git")
      .args(["clone", root.path().join("origin.git").to_str().unwrap(), main_work.to_str().unwrap()])
      .output()
      .expect("git clone runs");
    assert!(clone.status.success(), "git clone failed: {}", String::from_utf8_lossy(&clone.stderr));
    commit(&main_work, "shared.txt", "main side\n");
    assert!(
      Command::new("git").arg("-C").arg(&main_work).args(["push", "-q", "origin", "main"]).status().unwrap().success()
    );

    let before = head(&repo);
    assert!(git::git_stdout(&repo, &["fetch", "origin", "main"], GIT_WRITE_TIMEOUT, "git fetch").await.0);
    let step = rebase_onto_base(&repo, "origin/main").await.expect("the rebase step resolves");
    assert_eq!(step, RebaseStep::Conflict);
    abort_rebase(&repo).await;
    assert_eq!(head(&repo), before);
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn the_gate_reads_the_default_branch_workflows() {
    let root = tempfile::tempdir().expect("temporary directory is available");
    let repo = synthetic_repo(root.path(), "main");
    // No workflows at all: the no-CI repository case.
    assert!(matches!(gate_applicability(&repo, "main").await.unwrap(), GateApplicability::NotApplicable));
    // A workflow without a pull_request trigger stays outside the gate.
    fs::create_dir_all(repo.join(".github/workflows")).expect("the workflows directory is created");
    fs::write(repo.join(".github/workflows/nightly.yml"), "on:\n  schedule:\n    - cron: '0 3 * * *'\njobs: {}\n")
      .expect("the workflow is written");
    assert!(Command::new("git").arg("-C").arg(&repo).args(["add", "."]).status().unwrap().success());
    assert!(
      Command::new("git").arg("-C").arg(&repo).args(["commit", "-q", "-m", "nightly"]).status().unwrap().success()
    );
    assert!(
      Command::new("git").arg("-C").arg(&repo).args(["push", "-q", "origin", "main"]).status().unwrap().success()
    );
    assert!(matches!(gate_applicability(&repo, "main").await.unwrap(), GateApplicability::NotApplicable));
    // A pull_request workflow on the default branch applies the gate; a non-default base does not.
    fs::write(
      repo.join(".github/workflows/ci.yml"),
      "on:\n  pull_request:\n  push:\n    branches:\n      - main\njobs: {}\n",
    )
    .expect("the workflow is written");
    assert!(Command::new("git").arg("-C").arg(&repo).args(["add", "."]).status().unwrap().success());
    assert!(Command::new("git").arg("-C").arg(&repo).args(["commit", "-q", "-m", "ci"]).status().unwrap().success());
    assert!(
      Command::new("git").arg("-C").arg(&repo).args(["push", "-q", "origin", "main"]).status().unwrap().success()
    );
    assert!(matches!(gate_applicability(&repo, "main").await.unwrap(), GateApplicability::Applies));
    assert!(matches!(gate_applicability(&repo, "other").await.unwrap(), GateApplicability::NotApplicable));
  }

  // ---- the declared-scanner checks: the base ref's own tree decides ----

  /// A synthetic repository whose `origin/main` declares `dev/check-skills-host-leak.sh`, checked out on
  /// *branch* for the task's commits.
  fn declared_repo(root: &Path, branch: &str) -> std::path::PathBuf {
    let repo = synthetic_repo(root, "main");
    fs::create_dir_all(repo.join("dev")).expect("the dev directory is created");
    fs::write(repo.join("dev/check-skills-host-leak.sh"), "# the leak scanner declaration\n")
      .expect("the scanner declaration is written");
    assert!(Command::new("git").arg("-C").arg(&repo).args(["add", "dev"]).status().unwrap().success());
    assert!(
      Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["commit", "-q", "-m", "the scanner declaration"])
        .status()
        .unwrap()
        .success()
    );
    assert!(
      Command::new("git").arg("-C").arg(&repo).args(["push", "-q", "origin", "main"]).status().unwrap().success()
    );
    assert!(Command::new("git").arg("-C").arg(&repo).args(["switch", "-q", "-c", branch]).status().unwrap().success());
    repo
  }

  /// A profile home with a credentials leaf and a local leak-patterns file holding `patterns`.
  fn home_with_patterns(root: &Path, secret: &str, patterns: &str) -> PathBuf {
    let home = planted_home(root, secret);
    fs::write(home.join("skills_leak_patterns.local.txt"), patterns).expect("the pattern file is written");
    home
  }

  /// Commits a `*.gz` file whose decompressed content is `content`.
  fn commit_gz(repo: &Path, file: &str, content: &[u8]) {
    use std::io::Write as _;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(content).expect("the content compresses");
    let bytes = encoder.finish().expect("the gzip stream finishes");
    fs::write(repo.join(file), bytes).expect("the gz file is written");
    assert!(Command::new("git").arg("-C").arg(repo).args(["add", "."]).status().unwrap().success());
    assert!(
      Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "user.name=Synthetic", "-c", "user.email=synthetic@example.test"])
        .args(["commit", "-q", "-m", file])
        .status()
        .unwrap()
        .success()
    );
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_local_pattern_in_the_diff_refuses_a_declared_landing() {
    let root = tempfile::tempdir().expect("temporary directory is available");
    let home = home_with_patterns(root.path(), "fake-landing-secret-0010", "fictionalmarker\n");
    let repo = declared_repo(root.path(), "feature");
    commit(&repo, "notes.txt", "the notes carry fictionalmarker\n");
    fetch_main(&repo).await;
    let error = outbound_checks(&home, &repo, &repo, "feature", "origin/main", None)
      .await
      .expect_err("the diff holds the pattern");
    let text = format!("{error:#}");
    assert!(text.contains("Leak hit in commit") && text.contains(" diff"), "the field is named: {text}");
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_local_pattern_in_a_commit_message_refuses_a_declared_landing() {
    let root = tempfile::tempdir().expect("temporary directory is available");
    let home = home_with_patterns(root.path(), "fake-landing-secret-0011", "fictionalmarker\n");
    let repo = declared_repo(root.path(), "feature");
    commit_with_message(&repo, "notes.txt", "clean\n", "notes on fictionalmarker");
    fetch_main(&repo).await;
    let error = outbound_checks(&home, &repo, &repo, "feature", "origin/main", None)
      .await
      .expect_err("the message holds the pattern");
    assert!(format!("{error:#}").contains("message"), "{}", format!("{error:#}"));
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_local_pattern_in_a_gz_blob_refuses_a_declared_landing() {
    let root = tempfile::tempdir().expect("temporary directory is available");
    let home = home_with_patterns(root.path(), "fake-landing-secret-0012", "fictionalmarker\n");
    let repo = declared_repo(root.path(), "feature");
    commit_gz(&repo, "fixture.gz", b"compressed fictionalmarker content\n");
    fetch_main(&repo).await;
    let error = outbound_checks(&home, &repo, &repo, "feature", "origin/main", None)
      .await
      .expect_err("the blob holds the pattern");
    assert!(format!("{error:#}").contains("fixture.gz"), "{}", format!("{error:#}"));
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_local_pattern_in_the_branch_name_refuses_a_declared_landing() {
    let root = tempfile::tempdir().expect("temporary directory is available");
    let home = home_with_patterns(root.path(), "fake-landing-secret-0013", "fictionalmarker\n");
    let repo = declared_repo(root.path(), "feature-fictionalmarker");
    commit(&repo, "notes.txt", "clean\n");
    fetch_main(&repo).await;
    let error = outbound_checks(&home, &repo, &repo, "feature-fictionalmarker", "origin/main", None)
      .await
      .expect_err("the branch name holds the pattern");
    assert!(format!("{error:#}").contains("branch name"), "{}", format!("{error:#}"));
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_local_pattern_in_the_pr_text_refuses_a_declared_landing() {
    let root = tempfile::tempdir().expect("temporary directory is available");
    let home = home_with_patterns(root.path(), "fake-landing-secret-0014", "fictionalmarker\n");
    let repo = declared_repo(root.path(), "feature");
    commit(&repo, "notes.txt", "clean\n");
    fetch_main(&repo).await;
    let text = ResolvedPrText { title: "the fictionalmarker title".to_string(), body: "Evidence.\n".to_string() };
    let error = outbound_checks(&home, &repo, &repo, "feature", "origin/main", Some(&text))
      .await
      .expect_err("the PR title holds the pattern");
    assert!(format!("{error:#}").contains("PR title"), "{}", format!("{error:#}"));
    let text = ResolvedPrText { title: "Clean".to_string(), body: "Evidence on fictionalmarker.\n".to_string() };
    let error = outbound_checks(&home, &repo, &repo, "feature", "origin/main", Some(&text))
      .await
      .expect_err("the PR body holds the pattern");
    assert!(format!("{error:#}").contains("PR body"), "{}", format!("{error:#}"));
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_generic_leak_in_a_touched_doc_refuses_a_declared_landing() {
    let root = tempfile::tempdir().expect("temporary directory is available");
    let home = home_with_patterns(root.path(), "fake-landing-secret-0015", "fictionalmarker\n");
    let repo = declared_repo(root.path(), "feature");
    fs::create_dir_all(repo.join("skills/demo")).expect("the skill directory is created");
    commit_with_message(&repo, "skills/demo/notes.md", "see /home/someone/notes\n", "the doc");
    fetch_main(&repo).await;
    let error = outbound_checks(&home, &repo, &repo, "feature", "origin/main", None)
      .await
      .expect_err("the touched doc holds a generic leak");
    assert!(format!("{error:#}").contains("doc skills/demo/notes.md"), "{}", format!("{error:#}"));
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_declared_landing_refuses_a_missing_or_empty_pattern_file() {
    let root = tempfile::tempdir().expect("temporary directory is available");
    // No pattern file at all: the required local patterns cannot be loaded.
    let home = planted_home(root.path(), "fake-landing-secret-0016");
    let repo = declared_repo(root.path(), "feature");
    commit(&repo, "notes.txt", "clean\n");
    fetch_main(&repo).await;
    let error = outbound_checks(&home, &repo, &repo, "feature", "origin/main", None)
      .await
      .expect_err("the missing pattern file refuses");
    assert!(format!("{error:#}").contains("exit 2"), "{}", format!("{error:#}"));

    // A comments-only file holds no pattern: the refusal is the same.
    fs::write(home.join("skills_leak_patterns.local.txt"), "# only comments\n").expect("the pattern file is written");
    let error = outbound_checks(&home, &repo, &repo, "feature", "origin/main", None)
      .await
      .expect_err("the empty pattern file refuses");
    assert!(format!("{error:#}").contains("exit 2"), "{}", format!("{error:#}"));
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_declared_landing_refuses_a_stale_hooks_path() {
    let root = tempfile::tempdir().expect("temporary directory is available");
    let home = home_with_patterns(root.path(), "fake-landing-secret-0017", "fictionalmarker\n");
    let repo = declared_repo(root.path(), "feature");
    commit(&repo, "notes.txt", "clean\n");
    fetch_main(&repo).await;
    // The checkout pins a hooks directory that is gone: every push would skip the hooks.
    assert!(
      Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["config", "core.hooksPath", "/nonexistent/hooks"])
        .status()
        .unwrap()
        .success()
    );
    let error = outbound_checks(&home, &repo, &repo, "feature", "origin/main", None)
      .await
      .expect_err("the stale hooks path refuses");
    assert!(format!("{error:#}").contains("core.hooksPath"), "{}", format!("{error:#}"));
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_clean_declared_landing_passes() {
    let root = tempfile::tempdir().expect("temporary directory is available");
    let home = home_with_patterns(root.path(), "fake-landing-secret-0018", "fictionalmarker\n");
    let repo = declared_repo(root.path(), "feature");
    commit(&repo, "notes.txt", "clean notes\n");
    fetch_main(&repo).await;
    let text = ResolvedPrText {
      title: "Clean title".to_string(),
      body: "Evidence.\n\nView: /diff?session=aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee\n".to_string(),
    };
    outbound_checks(&home, &repo, &repo, "feature", "origin/main", Some(&text))
      .await
      .expect("the declared checks pass");
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn an_undeclared_repo_keeps_only_the_credential_checks() {
    let root = tempfile::tempdir().expect("temporary directory is available");
    // A repository whose base declares no scanner: the local patterns are never required or applied, so the
    // same text that would refuse a declared landing lands here — the private repo's own names stay private.
    let home = planted_home(root.path(), "fake-landing-secret-0019");
    let repo = synthetic_repo(root.path(), "feature");
    commit_with_message(&repo, "notes.txt", "the notes carry fictionalmarker\n", "notes on fictionalmarker");
    fetch_main(&repo).await;
    let text = ResolvedPrText { title: "fictionalmarker title".to_string(), body: "Evidence.\n".to_string() };
    outbound_checks(&home, &repo, &repo, "feature-fictionalmarker", "origin/main", Some(&text))
      .await
      .expect("the undeclared landing skips the pattern checks");
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_corrupt_gz_blob_refuses_the_check() {
    let root = tempfile::tempdir().expect("temporary directory is available");
    let home = home_with_patterns(root.path(), "fake-landing-secret-0020", "fictionalmarker\n");
    let repo = declared_repo(root.path(), "feature");
    fs::write(repo.join("fixture.gz"), b"not a gzip member").expect("the corrupt gz is written");
    assert!(Command::new("git").arg("-C").arg(&repo).args(["add", "."]).status().unwrap().success());
    assert!(
      Command::new("git").arg("-C").arg(&repo).args(["commit", "-q", "-m", "the fixture"]).status().unwrap().success()
    );
    fetch_main(&repo).await;
    let error =
      outbound_checks(&home, &repo, &repo, "feature", "origin/main", None).await.expect_err("the corrupt blob refuses");
    assert!(format!("{error:#}").contains("decompress"), "{}", format!("{error:#}"));
  }
  // -- the landing queue over a real bare origin --

  use std::collections::{HashMap, VecDeque};
  use std::future::Future;
  use std::os::unix::fs::PermissionsExt;
  use std::pin::Pin;
  use std::sync::Mutex;
  use std::sync::atomic::{AtomicU64, Ordering};

  static MANUAL_MOVES: AtomicU64 = AtomicU64::new(0);

  /// A repository whose base branch declares a pull_request workflow, so the CI gate applies, checked out on
  /// *branch*.
  fn gated_repo(root: &Path, branch: &str) -> PathBuf {
    let repo = synthetic_queue_repo(root, "main");
    fs::create_dir_all(repo.join(".github/workflows")).expect("the workflows directory is created");
    fs::write(repo.join(".github/workflows/ci.yml"), "on:\n  pull_request:\njobs: {}\n")
      .expect("the workflow is written");
    git_ok(&repo, &["add", "."]);
    git_ok(
      &repo,
      &["-c", "user.name=Synthetic", "-c", "user.email=synthetic@example.test", "commit", "-q", "-m", "workflow"],
    );
    git_ok(&repo, &["push", "-q", "origin", "main"]);
    if branch != "main" {
      git_ok(&repo, &["switch", "-q", "-c", branch]);
    }
    repo
  }

  /// One task's clone of the synthetic origin, on its own branch, with one commit pushed.
  fn task_clone(root: &Path, name: &str) -> PathBuf {
    let origin = root.join("origin.git");
    let work = root.join(name);
    git_ok(
      root,
      &[
        "clone",
        "-q",
        origin.to_str().expect("the origin path is UTF-8"),
        work.to_str().expect("the work path is UTF-8"),
      ],
    );
    git_ok(&work, &["switch", "-q", "-c", name]);
    // The landing rebases in this clone, and a host without a global git identity has no committer for it.
    git_ok(&work, &["config", "user.name", "Synthetic"]);
    git_ok(&work, &["config", "user.email", "synthetic@example.test"]);
    commit(&work, &format!("{name}.txt"), &format!("{name}\n"));
    git_ok(&work, &["push", "-q", "-u", "origin", name]);
    work
  }

  /// The landing context of a task clone, with its own session directory.
  fn venue(worktree: &Path, branch: &str, session_id: &str) -> TaskLanding {
    let mut ctx = landing_ctx(worktree, branch);
    ctx.session_id = session_id.to_string();
    fs::create_dir_all(&ctx.session_dir).expect("the session directory is created");
    ctx
  }

  /// The waiting record `land` wrote before it registered the queued command.
  fn queue_record(pr: u64, sha: &str) -> LandingRecord {
    LandingRecord {
      state: STATE_WAITING.to_string(),
      repo: "synthetic/synthetic".to_string(),
      base: "main".to_string(),
      sha: sha.to_string(),
      pr_url: Some(format!("https://example.test/owner/repo/pull/{pr}")),
      checks: Vec::new(),
      head_pushed_at: None,
      ci_passed_at: None,
      pushed_at: None,
      rebase_retries: 0,
      queue_ref: None,
    }
  }

  async fn git_async(repo: &Path, args: &[&str]) -> anyhow::Result<String> {
    let (ok, out, err) = git::git_stdout(repo, args, GIT_READ_TIMEOUT, "git").await;
    if !ok {
      anyhow::bail!("git {args:?} failed: {err}");
    }
    Ok(out.trim().to_string())
  }

  /// Pushes one empty commit onto origin's main from *work* and returns its sha: the outside write that a landing's
  /// leased main push must notice.
  async fn push_main_commit(work: &Path) -> anyhow::Result<String> {
    git_async(work, &["fetch", "-q", "origin", "main"]).await?;
    let tree = git_async(work, &["rev-parse", "origin/main^{tree}"]).await?;
    let message = format!("manual main move {}", MANUAL_MOVES.fetch_add(1, Ordering::Relaxed) + 1);
    let commit = git_async(
      work,
      &[
        "-c",
        "user.name=Synthetic",
        "-c",
        "user.email=synthetic@example.test",
        "commit-tree",
        &tree,
        "-p",
        "origin/main",
        "-m",
        &message,
      ],
    )
    .await?;
    git_async(work, &["push", "-q", "origin", &format!("{commit}:refs/heads/main")]).await?;
    Ok(commit)
  }

  /// A scripted GitHub for a queued landing: each head sha's CI reads answer from its own queue (a sha with no queue
  /// passes), every read is recorded, and the remote can move main before it answers.
  struct QueueRemote {
    answers: Mutex<HashMap<String, VecDeque<CiVerdict>>>,
    passed_at: Mutex<HashMap<String, String>>,
    reads: Mutex<HashMap<String, usize>>,
    seen: Mutex<Vec<(String, CiVerdict)>>,
    moved: Mutex<Vec<String>>,
    work: PathBuf,
    default_passed_at: String,
    /// Moves main before answering a read of this sha (every sha when empty) at this read count (every read when 0).
    move_main: Option<(String, usize)>,
  }

  impl QueueRemote {
    fn new(work: &Path, passed: &[(&str, &str)], default_passed_at: &str) -> QueueRemote {
      let mut times = HashMap::new();
      for (sha, at) in passed {
        times.insert((*sha).to_string(), (*at).to_string());
      }
      QueueRemote {
        answers: Mutex::new(HashMap::new()),
        passed_at: Mutex::new(times),
        reads: Mutex::new(HashMap::new()),
        seen: Mutex::new(Vec::new()),
        moved: Mutex::new(Vec::new()),
        work: work.to_path_buf(),
        default_passed_at: default_passed_at.to_string(),
        move_main: None,
      }
    }

    fn answer(&self, sha: &str, verdicts: Vec<CiVerdict>) {
      self.answers.lock().expect("the answers lock").insert(sha.to_string(), verdicts.into());
    }

    fn moves_main_at(&mut self, sha: &str, read: usize) {
      self.move_main = Some((sha.to_string(), read));
    }

    fn passed_reads(&self, sha: &str) -> usize {
      self
        .seen
        .lock()
        .expect("the seen lock")
        .iter()
        .filter(|(seen, verdict)| seen == sha && *verdict == CiVerdict::Pass)
        .count()
    }
  }

  impl LandRemote for QueueRemote {
    async fn ci_snapshot(
      &self,
      _ctx: &TaskLanding,
      sha: &str,
      _head_pushed_at: Option<&str>,
    ) -> anyhow::Result<CiSnapshot> {
      let verdict = self
        .answers
        .lock()
        .expect("the answers lock")
        .get_mut(sha)
        .and_then(|answers| answers.pop_front())
        .unwrap_or(CiVerdict::Pass);
      self.seen.lock().expect("the seen lock").push((sha.to_string(), verdict.clone()));
      let count = {
        let mut reads = self.reads.lock().expect("the reads lock");
        let count = reads.entry(sha.to_string()).or_insert(0);
        *count += 1;
        *count
      };
      let move_main =
        self.move_main.as_ref().is_some_and(|(who, at)| (who.is_empty() || who == sha) && (*at == 0 || *at == count));
      let passed_at = self
        .passed_at
        .lock()
        .expect("the times lock")
        .get(sha)
        .cloned()
        .unwrap_or_else(|| self.default_passed_at.clone());
      if move_main {
        let moved = push_main_commit(&self.work).await?;
        self.moved.lock().expect("the moved lock").push(moved);
      }
      Ok(CiSnapshot { verdict, checks: Vec::new(), ci_passed_at: Some(passed_at) })
    }

    async fn close_task_prs(&self, _ctx: &TaskLanding, _published: &str, _sha: &str) -> anyhow::Result<()> {
      Ok(())
    }
  }

  /// An owner whose wake program is a stub: it records its arguments and, for a trigger registration, writes the
  /// pending trigger file that the queue's dedupe reads; a cancel removes that file.
  fn stub_owner(root: &Path, worktree: &Path, session_id: &str) -> (land_queue::Owner, PathBuf) {
    let session_dir = root.join("sessions").join(session_id);
    let triggers = layout::triggers_dir(&session_dir);
    fs::create_dir_all(&triggers).expect("the triggers directory is created");
    let log = root.join(format!("{session_id}-program.log"));
    let program = root.join(format!("{session_id}-program.sh"));
    let script = format!(
      r#"#!/bin/sh
printf '%s\n' "$*" >> "{log}"
case " $* " in
  *" session trigger "*)
    message=
    previous=
    for argument in "$@"; do
      if [ "$previous" = "--message" ]; then message=$argument; fi
      previous=$argument
    done
    printf '{{"id":"wake-%s","message":"%s","status":"pending"}}\n' "$$" "$message" > "{triggers}/wake-$$.json"
    ;;
  *" trigger cancel "*)
    id=
    for argument in "$@"; do id=$argument; done
    rm -f "{triggers}/"*"$id"*
    ;;
esac
exit 0
"#,
      log = log.display(),
      triggers = triggers.display()
    );
    fs::write(&program, script).expect("the stub program is written");
    let mut permissions = fs::metadata(&program).expect("the stub program exists").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&program, permissions).expect("the stub program is executable");
    let owner =
      land_queue::Owner { session_id: session_id.to_string(), session_dir, worktree: worktree.to_path_buf(), program };
    (owner, log)
  }

  /// Drives a queued landing until the queue shows its turn held, so the test can start the clients that must wait
  /// behind it; the run itself stays unfinished.
  async fn wait_for_turn<F>(site: &land_queue::Site, run: &mut Pin<Box<F>>)
  where
    F: Future<Output = anyhow::Result<QueueEnd>>,
  {
    tokio::time::timeout(Duration::from_secs(30), async {
      loop {
        if site.read().await.expect("the queue reads").turn.is_some() {
          return;
        }
        tokio::select! {
          result = run.as_mut() => panic!(
            "the landing ended before it held the turn: {:?}",
            result.err().map(|error| format!("{error:#}"))
          ),
          _ = tokio::time::sleep(Duration::from_millis(5)) => {}
        }
      }
    })
    .await
    .expect("the landing took the turn within 30 s");
  }

  fn landed_record(end: anyhow::Result<QueueEnd>, who: &str) -> LandingRecord {
    match end.expect("the landing ends without an error") {
      QueueEnd::Record(record) => *record,
      QueueEnd::Superseded => panic!("{who} was superseded"),
    }
  }

  /// The subjects of origin's main, oldest first, as the test clones see them.
  async fn main_subjects(repo: &Path) -> Vec<String> {
    assert!(git::git_stdout(repo, &["fetch", "-q", "origin", "main"], GIT_WRITE_TIMEOUT, "git fetch").await.0);
    git_async(repo, &["log", "--reverse", "--format=%s", "origin/main"])
      .await
      .expect("the log reads")
      .lines()
      .map(str::to_string)
      .collect()
  }

  /// The merge command of a queued pull request merge: a squash-like empty commit on main.
  fn merge_command(work: &Path, pr: u64) -> String {
    let dir = work.display();
    format!(
      "git -C {dir} fetch -q origin main && git -C {dir} checkout -q -B landqueue origin/main && \
       git -C {dir} -c user.name=Synthetic -c user.email=synthetic@example.test commit -q --allow-empty -m 'merged PR {pr}' && \
       git -C {dir} push -q origin landqueue:main"
    )
  }

  const CADENCE: Cadence = Cadence { poll: Duration::from_millis(5), beat: Duration::from_millis(30) };
  const SESSION_A: &str = "00000000-0000-4000-8000-000000000001";
  const SESSION_B: &str = "00000000-0000-4000-8000-000000000002";
  const SESSION_C: &str = "00000000-0000-4000-8000-000000000003";
  const SESSION_MERGE: &str = "00000000-0000-4000-8000-000000000004";
  const SESSION_E: &str = "00000000-0000-4000-8000-000000000005";
  const SESSION_F: &str = "00000000-0000-4000-8000-000000000006";
  const SESSION_DIRECT: &str = "00000000-0000-4000-8000-000000000007";
  const SESSION_QUEUED: &str = "00000000-0000-4000-8000-000000000008";
  const SESSION_H: &str = "00000000-0000-4000-8000-000000000009";
  const SESSION_REPLACEMENT: &str = "00000000-0000-4000-8000-000000000010";
  const SESSION_REFUSED: &str = "00000000-0000-4000-8000-000000000011";

  #[tokio::test]
  #[ignore = "integration"]
  async fn landings_and_a_merge_reach_main_in_ci_pass_order() {
    let root = tempfile::tempdir().expect("temporary directory is available");
    gated_repo(root.path(), "main");
    let a_work = task_clone(root.path(), "task-a");
    let b_work = task_clone(root.path(), "task-b");
    let c_work = task_clone(root.path(), "task-c");
    let merge_work = task_clone(root.path(), "merge-work");
    let remote_work = task_clone(root.path(), "remote-work");
    let home = root.path().join("home");
    fs::create_dir_all(&home).expect("the home is created");
    let a = venue(&a_work, "task-a", SESSION_A);
    let b = venue(&b_work, "task-b", SESSION_B);
    let c = venue(&c_work, "task-c", SESSION_C);
    let (sha_a, sha_b, sha_c) = (head(&a_work), head(&b_work), head(&c_work));
    for (ctx, pr, sha) in [(&a, 101, &sha_a), (&b, 102, &sha_b), (&c, 103, &sha_c)] {
      write_landing_record(&ctx.session_dir, &queue_record(pr, sha)).expect("the landing record is written");
    }
    let remote = QueueRemote::new(
      &remote_work,
      &[(&sha_a, "2026-11-27T00:00:01Z"), (&sha_b, "2026-11-27T00:00:02Z"), (&sha_c, "2026-11-27T00:00:05Z")],
      "2026-11-27T00:00:09Z",
    );
    // The first landing holds the turn while its CI is still running, which gives the other clients time to queue.
    remote.answer(&sha_a, vec![CiVerdict::Pass, CiVerdict::Waiting, CiVerdict::Pass]);
    let registry = Registry::new();
    let merge_site = land_queue::Site { worktree: merge_work.clone() };
    let (owner, _log) = stub_owner(root.path(), &merge_work, SESSION_MERGE);
    let merge_head = head(&merge_work);
    let merge_cmd = merge_command(&merge_work, 104);
    let merge_key = rfc3339_millis("2026-11-27T00:00:03Z").expect("the merge pass time parses");

    let mut a_run = Box::pin(queued_landing(&a, &registry, &home, &sha_a, &remote, CADENCE, Duration::from_secs(60)));
    wait_for_turn(&land_queue::Site { worktree: a_work.clone() }, &mut a_run).await;
    let gate = tokio::time::timeout(
      Duration::from_secs(10),
      land_queue::pr_merge_gate(
        &merge_site,
        Some(&owner),
        104,
        &merge_head,
        Some(&merge_cmd),
        None,
        CADENCE,
        merge_key,
      ),
    )
    .await
    .expect("the merge gate answers")
    .expect("the merge gate reads the queue");
    assert_eq!(gate, land_queue::MergeGate::Queued, "the held turn queues the merge");
    let b_run = Box::pin(queued_landing(&b, &registry, &home, &sha_b, &remote, CADENCE, Duration::from_secs(60)));
    let c_run = Box::pin(queued_landing(&c, &registry, &home, &sha_c, &remote, CADENCE, Duration::from_secs(60)));
    let merge_run = Box::pin(land_queue::run_pr_merge_queued(&merge_site, &owner, 104, &merge_cmd, CADENCE));

    let (a_end, b_end, c_end, merge_end) = tokio::join!(a_run, b_run, c_run, merge_run);
    let (a_record, b_record, c_record) =
      (landed_record(a_end, "A"), landed_record(b_end, "B"), landed_record(c_end, "C"));
    assert_eq!(merge_end.expect("the queued merge ends without an error"), 0);
    assert_eq!(a_record.state, STATE_LANDED);
    assert_eq!(b_record.state, STATE_LANDED);
    assert_eq!(c_record.state, STATE_LANDED);

    // The merge key sits between B's and C's, so main takes exactly the queue's order.
    assert_eq!(
      main_subjects(&a_work).await,
      vec![
        "base".to_string(),
        "workflow".to_string(),
        "task-a.txt".to_string(),
        "task-b.txt".to_string(),
        "merged PR 104".to_string(),
        "task-c.txt".to_string()
      ]
    );
    // Each landed commit passed its own CI read on itself.
    for record in [&a_record, &b_record, &c_record] {
      assert!(remote.passed_reads(&record.sha) > 0, "{} never passed a CI read", record.sha);
    }
    let view = land_queue::Site { worktree: a_work.clone() }.read().await.expect("the queue reads");
    assert!(view.turn.is_none() && view.entries.is_empty(), "the queue is empty after the landings");
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn an_earlier_ci_pass_written_later_goes_first_without_interrupting_the_holder() {
    let root = tempfile::tempdir().expect("temporary directory is available");
    gated_repo(root.path(), "main");
    let a_work = task_clone(root.path(), "task-a");
    let b_work = task_clone(root.path(), "task-b");
    let c_work = task_clone(root.path(), "task-c");
    let remote_work = task_clone(root.path(), "remote-work");
    let home = root.path().join("home");
    fs::create_dir_all(&home).expect("the home is created");
    let a = venue(&a_work, "task-a", SESSION_A);
    let b = venue(&b_work, "task-b", SESSION_B);
    let c = venue(&c_work, "task-c", SESSION_C);
    let (sha_a, sha_b, sha_c) = (head(&a_work), head(&b_work), head(&c_work));
    for (ctx, pr, sha) in [(&a, 201, &sha_a), (&b, 202, &sha_b), (&c, 203, &sha_c)] {
      write_landing_record(&ctx.session_dir, &queue_record(pr, sha)).expect("the landing record is written");
    }
    let remote = QueueRemote::new(
      &remote_work,
      &[(&sha_a, "2026-11-27T00:00:01Z"), (&sha_b, "2026-11-27T00:00:05Z"), (&sha_c, "2026-11-27T00:00:03Z")],
      "2026-11-27T00:00:09Z",
    );
    remote.answer(&sha_a, vec![CiVerdict::Pass, CiVerdict::Waiting, CiVerdict::Pass]);
    let registry = Registry::new();
    let mut a_run = Box::pin(queued_landing(&a, &registry, &home, &sha_a, &remote, CADENCE, Duration::from_secs(60)));
    wait_for_turn(&land_queue::Site { worktree: a_work.clone() }, &mut a_run).await;
    // B writes its entry first; C, whose CI passed earlier, writes second and takes the place ahead of B.
    let b_run = Box::pin(queued_landing(&b, &registry, &home, &sha_b, &remote, CADENCE, Duration::from_secs(60)));
    tokio::time::sleep(Duration::from_millis(20)).await;
    let c_run = Box::pin(queued_landing(&c, &registry, &home, &sha_c, &remote, CADENCE, Duration::from_secs(60)));
    let (a_end, b_end, c_end) = tokio::join!(a_run, b_run, c_run);
    for (end, who) in [(a_end, "A"), (b_end, "B"), (c_end, "C")] {
      assert_eq!(landed_record(end, who).state, STATE_LANDED);
    }
    assert_eq!(
      main_subjects(&a_work).await,
      vec![
        "base".to_string(),
        "workflow".to_string(),
        "task-a.txt".to_string(),
        "task-c.txt".to_string(),
        "task-b.txt".to_string()
      ]
    );
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_repeat_landing_call_registers_nothing_and_a_new_head_replaces_the_entry() {
    let root = tempfile::tempdir().expect("temporary directory is available");
    gated_repo(root.path(), "main");
    let work = task_clone(root.path(), "task");
    let remote_work = task_clone(root.path(), "remote-work");
    let home = root.path().join("home");
    fs::create_dir_all(&home).expect("the home is created");
    let ctx = venue(&work, "task", SESSION_C);
    let sha = head(&work);
    write_landing_record(&ctx.session_dir, &queue_record(301, &sha)).expect("the landing record is written");
    let site = land_queue::Site { worktree: work.clone() };
    let (owner, log) = stub_owner(root.path(), &work, SESSION_C);
    let remote = QueueRemote::new(&remote_work, &[(&sha, "2026-11-27T00:00:01Z")], "2026-11-27T00:00:09Z");
    remote
      .answer(&sha, vec![CiVerdict::Pass, CiVerdict::Waiting, CiVerdict::Waiting, CiVerdict::Waiting, CiVerdict::Pass]);
    assert_eq!(
      land_queue::arm_landing(&site, &owner, 301, &sha).await.expect("the first call registers"),
      land_queue::Armed::Registered
    );
    let registry = Registry::new();
    let mut run = Box::pin(queued_landing(&ctx, &registry, &home, &sha, &remote, CADENCE, Duration::from_secs(60)));
    wait_for_turn(&site, &mut run).await;
    // A repeat call for the same head returns the current state and registers no second command.
    assert_eq!(
      land_queue::arm_landing(&site, &owner, 301, &sha).await.expect("the repeat reads"),
      land_queue::Armed::Existing
    );
    let registered = fs::read_to_string(&log).expect("the program log reads");
    assert_eq!(registered.matches("session trigger").count(), 1, "no second trigger was registered: {registered}");
    // A new head replaces the old entry and registers the new head's command.
    commit(&work, "two.txt", "two\n");
    let new_sha = head(&work);
    assert_eq!(
      land_queue::arm_landing(&site, &owner, 301, &new_sha).await.expect("the new head registers"),
      land_queue::Armed::Registered
    );
    let view = site.read().await.expect("the queue reads");
    let session_c = land_queue::owner_token(SESSION_C).expect("the session id names a token");
    assert!(view.entry_of(301, &session_c).is_none(), "the replaced head's entry was removed");
    assert!(matches!(run.await.expect("the old command ends"), QueueEnd::Superseded), "the moved head supersedes it");
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_failed_holder_ci_releases_the_turn_to_the_next_entry() {
    let root = tempfile::tempdir().expect("temporary directory is available");
    gated_repo(root.path(), "main");
    let a_work = task_clone(root.path(), "task-a");
    let b_work = task_clone(root.path(), "task-b");
    let remote_work = task_clone(root.path(), "remote-work");
    let home = root.path().join("home");
    fs::create_dir_all(&home).expect("the home is created");
    let a = venue(&a_work, "task-a", SESSION_A);
    let b = venue(&b_work, "task-b", SESSION_B);
    let (sha_a, sha_b) = (head(&a_work), head(&b_work));
    for (ctx, pr, sha) in [(&a, 401, &sha_a), (&b, 402, &sha_b)] {
      write_landing_record(&ctx.session_dir, &queue_record(pr, sha)).expect("the landing record is written");
    }
    let remote = QueueRemote::new(
      &remote_work,
      &[(&sha_a, "2026-11-27T00:00:01Z"), (&sha_b, "2026-11-27T00:00:02Z")],
      "2026-11-27T00:00:09Z",
    );
    remote.answer(&sha_a, vec![CiVerdict::Pass, CiVerdict::Failed { failing: Vec::new() }]);
    let registry = Registry::new();
    let mut a_run = Box::pin(queued_landing(&a, &registry, &home, &sha_a, &remote, CADENCE, Duration::from_secs(60)));
    wait_for_turn(&land_queue::Site { worktree: a_work.clone() }, &mut a_run).await;
    let b_run = Box::pin(queued_landing(&b, &registry, &home, &sha_b, &remote, CADENCE, Duration::from_secs(60)));
    let (a_end, b_end) = tokio::join!(a_run, b_run);
    assert_eq!(landed_record(a_end, "A").state, STATE_CI_FAILED);
    assert_eq!(landed_record(b_end, "B").state, STATE_LANDED);
    let view = land_queue::Site { worktree: a_work.clone() }.read().await.expect("the queue reads");
    assert!(view.turn.is_none() && view.entries.is_empty());
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_stale_heartbeat_is_pruned_from_the_entry_and_the_turn() {
    let root = tempfile::tempdir().expect("temporary directory is available");
    let repo = gated_repo(root.path(), "main");
    let parent = head(&repo);
    let stale = |refname: &str| {
      let stamp = format!("@{} +0000", OffsetDateTime::now_utc().unix_timestamp() - 600);
      let output = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args([
          "-c",
          "commit.gpgsign=false",
          "commit-tree",
          "--no-gpg-sign",
          "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
        ])
        .args(["-p", &parent, "-m", refname])
        .env("GIT_AUTHOR_NAME", "charliebot land queue")
        .env("GIT_AUTHOR_EMAIL", "land-queue@charliebot.invalid")
        .env("GIT_COMMITTER_NAME", "charliebot land queue")
        .env("GIT_COMMITTER_EMAIL", "land-queue@charliebot.invalid")
        .env("GIT_AUTHOR_DATE", &stamp)
        .env("GIT_COMMITTER_DATE", &stamp)
        .output()
        .expect("git commit-tree runs");
      assert!(output.status.success(), "commit-tree failed: {}", String::from_utf8_lossy(&output.stderr));
      let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
      git_ok(&repo, &["push", "-q", "--force", "origin", &format!("{sha}:{refname}")]);
    };
    let session_e = land_queue::owner_token(SESSION_E).expect("the session id names a token");
    let entry_e = land_queue::EntryName::new(1_795_000_000_000, 7, &session_e);
    stale(&entry_e.refname());
    stale(land_queue::TURN_REF);
    let site = land_queue::Site { worktree: repo.clone() };
    let view = site.read().await.expect("the queue reads and prunes");
    assert!(view.turn.is_none(), "the stale turn is pruned");
    assert!(view.entries.is_empty(), "the stale entry is pruned");
    let left = git_async(&repo, &["ls-remote", "origin", land_queue::TURN_REF, "refs/land-queue/*"])
      .await
      .expect("ls-remote runs");
    assert!(left.is_empty(), "origin holds no stale queue refs: {left}");
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_main_push_from_outside_starts_another_rebase_round() {
    let root = tempfile::tempdir().expect("temporary directory is available");
    gated_repo(root.path(), "main");
    let work = task_clone(root.path(), "task");
    let remote_work = task_clone(root.path(), "remote-work");
    let home = root.path().join("home");
    fs::create_dir_all(&home).expect("the home is created");
    let ctx = venue(&work, "task", SESSION_F);
    let sha = head(&work);
    write_landing_record(&ctx.session_dir, &queue_record(501, &sha)).expect("the landing record is written");
    let mut remote = QueueRemote::new(&remote_work, &[(&sha, "2026-11-27T00:00:01Z")], "2026-11-27T00:00:09Z");
    // The turn holder's first CI read is answered after main moves from outside.
    remote.moves_main_at(&sha, 2);
    let end = queued_landing(&ctx, &Registry::new(), &home, &sha, &remote, CADENCE, Duration::from_secs(60))
      .await
      .expect("the landing recovers");
    let record = landed_record(Ok(end), "A");
    assert_eq!(record.state, STATE_LANDED);
    let moved = remote.moved.lock().expect("the moved lock").clone();
    assert_eq!(moved.len(), 1, "one outside push moved main");
    let subjects = main_subjects(&work).await;
    assert!(subjects.contains(&"task.txt".to_string()), "{subjects:?}");
    let tip = git_async(&work, &["rev-parse", "origin/main"]).await.expect("the tip reads");
    assert_eq!(tip, record.sha, "the landed commit is main's tip");
    let ancestry = Command::new("git")
      .arg("-C")
      .arg(&work)
      .args(["merge-base", "--is-ancestor", &moved[0], &record.sha])
      .status()
      .expect("git runs");
    assert!(ancestry.success(), "the outside commit stays under the landing");
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_pr_merge_takes_the_turn_directly_and_completes_from_the_queue() {
    let root = tempfile::tempdir().expect("temporary directory is available");
    gated_repo(root.path(), "main");
    let work = task_clone(root.path(), "task");
    let site = land_queue::Site { worktree: work.clone() };
    let head = head(&work);
    let direct_marker = root.path().join("direct-marker");
    let direct_cmd = format!("printf '%s' \"$CHARLIEBOT_LAND_TURN\" > {}", direct_marker.display());
    let (direct_owner, _log) = stub_owner(root.path(), &work, SESSION_DIRECT);
    let gate = land_queue::pr_merge_gate(&site, Some(&direct_owner), 601, &head, Some(&direct_cmd), None, CADENCE, 1)
      .await
      .expect("the gate answers");
    assert_eq!(gate, land_queue::MergeGate::Ran(0));
    assert_eq!(fs::read_to_string(&direct_marker).expect("the marker reads"), "direct pr 601");
    assert!(site.read().await.expect("the queue reads").turn.is_none(), "the direct merge released the turn");

    // With the turn held, the same call queues and its wake completes the merge.
    let mut holder = land_queue::Claim::direct(&site, 602, &head);
    assert_eq!(holder.take_turn().await.expect("the holder takes the turn"), land_queue::Take::Taken);
    let queued_marker = root.path().join("queued-marker");
    let queued_cmd = format!("printf '%s' \"$CHARLIEBOT_LAND_TURN\" > {}", queued_marker.display());
    let (queued_owner, _log) = stub_owner(root.path(), &work, SESSION_QUEUED);
    let gate = land_queue::pr_merge_gate(&site, Some(&queued_owner), 601, &head, Some(&queued_cmd), None, CADENCE, 5)
      .await
      .expect("the gate answers");
    assert_eq!(gate, land_queue::MergeGate::Queued);
    let view = site.read().await.expect("the queue reads");
    let queued_key = land_queue::owner_token(SESSION_QUEUED).expect("the session id names a token");
    let (entry, _) = view.entry_of(601, &queued_key).expect("the queued merge entered the queue").clone();
    let mut runner = Box::pin(land_queue::run_pr_merge_queued(&site, &queued_owner, 601, &queued_cmd, CADENCE));
    tokio::select! {
      result = &mut runner => panic!("the merge ran before the turn was free: {:?}", result.err().map(|error| format!("{error:#}"))),
      _ = tokio::time::sleep(Duration::from_millis(50)) => {}
    }
    holder.release().await.expect("the holder releases");
    assert_eq!(runner.await.expect("the queued merge ends"), 0);
    assert_eq!(fs::read_to_string(&queued_marker).expect("the marker reads"), entry.refname());
    let view = site.read().await.expect("the queue reads");
    assert!(view.turn.is_none() && view.entries.is_empty(), "the queued merge released the turn and its entry");
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_pr_merge_replaces_its_entry_when_the_head_changes() {
    let root = tempfile::tempdir().expect("temporary directory is available");
    gated_repo(root.path(), "main");
    let work = task_clone(root.path(), "task");
    let site = land_queue::Site { worktree: work.clone() };
    let old_head = head(&work);
    let (owner, log) = stub_owner(root.path(), &work, SESSION_REPLACEMENT);
    let key = owner.token().expect("the session id names a token");
    let mut blocker = land_queue::Claim::direct(&site, 602, &old_head);
    assert_eq!(blocker.take_turn().await.expect("the blocker takes the turn"), land_queue::Take::Taken);

    let first_ms = 1_795_000_000_000;
    assert_eq!(
      land_queue::pr_merge_gate(&site, Some(&owner), 601, &old_head, Some("true"), None, CADENCE, first_ms)
        .await
        .expect("the first merge queues"),
      land_queue::MergeGate::Queued
    );
    let first_view = site.read().await.expect("the first entry reads");
    let (first_name, _) = first_view.entry_of(601, &key).expect("the first entry belongs to its owner").clone();

    commit(&work, "second.txt", "second\n");
    let new_head = head(&work);
    assert_eq!(
      land_queue::pr_merge_gate(&site, Some(&owner), 601, &new_head, Some("true"), None, CADENCE, first_ms + 1,)
        .await
        .expect("the changed head replaces the entry"),
      land_queue::MergeGate::Queued
    );
    let view = site.read().await.expect("the replacement entry reads");
    let (name, beat) = view.entry_of(601, &key).expect("the replacement entry belongs to its owner");
    assert_eq!(name.ci_passed_ms, first_ms + 1, "the replacement uses the new head's CI time");
    assert_eq!(beat.parent, new_head, "the replacement points at the new head");
    assert_ne!(name.refname(), first_name.refname(), "the replacement has a new queue key");
    let program_log = fs::read_to_string(log).expect("the wake log reads");
    assert!(program_log.contains("trigger cancel"), "the old wake was cancelled: {program_log}");

    blocker.release().await.expect("the blocker releases the turn");
    land_queue::release(&site, &key, 601).await.expect("the replacement entry is removed");
    let view = site.read().await.expect("the queue reads after release");
    assert!(view.turn.is_none() && view.entries.is_empty(), "both queue refs were released");
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_refused_release_keeps_its_refs_and_reports_the_failure() {
    let root = tempfile::tempdir().expect("temporary directory is available");
    gated_repo(root.path(), "main");
    let work = task_clone(root.path(), "task");
    let site = land_queue::Site { worktree: work.clone() };
    let sha = head(&work);
    let key = land_queue::owner_token(SESSION_REFUSED).expect("the session id names a token");
    let name = land_queue::EntryName::new(1_795_000_000_000, 701, &key);
    let mut claim = land_queue::Claim::enter(&site, &name, &sha).await.expect("the entry is created");
    assert_eq!(claim.take_turn().await.expect("the claim takes the turn"), land_queue::Take::Taken);

    let patterns = root.path().join("land-queue-leak-scan/patterns.txt");
    let mut local_patterns = fs::read_to_string(&patterns).expect("the local pattern file reads");
    local_patterns.push_str(&format!("{}\n", name.refname()));
    fs::write(&patterns, local_patterns).expect("the holder pattern is added");

    let error = claim.release().await.expect_err("the scanner refuses the release push");
    let message = format!("{error:#}");
    assert!(message.contains("could not release the landing queue refs"), "{message}");
    assert!(message.contains("pre-receive hook declined"), "{message}");
    let view = site.read().await.expect("the queue reads after the refused release");
    assert!(view.turn.is_some() && view.entry_of(701, &key).is_some(), "the refused push leaves both refs in place");
  }

  /// The queue fixture scans every received ref name and newly received commit message. This full cycle covers the
  /// landing, queued merge, direct merge and round paths against one origin.
  #[tokio::test]
  #[ignore = "integration"]
  async fn a_full_queue_cycle_pushes_only_text_the_leak_scan_passes() {
    let root = tempfile::tempdir().expect("temporary directory is available");
    synthetic_queue_repo(root.path(), "main");
    let land = task_clone(root.path(), "task-a");
    let merge = task_clone(root.path(), "merge-work");
    let site_land = land_queue::Site { worktree: land.clone() };
    let site_merge = land_queue::Site { worktree: merge.clone() };
    let base = git_async(&land, &["rev-parse", "origin/main"]).await.expect("origin's main reads");
    let sha_land = head(&land);
    let sha_merge = head(&merge);
    let (owner_land, _) = stub_owner(root.path(), &land, "5e551011-6f0a-4c1d-9b2e-0a1b2c3d4e5f");
    let (owner_merge, _) = stub_owner(root.path(), &merge, "0d0e1e2a-7b3c-4d4e-8f5a-6b7c8d9e0f1a");
    let ms = 1_795_000_000_000;

    // The land path: the entry joins the queue under the session's token, which takes the turn.
    let land_name = land_queue::EntryName::new(ms, 101, &owner_land.token().expect("the session names a token"));
    let mut land_claim =
      land_queue::Claim::enter_or_resume(&site_land, &land_name, &sha_land).await.expect("the land path enqueues");
    assert_eq!(land_claim.take_turn().await.expect("the turn is taken"), land_queue::Take::Taken);

    // The pr-merge path queues behind the held turn, and its wake carries no session id.
    let gate =
      land_queue::pr_merge_gate(&site_merge, Some(&owner_merge), 104, &sha_merge, Some("true"), None, CADENCE, ms + 1)
        .await
        .expect("the merge gate queues");
    assert_eq!(gate, land_queue::MergeGate::Queued, "the held turn queues the merge");

    // The landing beats, publishes main with its turn and entry removed, and the queued merge runs from the turn.
    assert_eq!(land_claim.beat(&sha_land).await.expect("the heartbeat pushes"), land_queue::Pushed::Done);
    let publish = land_claim.publish_changes("refs/heads/main", &base, &sha_land);
    assert_eq!(site_land.push(&publish).await.expect("the publish pushes"), land_queue::Pushed::Done);
    land_claim.landed();
    let merged = land_queue::run_pr_merge_queued(&site_merge, &owner_merge, 104, "true", CADENCE)
      .await
      .expect("the queued merge runs");
    assert_eq!(merged, 0, "the queued merge exits 0");

    // A free queue merges directly, and a round acquires and releases under its launch id.
    let direct =
      land_queue::pr_merge_gate(&site_merge, Some(&owner_merge), 105, &sha_merge, Some("true"), None, CADENCE, 1)
        .await
        .expect("the direct merge runs");
    assert_eq!(direct, land_queue::MergeGate::Ran(0));
    let round = land_queue::acquire(
      &site_merge,
      "20261127T083000Z-12345-23456",
      106,
      &sha_merge,
      ms,
      Duration::from_secs(60),
      CADENCE,
    )
    .await
    .expect("the round acquires the turn");
    assert_eq!(round, land_queue::Acquired::Held);
    land_queue::release(&site_merge, "20261127T083000Z-12345-23456", 106).await.expect("the round releases");

    assert!(site_land.read().await.expect("the queue reads").turn.is_none(), "the cycle released the turn");
    let main = git_async(&land, &["ls-remote", "origin", "refs/heads/main"]).await.expect("main reads");
    assert!(main.starts_with(&sha_land), "the landing published main: {main}");
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn main_that_keeps_moving_past_five_rounds_releases_the_turn() {
    let root = tempfile::tempdir().expect("temporary directory is available");
    gated_repo(root.path(), "main");
    let work = task_clone(root.path(), "task");
    let remote_work = task_clone(root.path(), "remote-work");
    let home = root.path().join("home");
    fs::create_dir_all(&home).expect("the home is created");
    let ctx = venue(&work, "task", SESSION_H);
    let sha = head(&work);
    write_landing_record(&ctx.session_dir, &queue_record(701, &sha)).expect("the landing record is written");
    let mut remote = QueueRemote::new(&remote_work, &[(&sha, "2026-11-27T00:00:01Z")], "2026-11-27T00:00:09Z");
    // Every CI read moves main, so every round's leased main push is refused.
    remote.moves_main_at("", 0);
    let error =
      match queued_landing(&ctx, &Registry::new(), &home, &sha, &remote, CADENCE, Duration::from_secs(60)).await {
        Ok(_) => panic!("the landing gives up after five rounds"),
        Err(error) => error,
      };
    assert!(format!("{error:#}").contains("kept moving past 5"), "{error:#}");
    let view = land_queue::Site { worktree: work.clone() }.read().await.expect("the queue reads");
    assert!(view.turn.is_none() && view.entries.is_empty(), "the give-up released the turn and the entry");
  }
}
