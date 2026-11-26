//! The implement landing gate: the engine behind `charliebot land`.
//!
//! This module is the one owner of the implement landing. Every call re-reads git and GitHub state
//! and acts on it — no saved intermediate state — so a review run that a trigger wake resumes and a
//! later run in the task's own session take the same path. The gate applies when the base is
//! the repository's default branch and that branch declares a workflow with a `pull_request`
//! trigger; every other landing pushes as today and records `landed-no-ci`.
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
//! `--force-with-lease`, keeps one draft PR for the branch, and pushes `<sha>:<base>` only when
//! every expected workflow ran green on the head sha and every check run and commit status on it is
//! success, skipped or neutral. Both clocks — the 10-minute not-started clock and the 90-minute
//! timeout — run per head sha from the time that sha was pushed to the task branch, so a rebase
//! starts both again and a busy main costs `rebase_retries`, never a timeout. Main is never pushed
//! on any state other than a pass, and no state falls back to a direct push.
//!
//! A push that moves the base branch, gated or not, leaves no open PR of the task branch behind:
//! the call reads the branch's PRs back after the push and closes each one GitHub has not marked
//! merged, so `landed` and `landed-no-ci` both mean the PR reads merged or closed.
//!
//! A rejected push of the base starts another round only when the remote base verifiably advanced
//! past the pushed commit's base; every other refusal — a hook, a dead transport, a timeout, an
//! auth error — ends the call at once with git's stderr verbatim.

use std::collections::BTreeSet;
use std::fs::DirBuilder;
use std::future::Future;
use std::io::Read as _;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context as _;
use charliebot_backend_check::{Vocabulary, gate};
use charliebot_core::git::ancestry_args;
use charliebot_core::home::CHARLIEBOT_HOME_ENV;
use charliebot_core::text_repr::string_field;
use charliebot_registry::Registry;
use charliebot_registry::pages::WEB_DIR_NAME;
use charliebot_store::layout;
use charliebot_store::metadata::{read_metadata_value, read_session_metadata};
use charliebot_store::run_records::{self, RunRecord};
use charliebot_store::trigger_files;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::git;
use crate::github_api::{GhOutput, RestOutcome, rest_get, run_gh};
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
}

impl LandingRecord {
  fn with_state(&self, state: &str) -> LandingRecord {
    LandingRecord { state: state.to_string(), ..self.clone() }
  }
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
struct TaskLanding {
  repo: String,
  worktree: PathBuf,
  repo_path: PathBuf,
  base_branch: String,
  branch_name: String,
  session_dir: PathBuf,
}

impl TaskLanding {
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
    let repo = repo_from_remote_url(&url)
      .ok_or_else(|| anyhow::anyhow!("origin's URL {url} in {repo_path} names no owner/repo path"))?;
    Ok(Self {
      repo,
      worktree: PathBuf::from(worktree),
      repo_path: PathBuf::from(repo_path),
      base_branch,
      branch_name,
      session_dir,
    })
  }
}

/// The `owner/repo` path a remote URL names, the form every `gh api` path needs. The URL forms
/// GitHub serves — `https://`, `ssh://` and the scp style — all end in an `owner/repo` path.
fn repo_from_remote_url(url: &str) -> Option<String> {
  let trimmed = url.trim().trim_end_matches('/');
  let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);
  let path = if let Some((_, rest)) = trimmed.split_once("://") {
    rest.split_once('/').map(|(_, tail)| tail)
  } else {
    trimmed.split_once(':').map(|(_, tail)| tail)
  }?;
  let repo = path.trim_start_matches('/');
  (!repo.is_empty() && repo.contains('/') && !repo.contains(':')).then(|| repo.to_string())
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

/// Refuses a landing whose `paths` hold a hardcoded backend choice that the check does not exempt, and names each site.
/// The check reads the host's vocabulary: the registered packages and the profile's `config.yaml`.
fn refuse_backend_literals(registry: &Registry, home: &Path, worktree: &Path, paths: &[PathBuf]) -> anyhow::Result<()> {
  let roots = gate::roots(Some(worktree), home)?;
  let vocab = Vocabulary::load(registry, home, None)?;
  let hits = gate::open_hits(&vocab, &roots, paths)?;
  if hits.is_empty() {
    return Ok(());
  }
  anyhow::bail!(
    "landing refused before any push or PR create: a hardcoded backend choice in the changed files:\n{}",
    gate::report(&hits, roots.repo.as_deref())
  )
}

/// The backend-literal check of a landing: the files the task branch changes against the base.
async fn backend_literal_check(
  registry: &Registry,
  home: &Path,
  worktree: &Path,
  base_ref: &str,
) -> anyhow::Result<()> {
  let changed = changed_paths(worktree, base_ref).await?;
  refuse_backend_literals(registry, home, worktree, &literal_check_paths(worktree, &changed)?)
}

