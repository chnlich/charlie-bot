//! The implement landing gate: the engine behind `charliebot land`.
//!
//! This module is the one owner of the implement landing. Every call re-reads git and GitHub state
//! and acts on it — no saved intermediate state — so a review run that a trigger wake resumes and a
//! later run in the task's own session take the same path. The gate applies when the base is
//! the repository's default branch and that branch declares a workflow with a `pull_request`
//! trigger; every other landing pushes as today and records `landed-no-ci`.
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

use std::path::{Path, PathBuf};
use std::time::Duration;

use charliebot_core::git::ancestry_args;
use charliebot_store::layout;
use charliebot_store::metadata::read_session_metadata;
use charliebot_store::run_records::{self, RunRecord};
use charliebot_store::trigger_files;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::git;
use crate::review_text;

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

/// Run one landing: the whole flow, ending in a written `landing.json` and the record returned.
pub async fn land(home: &Path, task_session_id: &str) -> anyhow::Result<LandingRecord> {
  let ctx = TaskLanding::load(home, task_session_id).await?;
  let published = review_text::review_published_branch(&ctx.base_branch);
  let base_ref = format!("origin/{published}");
  fetch_base(&ctx.worktree, published).await?;

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

    let sha = head_sha(&ctx.worktree).await?;
    let push_base_sha = git::git_rev_parse(&ctx.worktree, &base_ref)
      .await
      .ok_or_else(|| anyhow::anyhow!("{base_ref} does not resolve in {}", ctx.worktree.display()))?;
    let head_pushed_at = carried_pushed_at(prev.as_ref(), &sha, &now_rfc3339());

    if !gated {
      // No gate: land as today — one fast-forward push of the head onto the base branch.
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

    push_task_branch(&ctx, &ctx.branch_name).await?;
    let (pr_url, pr_number) = ensure_draft_pr(&ctx, &ctx.repo, published).await?;
    let expected = expected_workflow_paths(&workflow_files_at(&ctx.worktree, &sha).await?)?;
    let evaluation = read_ci_state(&ctx.worktree, &ctx.repo, &sha).await?;
    let elapsed = match &head_pushed_at {
      Some(pushed_at) => elapsed_secs(pushed_at)?,
      None => 0,
    };
    let verdict = judge_ci(&expected, &evaluation.runs, &evaluation.check_runs, &evaluation.statuses, elapsed);

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
  } else if NON_FAST_FORWARD_MARKERS.iter().any(|marker| lowered.contains(marker)) {
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

/// The delivery-side gate check (execution.rs): when the gate applies, a commit that reached the
/// base branch without a `landing.json` in state `landed` blocks the delivery, so a hand push that
/// bypasses the gate cannot be reported as landed.
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

async fn read_ci_state(workdir: &Path, repo: &str, sha: &str) -> anyhow::Result<CiEvaluation> {
  let runs = gh_json(workdir, &["api", &format!("repos/{repo}/actions/runs?head_sha={sha}&per_page=100")]).await?;
  let check_runs = gh_json(workdir, &["api", &format!("repos/{repo}/commits/{sha}/check-runs?per_page=100")]).await?;
  let statuses = gh_json(workdir, &["api", &format!("repos/{repo}/commits/{sha}/status")]).await?;
  Ok(CiEvaluation {
    runs: runs.get("workflow_runs").cloned().unwrap_or(Value::Array(Vec::new())),
    check_runs: check_runs.get("check_runs").cloned().unwrap_or(Value::Array(Vec::new())),
    statuses: statuses.get("statuses").cloned().unwrap_or(Value::Array(Vec::new())),
  })
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

fn string_field(value: &Value, field: &str) -> Option<String> {
  value.get(field).and_then(Value::as_str).map(str::to_string)
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
  let mut command = tokio::process::Command::new("gh");
  command
    .args(args)
    .current_dir(workdir)
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .kill_on_drop(true);
  let output = tokio::time::timeout(GH_TIMEOUT, command.output()).await;
  let output = match output {
    Ok(Ok(output)) => output,
    Ok(Err(error)) => anyhow::bail!("gh {} failed to start: {error}", args.join(" ")),
    Err(_) => anyhow::bail!("gh {} timed out after {}s", args.join(" "), GH_TIMEOUT.as_secs()),
  };
  if !output.status.success() {
    anyhow::bail!("gh {} failed: {}", args.join(" "), String::from_utf8_lossy(&output.stderr).trim());
  }
  Ok(output.stdout)
}

/// The wake the waiting state registers: one `charliebot session trigger --cmd` per head sha whose
/// `--cmd` watches the PR's checks through to their end, capped at the 90-minute timeout. A
/// pending trigger for this head sha already sitting in the session's triggers directory is left
/// alone, so repeated `land` calls never stack wakes.
async fn ensure_waiting_trigger(ctx: &TaskLanding, repo: &str, pr_number: i64, sha: &str) -> anyhow::Result<()> {
  let message = trigger_message(sha);
  if pending_trigger_with_message(&ctx.session_dir, &message)? {
    return Ok(());
  }
  let cmd = format!("gh pr checks {pr_number} --repo {repo} --watch --fail-fast");
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
    let Ok(stored) = trigger_files::read_trigger_value(&path) else { continue };
    if stored.get("status").and_then(Value::as_str) == Some("pending")
      && stored.get("message").and_then(Value::as_str) == Some(message)
    {
      return Ok(true);
    }
  }
  Ok(false)
}

/// The one draft PR the landing keeps: reused when one is open for the task branch, created
/// otherwise. Returns `(url, number)`.
async fn ensure_draft_pr(ctx: &TaskLanding, repo: &str, published_base: &str) -> anyhow::Result<(String, i64)> {
  if let Ok(pr) =
    gh_json(&ctx.worktree, &["pr", "view", &ctx.branch_name, "--repo", repo, "--json", "number,url,state"]).await
    && string_field(&pr, "state").as_deref() == Some("OPEN")
  {
    return pr_url_and_number(&pr);
  }
  let title = format!("Land {}", ctx.branch_name);
  let body = format!(
    "Landing gate for `{branch}` → `{base}`.\n\nOpened by `charliebot land`; the base branch moves only when \
     CI passes on this PR's head sha.",
    branch = ctx.branch_name,
    base = published_base
  );
  let created = gh_json(
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
      &title,
      "--body",
      &body,
    ],
  )
  .await;
  if let Err(error) = &created {
    // A concurrent creation or a just-opened PR: log the refusal, then re-read before failing.
    tracing::info!(target: "charliebot_tasks", branch = %ctx.branch_name, error = %error, "land_pr_create_refused");
    tokio::time::sleep(Duration::from_secs(2)).await;
  }
  let pr =
    gh_json(&ctx.worktree, &["pr", "view", &ctx.branch_name, "--repo", repo, "--json", "number,url,state"]).await?;
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
  let mut open = open_task_prs(ctx).await?;
  while open.iter().any(|pr| pr.head_sha == sha) && tokio::time::Instant::now() < deadline {
    tokio::time::sleep(PR_MERGE_POLL).await;
    open = open_task_prs(ctx).await?;
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
  let left = open_task_prs(ctx).await?;
  if !left.is_empty() {
    let numbers = left.iter().map(|pr| format!("#{}", pr.number)).collect::<Vec<_>>().join(", ");
    anyhow::bail!("PR {numbers} of {} still reads open after the close ({})", ctx.branch_name, refusals.join("; "));
  }
  for refusal in &refusals {
    tracing::info!(target: "charliebot_tasks", branch = %ctx.branch_name, refusal = %refusal, "land_pr_close_refused");
  }
  Ok(())
}

async fn open_task_prs(ctx: &TaskLanding) -> anyhow::Result<Vec<OpenPr>> {
  let listed = gh_json(
    &ctx.worktree,
    &["pr", "list", "--repo", &ctx.repo, "--head", &ctx.branch_name, "--state", "open", "--json", "number,headRefOid"],
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
    // A hook refusal, an SSH reset, a timeout and an auth error each carry their own class.
    let hook = "pre-push: push to dev is restricted; use merge_to_dev.sh <branch>\n\
                error: failed to push some refs to 'gitee.com:owner/repo.git'";
    assert_eq!(push_rejection_kind(hook), PushRejection::Refused);
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

  #[test]
  fn the_remote_url_names_the_owner_repo_path() {
    assert_eq!(repo_from_remote_url("https://github.com/owner/repo.git"), Some("owner/repo".to_string()));
    assert_eq!(repo_from_remote_url("git@github.com:owner/repo.git"), Some("owner/repo".to_string()));
    assert_eq!(repo_from_remote_url("ssh://git@github.com/owner/repo"), Some("owner/repo".to_string()));
    assert_eq!(repo_from_remote_url("ssh://git@github.com:22/owner/repo.git"), Some("owner/repo".to_string()));
    assert_eq!(repo_from_remote_url("https://github.com/owner/repo/"), Some("owner/repo".to_string()));
    assert_eq!(repo_from_remote_url("not-a-remote"), None);
  }
}

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
}