/// Run one landing: the whole flow, ending in a written `landing.json` and the record returned. The backend-literal
/// check runs on the head that the landing pushes, before either push.
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
  let prev = read_landing_record(&ctx.session_dir)?;
  let prev = prev.filter(|record| record.state != STATE_LANDED && record.state != STATE_LANDED_NO_CI);
  let mut retries = prev.as_ref().map_or(0, |record| record.rebase_retries);
  let mut moved_base_tips: Vec<String> = Vec::new();
  let mut first_round = true;

  for _round in 0..MAX_LANDING_ROUNDS {
    if !first_round {
      fetch_base(&ctx.worktree, published).await?;
    }
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
        };
        write_landing_record(&ctx.session_dir, &record)?;
        return Ok(record);
      }
      RebaseStep::UpToDate => {}
      RebaseStep::Rewrote => {
        if rewrite_counts_as_retry(prev.as_ref(), first_round) {
          retries += 1;
        }
      }
    }
    first_round = false;

    carry_view_trailers(&ctx.worktree, &views).await?;
    backend_literal_check(registry, home, &ctx.worktree, &base_ref).await?;
    let sha = head_sha(&ctx.worktree).await?;
    let push_base_sha = git::git_rev_parse(&ctx.worktree, &base_ref)
      .await
      .ok_or_else(|| anyhow::anyhow!("{base_ref} does not resolve in {}", ctx.worktree.display()))?;
    let head_pushed_at = carried_pushed_at(prev.as_ref(), &sha, &now_rfc3339());

    if !gated {
      // No gate: land as today — one fast-forward push of the head onto the base branch.
      outbound_checks(home, &ctx.worktree, &ctx.repo_path, &ctx.branch_name, &base_ref, None).await?;
      let (ok, _, err) = git::git_stdout(
        &ctx.worktree,
        &["push", "origin", &format!("{sha}:refs/heads/{published}")],
        GIT_PUSH_TIMEOUT,
        "git push",
      )
      .await;
      if ok {
        let record = LandingRecord {
          state: STATE_LANDED_NO_CI.to_string(),
          repo: ctx.repo.clone(),
          base: published.to_string(),
          sha,
          pr_url: None,
          checks: Vec::new(),
          head_pushed_at,
          ci_passed_at: None,
          pushed_at: Some(now_rfc3339()),
          rebase_retries: retries,
        };
        return record_landed(&ctx, published, record).await;
      }
      match judge_push_rejection(&ctx.worktree, published, &push_base_sha, &sha, &err).await? {
        PushOutcome::PrMergeRequired => {
          let record = LandingRecord {
            state: STATE_PR_MERGE_REQUIRED.to_string(),
            repo: ctx.repo.clone(),
            base: published.to_string(),
            sha,
            pr_url: None,
            checks: Vec::new(),
            head_pushed_at,
            ci_passed_at: None,
            pushed_at: None,
            rebase_retries: retries,
          };
          write_landing_record(&ctx.session_dir, &record)?;
          return Ok(record);
        }
        PushOutcome::BaseMoved(tip) => {
          // The base verifiably moved: the next round rebases onto the new tip and pushes again.
          tracing::info!(target: "charliebot_tasks", repo = %ctx.repo_path.display(), base_tip = %tip, error = %err, "land_push_rejected_retry");
          moved_base_tips.push(tip);
          continue;
        }
      }
    }

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
      // The watch that this state registers waits out the quota and then reads the head sha again.
      CiRead::RateLimited { reset } => {
        tracing::info!(target: "charliebot_tasks", branch = %ctx.branch_name, reset, "land_ci_read_rate_limited");
        (CiEvaluation::unread(), CiVerdict::Waiting)
      }
    };

    let base_record = LandingRecord {
      state: String::new(),
      repo: ctx.repo.clone(),
      base: published.to_string(),
      sha: sha.clone(),
      pr_url: Some(pr_url.clone()),
      checks: evaluation.check_rows(),
      head_pushed_at: head_pushed_at.clone(),
      ci_passed_at: evaluation.ci_passed_at(),
      pushed_at: None,
      rebase_retries: retries,
    };
    match verdict {
      CiVerdict::Pass => {
        let (ok, _, err) = git::git_stdout(
          &ctx.worktree,
          &["push", "origin", &format!("{sha}:refs/heads/{published}")],
          GIT_PUSH_TIMEOUT,
          "git push",
        )
        .await;
        if ok {
          let record = LandingRecord { state: STATE_LANDED.to_string(), pushed_at: Some(now_rfc3339()), ..base_record };
          return record_landed(&ctx, published, record).await;
        }
        match judge_push_rejection(&ctx.worktree, published, &push_base_sha, &sha, &err).await? {
          PushOutcome::PrMergeRequired => {
            let record = base_record.with_state(STATE_PR_MERGE_REQUIRED);
            write_landing_record(&ctx.session_dir, &record)?;
            return Ok(record);
          }
          PushOutcome::BaseMoved(tip) => {
            // Main moved while the checks ran: the next round rebases, and both clocks restart.
            tracing::info!(target: "charliebot_tasks", repo = %ctx.repo_path.display(), base_tip = %tip, error = %err, "land_main_push_rejected_rebase");
            moved_base_tips.push(tip);
            continue;
          }
        }
      }
      CiVerdict::Failed { .. } => {
        let record = base_record.with_state(STATE_CI_FAILED);
        write_landing_record(&ctx.session_dir, &record)?;
        return Ok(record);
      }
      CiVerdict::NotStarted { .. } => {
        let record = base_record.with_state(STATE_CI_NOT_STARTED);
        write_landing_record(&ctx.session_dir, &record)?;
        return Ok(record);
      }
      CiVerdict::Timeout => {
        let record = base_record.with_state(STATE_CI_TIMEOUT);
        write_landing_record(&ctx.session_dir, &record)?;
        return Ok(record);
      }
      CiVerdict::Waiting => {
        ensure_waiting_trigger(&ctx, &ctx.repo, pr_number, &sha).await?;
        let record = base_record.with_state(STATE_WAITING);
        write_landing_record(&ctx.session_dir, &record)?;
        return Ok(record);
      }
    }
  }
  anyhow::bail!(
    "main kept moving past {MAX_LANDING_ROUNDS} landing rounds (the base advanced to {}); run `charliebot land` again when main settles",
    moved_base_tips.join(", ")
  )
}

const GIT_READ_TIMEOUT: Duration = Duration::from_secs(30);
const GIT_WRITE_TIMEOUT: Duration = Duration::from_secs(120);
const GIT_PUSH_TIMEOUT: Duration = Duration::from_secs(120);
const GH_TIMEOUT: Duration = Duration::from_secs(60);

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

/// The task branch push under `--force-with-lease`: the lease is the fetched tracking ref when origin
/// already has the branch, and an absent ref when it does not, so a rebase never clobbers another push.
async fn push_task_branch(ctx: &TaskLanding, branch: &str) -> anyhow::Result<()> {
  if origin_has_branch(ctx, branch).await? {
    let (fetched, _, fetch_err) =
      git::git_stdout(&ctx.worktree, &["fetch", "origin", branch], GIT_WRITE_TIMEOUT, "git fetch task branch").await;
    if !fetched {
      anyhow::bail!("git fetch origin {branch} failed: {fetch_err}");
    }
  }
  let (ok, _, err) = git::git_stdout(
    &ctx.worktree,
    &["push", "--force-with-lease", "origin", branch],
    GIT_PUSH_TIMEOUT,
    "git push task branch",
  )
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

/// The verdict of one rejected base push: `PrMergeRequired` keeps the repository-rules record, and
/// `BaseMoved` — the only outcome that starts another round — names the moved base tip.
enum PushOutcome {
  PrMergeRequired,
  BaseMoved(String),
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

/// The terminal error of a rejected push no round follows: the refusal's class name plus git's
/// stderr verbatim.
fn push_failure_text(kind: PushRejection, published: &str, stderr: &str) -> String {
  let wording = match kind {
    PushRejection::Timeout => "timed out",
    PushRejection::Transport => "failed on the transport",
    PushRejection::Auth => "failed authentication",
    PushRejection::NonFastForward => "was rejected as non-fast-forward",
    PushRejection::Refused => "was refused",
    PushRejection::RepositoryRules => unreachable!("repository rules keep their own record"),
  };
  format!("push to refs/heads/{published} {wording}: {stderr}")
}

/// Decide one rejected base push: repository rules hand back `PrMergeRequired`; a rejection whose
/// class can report on a moved base yields `BaseMoved` only when the remote tip really advanced
/// past the pushed commit's base — the pushed sha is no descendant of the just-fetched tip. Every
/// other refusal ends the landing here with git's stderr verbatim.
async fn judge_push_rejection(
  worktree: &Path,
  published: &str,
  push_base_sha: &str,
  sha: &str,
  stderr: &str,
) -> anyhow::Result<PushOutcome> {
  let kind = push_rejection_kind(stderr);
  match kind {
    PushRejection::RepositoryRules => return Ok(PushOutcome::PrMergeRequired),
    PushRejection::Timeout | PushRejection::Transport | PushRejection::Auth | PushRejection::Refused => {
      anyhow::bail!("{}", push_failure_text(kind, published, stderr))
    }
    PushRejection::NonFastForward => {}
  }
  match verify_base_moved(worktree, published, push_base_sha, sha).await {
    Ok(Some(tip)) => Ok(PushOutcome::BaseMoved(tip)),
    Ok(None) => anyhow::bail!("{}", push_failure_text(kind, published, stderr)),
    Err(error) => {
      anyhow::bail!(
        "{}; checking whether {published} advanced failed too: {error:#}",
        push_failure_text(kind, published, stderr)
      )
    }
  }
}

/// Whether the remote base really advanced past the pushed commit's base: re-fetch the base, then
/// check the pushed sha against the new tip. `Some(tip)` names the moved tip; `None` means the push
/// still fast-forwards, so the rejection named a refusal of another kind.
async fn verify_base_moved(
  worktree: &Path,
  published: &str,
  push_base_sha: &str,
  sha: &str,
) -> anyhow::Result<Option<String>> {
  fetch_base(worktree, published).await?;
  let base_ref = format!("origin/{published}");
  let tip = git::git_rev_parse(worktree, &base_ref)
    .await
    .ok_or_else(|| anyhow::anyhow!("{base_ref} does not resolve in {}", worktree.display()))?;
  let advanced =
    git::git_stdout(worktree, &ancestry_args(push_base_sha, &tip), GIT_READ_TIMEOUT, "git merge-base").await;
  match advanced {
    (true, _, _) => {}
    (false, _, err) if merge_base_unknown(&err) => return Ok(None),
    (false, _, err) => anyhow::bail!("git merge-base --is-ancestor {push_base_sha} {tip} failed: {err}"),
  }
  let pushed_sha_is_ancestor =
    git::git_stdout(worktree, &ancestry_args(&tip, sha), GIT_READ_TIMEOUT, "git merge-base").await;
  match pushed_sha_is_ancestor {
    (true, _, _) => Ok(None),
    (false, _, err) if merge_base_unknown(&err) => Ok(Some(tip)),
    (false, _, err) => anyhow::bail!("git merge-base --is-ancestor {tip} {sha} failed: {err}"),
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

/// The wake the waiting state registers: one `charliebot session trigger --cmd` per head sha, running
/// `ci_gate_watch_command`. A pending trigger for this head sha already sitting in the session's
/// triggers directory is left alone, so repeated `land` calls never stack wakes.
async fn ensure_waiting_trigger(ctx: &TaskLanding, repo: &str, pr_number: i64, sha: &str) -> anyhow::Result<()> {
  let message = trigger_message(sha);
  if pending_trigger_with_message(&ctx.session_dir, &message)? {
    return Ok(());
  }
  let cmd = ci_gate_watch_command(repo, pr_number, sha);
  let max_wait = TIMEOUT_SECS.to_string();
  let mut command = tokio::process::Command::new("charliebot");
  command
    .args(["session", "trigger", "--max-wait", &max_wait, "--message", &message, "--cmd", &cmd])
    .current_dir(&ctx.worktree)
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .kill_on_drop(true);
  let output = tokio::time::timeout(GH_TIMEOUT, command.output()).await;
  match output {
    Ok(Ok(output)) if output.status.success() => Ok(()),
    Ok(Ok(output)) => anyhow::bail!(
      "registering the waiting trigger failed (exit {}): {}",
      output.status.code().unwrap_or(-1),
      String::from_utf8_lossy(&output.stderr).trim()
    ),
    Ok(Err(error)) => anyhow::bail!("charliebot session trigger failed to start: {error}"),
    Err(_) => anyhow::bail!("charliebot session trigger timed out after {}s", GH_TIMEOUT.as_secs()),
  }
}

/// The pause between the CI gate's registration polls of a head sha. A poll whose answer is unchanged costs no
/// count against the primary rate limit (a 304), so the pause only bounds how fast a registration is noticed.
pub const CHECK_REGISTRATION_POLL: Duration = Duration::from_secs(30);
/// Seconds the CI gate's watch may run past `TIMEOUT_SECS`. `start_and_register` in
/// `rust/crates/app/cli/src/commands/session_trigger.rs` starts the watch before it posts the
/// registration that sets the wake's clock. A watch bounded at exactly `TIMEOUT_SECS` would exit
/// first and wake as a failed watch; this margin lets the trigger's timeout end the wait instead.
const CI_WATCH_BOUND_MARGIN_SECS: i64 = 120;

/// The CI gate's watch command for one head sha. An empty check list right after a push means GitHub
/// has not registered the checks yet, so `charliebot ci-wait` polls until the head sha has a check run or a
/// commit status (`await_check_registration`). The command then watches the PR's checks to their end with
/// fail-fast: exit 0 when every check passes, exit 1 at the first failure. The whole command ends at
/// `TIMEOUT_SECS + CI_WATCH_BOUND_MARGIN_SECS`, so a PR that never gets a check wakes as the trigger's timeout.
fn ci_gate_watch_command(repo: &str, pr_number: i64, sha: &str) -> String {
  let bound = TIMEOUT_SECS + CI_WATCH_BOUND_MARGIN_SECS;
  format!(
    "timeout {bound} sh -c 'charliebot ci-wait --repo {repo} --sha {sha} || exit $?; \
     gh pr checks {pr_number} --repo {repo} --watch --fail-fast'"
  )
}

/// The CI gate's registration wait: polls the head sha's check runs, then its commit statuses, until one of
/// them exists. Each endpoint's ETag goes back with its next poll, so an unchanged answer is a 304, which GitHub
/// does not count against the primary rate limit. A rate-limit 403 is reported through `report` with its reset
/// time, and the poll waits until that time before it goes on. Any other failure ends the wait as an error.
pub async fn await_check_registration<Run, Fut>(
  run: &Run,
  repo: &str,
  sha: &str,
  interval: Duration,
  report: &dyn Fn(&str),
) -> anyhow::Result<()>
where
  Run: Fn(Vec<String>) -> Fut,
  Fut: Future<Output = anyhow::Result<GhOutput>>,
{
  let paths = [format!("repos/{repo}/commits/{sha}/check-runs"), format!("repos/{repo}/commits/{sha}/status")];
  let mut etags: [Option<String>; 2] = [None, None];
  loop {
    for (path, etag) in paths.iter().zip(etags.iter_mut()) {
      let Some((body, fresh_etag)) = poll_until_answered(run, path, etag.as_deref(), report).await? else {
        continue;
      };
      let total = body
        .get("total_count")
        .and_then(Value::as_u64)
        .with_context(|| format!("gh api {path} printed no total_count"))?;
      if total > 0 {
        return Ok(());
      }
      *etag = fresh_etag;
    }
    tokio::time::sleep(interval).await;
  }
}

/// One endpoint's answer: its body with the ETag to send next, or `None` when the body is unchanged. A
/// rate-limit 403 is reported and waited out before the same request is sent again.
async fn poll_until_answered<Run, Fut>(
  run: &Run,
  path: &str,
  etag: Option<&str>,
  report: &dyn Fn(&str),
) -> anyhow::Result<Option<(Value, Option<String>)>>
where
  Run: Fn(Vec<String>) -> Fut,
  Fut: Future<Output = anyhow::Result<GhOutput>>,
{
  loop {
    match rest_get(run, path, etag).await? {
      RestOutcome::Body { value, etag: next_etag } => return Ok(Some((value, next_etag))),
      RestOutcome::NotModified => return Ok(None),
      RestOutcome::RateLimited { reset } => {
        let reset_at = OffsetDateTime::from_unix_timestamp(reset)
          .with_context(|| format!("the X-RateLimit-Reset epoch {reset} is out of range"))?
          .format(&Rfc3339)
          .expect("the RFC 3339 format accepts a valid timestamp");
        report(&format!("GitHub REST quota is exhausted; {path} waits until {reset_at}"));
        tokio::time::sleep(duration_until(reset)).await;
      }
    }
  }
}

/// The time from now until the epoch second `reset`, zero when that second has passed.
fn duration_until(reset: i64) -> Duration {
  let seconds = (reset - OffsetDateTime::now_utc().unix_timestamp()).max(0);
  Duration::from_secs(u64::try_from(seconds).expect("the seconds are clamped to zero or more"))
}

/// The wake label of one head sha's checks; the dedup key of the landing's triggers.
pub fn trigger_message(sha: &str) -> String {
  format!("ci gate {}", &sha[..sha.len().min(12)])
}

fn pending_trigger_with_message(session_dir: &Path, message: &str) -> anyhow::Result<bool> {
  let triggers_dir = layout::triggers_dir(session_dir);
  if !triggers_dir.is_dir() {
    return Ok(false);
  }
  for (path, _) in trigger_files::iter_trigger_file_stats(&triggers_dir)? {
    let Ok(stored) = read_metadata_value(&path) else { continue };
    if stored.get("status").and_then(Value::as_str) == Some("pending")
      && stored.get("message").and_then(Value::as_str) == Some(message)
    {
      return Ok(true);
    }
  }
  Ok(false)
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

/// Write the record of a landing whose push moved the base branch to `record.sha`, then close what
/// GitHub left open of the task branch's PRs. The record comes first: the base branch holds the sha
/// whatever the PR step meets, and the delivery check reads that record.
async fn record_landed(ctx: &TaskLanding, published: &str, record: LandingRecord) -> anyhow::Result<LandingRecord> {
  write_landing_record(&ctx.session_dir, &record)?;
  close_task_prs(ctx, published, &record.sha).await.map_err(|error| {
    anyhow::anyhow!("{published} holds {} and landing.json reads {}, but {error:#}", record.sha, record.state)
  })?;
  Ok(record)
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
  use std::cell::RefCell;
  use std::collections::{HashSet, VecDeque};

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
    // The terminal error names the class and carries the push's stderr verbatim.
    let text = push_failure_text(PushRejection::Refused, "dev", hook);
    assert!(text.contains("was refused"), "{text}");
    assert!(text.contains(hook), "{text}");
    assert!(push_failure_text(PushRejection::Transport, "dev", "x").contains("transport"));
    assert!(push_failure_text(PushRejection::Timeout, "dev", "x").contains("timed out"));
    assert!(push_failure_text(PushRejection::Auth, "dev", "x").contains("authentication"));
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
  fn a_pending_trigger_for_the_head_sha_dedups_the_wake() {
    let session = tempfile::tempdir().expect("temporary directory is available");
    let sha = "c".repeat(40);
    assert!(!pending_trigger_with_message(session.path(), &trigger_message(&sha)).expect("the probe reads"));
    let triggers_dir = layout::triggers_dir(session.path());
    std::fs::create_dir_all(&triggers_dir).expect("the triggers directory is created");
    std::fs::write(
      triggers_dir.join("pending.json"),
      json!({ "id": "t1", "session_id": "s", "fire_at": "2026-11-10T03:00:00Z", "message": trigger_message(&sha), "status": "pending" })
        .to_string(),
    )
    .expect("the trigger file is written");
    assert!(pending_trigger_with_message(session.path(), &trigger_message(&sha)).expect("the probe reads"));
    assert!(!pending_trigger_with_message(session.path(), &trigger_message(&"d".repeat(40))).expect("the probe reads"));
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
  fn the_trigger_message_caps_at_the_head_sha_prefix() {
    let sha = "e".repeat(40);
    assert_eq!(trigger_message(&sha), "ci gate eeeeeeeeeeee");
  }

  /// The stubs of the watch command: `charliebot ci-wait` exits `CI_WAIT_EXIT`, and `gh pr checks` records that
  /// the PR check watch started and exits `GH_WATCH_EXIT`.
  const CI_WAIT_STUB: &str = "echo ran >> \"$GH_STATE/ci-wait\"\nexit \"$CI_WAIT_EXIT\"\n";
  const GH_WATCH_STUB: &str = r#"
case "$1 $2" in
  "pr checks")
    echo started > "$GH_STATE/watch"
    exit "$GH_WATCH_EXIT"
    ;;
  *) echo "unexpected gh $*" >&2; exit 2 ;;
esac
"#;

  /// Runs the CI gate's watch command with the stubbed `ci-wait` and `gh`. Returns the run and whether the PR check
  /// watch started; the stubs' state is read before the sandbox is dropped.
  fn run_ci_gate_watch(ci_wait_exit: u8, watch_exit: u8) -> (charliebot_shell_harness::Run, bool) {
    let sandbox = charliebot_shell_harness::Sandbox::new();
    let state = sandbox.dir("state");
    sandbox.stub("charliebot", CI_WAIT_STUB);
    sandbox.stub("gh", GH_WATCH_STUB);
    let mut env = sandbox.env();
    env.insert("GH_STATE".to_string(), state.display().to_string());
    env.insert("CI_WAIT_EXIT".to_string(), ci_wait_exit.to_string());
    env.insert("GH_WATCH_EXIT".to_string(), watch_exit.to_string());
    let cmd = ci_gate_watch_command("acme/widgets", 7, &"a".repeat(40));
    let mut command = charliebot_shell_harness::command("bash", &env);
    command.arg("-c").arg(&cmd);
    let run = charliebot_shell_harness::run(&mut command);
    let watch_started = state.join("watch").exists();
    (run, watch_started)
  }

  #[test]
  fn a_registered_head_sha_starts_the_pr_check_watch_and_keeps_its_exit() {
    for watch_exit in [0, 1] {
      let (run, watch_started) = run_ci_gate_watch(0, watch_exit);
      assert_eq!(run.code, i32::from(watch_exit), "{}", run.describe());
      assert!(watch_started, "the PR check watch starts once the registration wait returns");
    }
  }

  #[test]
  fn a_failed_registration_wait_fails_without_starting_the_watch() {
    let (run, watch_started) = run_ci_gate_watch(9, 0);
    assert_eq!(run.code, 9, "{}", run.describe());
    assert!(!watch_started, "the PR check watch must not start after a failed registration wait");
  }

  /// Replies to the registration poll's calls in order, and records the arguments of each call.
  struct Script {
    replies: RefCell<VecDeque<GhOutput>>,
    calls: RefCell<Vec<Vec<String>>>,
  }

  impl Script {
    fn new(replies: Vec<GhOutput>) -> Self {
      Self { replies: RefCell::new(VecDeque::from(replies)), calls: RefCell::new(Vec::new()) }
    }

    fn run(&self) -> impl Fn(Vec<String>) -> std::future::Ready<anyhow::Result<GhOutput>> + '_ {
      move |args: Vec<String>| {
        self.calls.borrow_mut().push(args);
        let next = self.replies.borrow_mut().pop_front().expect("the script has a reply for each call");
        std::future::ready(Ok(next))
      }
    }
  }

  fn json_reply(body: Value, etag: &str) -> GhOutput {
    GhOutput {
      success: true,
      stdout: format!("HTTP/2.0 200 OK\r\nEtag: {etag}\r\n\r\n{body}").into_bytes(),
      stderr: String::new(),
    }
  }

  fn not_modified() -> GhOutput {
    GhOutput {
      success: false,
      stdout: b"HTTP/2.0 304 Not Modified\r\n\r\n".to_vec(),
      stderr: "gh: HTTP 304".to_string(),
    }
  }

  fn rate_limited(reset: i64) -> GhOutput {
    GhOutput {
      success: false,
      stdout: format!(
        "HTTP/2.0 403 Forbidden\r\nX-RateLimit-Remaining: 0\r\nX-RateLimit-Reset: {reset}\r\n\r\n{{\"message\":\"x\"}}"
      )
      .into_bytes(),
      stderr: "gh: API rate limit exceeded (HTTP 403)".to_string(),
    }
  }

  fn server_error() -> GhOutput {
    GhOutput {
      success: false,
      stdout: b"HTTP/2.0 500 Internal Server Error\r\n\r\n".to_vec(),
      stderr: "gh: Internal Server Error (HTTP 500)".to_string(),
    }
  }

  const WATCHED_REPO: &str = "acme/widgets";
  const WATCHED_SHA: &str = "abcdef0123456789abcdef0123456789abcdef01";

  #[tokio::test(start_paused = true)]
  async fn a_rate_limited_registration_poll_waits_until_the_reset_then_continues() {
    let reset = OffsetDateTime::now_utc().unix_timestamp() + 60;
    let script = Script::new(vec![rate_limited(reset), json_reply(json!({"total_count": 1}), "\"checks-1\"")]);
    let reports = RefCell::new(Vec::new());
    let report = |line: &str| reports.borrow_mut().push(line.to_string());
    let start = tokio::time::Instant::now();
    await_check_registration(&script.run(), WATCHED_REPO, WATCHED_SHA, Duration::from_secs(30), &report)
      .await
      .expect("the poll continues after the quota returns");
    let waited = start.elapsed();
    assert!(
      (Duration::from_secs(59)..=Duration::from_secs(61)).contains(&waited),
      "the poll waits until the reset second, which is 60 s away; it waited {waited:?}"
    );
    assert_eq!(script.calls.borrow().len(), 2, "the same check-runs request is sent again after the reset");
    let reports = reports.borrow();
    assert_eq!(reports.len(), 1, "the 403 is reported once");
    assert!(reports[0].contains("quota is exhausted") && reports[0].contains("waits until"), "{}", reports[0]);
  }

  #[tokio::test(start_paused = true)]
  async fn an_unchanged_registration_poll_sends_its_etag_and_reads_the_304_as_still_empty() {
    let script = Script::new(vec![
      json_reply(json!({"total_count": 0}), "\"checks-1\""),
      json_reply(json!({"total_count": 0}), "\"status-1\""),
      not_modified(),
      not_modified(),
      json_reply(json!({"total_count": 1}), "\"checks-2\""),
    ]);
    let report = |_: &str| panic!("no rate limit was hit");
    await_check_registration(&script.run(), WATCHED_REPO, WATCHED_SHA, Duration::from_secs(30), &report)
      .await
      .expect("a registered check run ends the wait");
    let calls = script.calls.borrow();
    assert_eq!(calls.len(), 5, "check-runs, status, then both again, then check-runs");
    assert!(!calls[0].iter().any(|arg| arg.starts_with("If-None-Match")), "the first poll has no ETag to send");
    assert_eq!(calls[2][3], "If-None-Match: \"checks-1\"", "{:?}", calls[2]);
    assert_eq!(calls[3][3], "If-None-Match: \"status-1\"", "{:?}", calls[3]);
  }

  #[tokio::test(start_paused = true)]
  async fn a_commit_status_registers_the_head_sha_without_a_check_run() {
    let script = Script::new(vec![
      json_reply(json!({"total_count": 0}), "\"checks-1\""),
      json_reply(json!({"total_count": 1}), "\"status-1\""),
    ]);
    let report = |_: &str| panic!("no rate limit was hit");
    await_check_registration(&script.run(), WATCHED_REPO, WATCHED_SHA, Duration::from_secs(30), &report)
      .await
      .expect("a commit status ends the wait");
    assert_eq!(script.calls.borrow().len(), 2);
  }

  #[tokio::test(start_paused = true)]
  async fn a_failed_registration_poll_is_an_error_and_waits_for_nothing() {
    let script = Script::new(vec![server_error()]);
    let report = |_: &str| panic!("a 500 is not a rate limit");
    let start = tokio::time::Instant::now();
    let error = await_check_registration(&script.run(), WATCHED_REPO, WATCHED_SHA, Duration::from_secs(30), &report)
      .await
      .unwrap_err();
    assert!(error.to_string().contains("answered HTTP 500"), "{error:#}");
    assert_eq!(script.calls.borrow().len(), 1);
    assert_eq!(start.elapsed(), Duration::ZERO, "a failure does not sleep");
  }

  #[test]
  fn the_remote_url_names_the_owner_repo_path() {
    assert_eq!(repo_from_remote_url("https://github.com/owner/repo.git"), Some("owner/repo".to_string()));
    assert_eq!(repo_from_remote_url("git@github.com:owner/repo.git"), Some("owner/repo".to_string()));
    assert_eq!(repo_from_remote_url("ssh://git@github.com/owner/repo"), Some("owner/repo".to_string()));
    assert_eq!(repo_from_remote_url("ssh://git@github.com:22/owner/repo.git"), Some("owner/repo".to_string()));
    assert_eq!(repo_from_remote_url("https://github.com/owner/repo/"), Some("owner/repo".to_string()));
    assert_eq!(repo_from_remote_url("not-a-remote"), None);
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
    let text =
      format!("{:#}", refuse_backend_literals(&Registry::new(), &worktree.join("home"), worktree, &paths).unwrap_err());
    assert!(text.contains("scripts/run.sh:2") && text.contains("claude-opus-5-5"), "{text}");

    fs::write(worktree.join("scripts/run.sh"), "echo ok\n").unwrap();
    refuse_backend_literals(&Registry::new(), &worktree.join("home"), worktree, &paths).expect("a clean file passes");
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

  /// A lease-check rejection becomes `BaseMoved` only when the remote base verifiably advanced, and a
  /// hook refusal never turns into one even after the base moved.
  #[tokio::test]
  #[ignore = "integration"]
  async fn a_lease_check_rejection_is_a_base_move_only_when_the_remote_tip_moved() {
    let root = tempfile::tempdir().expect("temporary directory is available");
    let repo = synthetic_repo(root.path(), "feature");
    commit(&repo, "notes.txt", "feature work\n");
    let sha = head(&repo);
    let push_base_sha = git::git_rev_parse(&repo, "origin/main").await.expect("origin/main resolves");
    let lease_stderr = "remote: error: cannot lock ref 'refs/heads/main': is at 1111111 but expected 2222222\n\
                        To ../origin.git\n ! [remote rejected] abc123 -> main (cannot lock ref)\n\
                        error: failed to push some refs";

    // The base still holds the pushed base: the same lease-check stderr stays terminal.
    let unmoved = match judge_push_rejection(&repo, "main", &push_base_sha, &sha, lease_stderr).await {
      Ok(_) => panic!("an unmoved base keeps the rejection terminal"),
      Err(error) => format!("{error:#}"),
    };
    assert!(unmoved.contains("was rejected as non-fast-forward"), "{unmoved}");

    // The base really moved: the rejection starts another round at the new tip.
    let main_work = root.path().join("main-work");
    let clone =
      Command::new("git").args(["clone", "-q"]).arg(root.path().join("origin.git")).arg(&main_work).output().unwrap();
    assert!(clone.status.success(), "git clone failed: {}", String::from_utf8_lossy(&clone.stderr));
    commit(&main_work, "main.txt", "main work\n");
    assert!(
      Command::new("git").arg("-C").arg(&main_work).args(["push", "-q", "origin", "main"]).status().unwrap().success()
    );
    let moved_tip = head(&main_work);
    match judge_push_rejection(&repo, "main", &push_base_sha, &sha, lease_stderr).await {
      Ok(PushOutcome::BaseMoved(tip)) => assert_eq!(tip, moved_tip),
      Ok(PushOutcome::PrMergeRequired) => panic!("a lease check is not a repository-rules answer"),
      Err(error) => panic!("the moved base starts another round: {error:#}"),
    }

    // A hook refusal keeps its own class even now that the base moved.
    let hook_stderr = "remote: push to main is restricted; use merge_to_dev.sh <branch>\n\
                       ! [remote rejected] abc123 -> main (pre-receive hook declined)\n\
                       error: failed to push some refs";
    let hook = match judge_push_rejection(&repo, "main", &push_base_sha, &sha, hook_stderr).await {
      Ok(_) => panic!("a hook refusal is terminal"),
      Err(error) => format!("{error:#}"),
    };
    assert!(hook.contains("was refused"), "{hook}");
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
}
