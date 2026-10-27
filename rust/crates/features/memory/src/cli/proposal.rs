use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use charliebot_cli_common::args::with_infer_long_args;
use charliebot_cli_common::exit::CliExit;
use charliebot_core::home;
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};

const PROPOSAL_BRANCH: &str = "proposal";
const PROPOSAL_WORKTREE_SUFFIX: &str = "-proposal";

#[derive(Parser)]
#[command(name = "proposal", about = "Drive the memory store's review flow")]
struct ProposalCli {
  #[command(subcommand)]
  command: ProposalCommand,
}

#[derive(Subcommand)]
enum ProposalCommand {
  /// Ensure the proposal branch and worktree exist, aligned with the base branch.
  Open,
  /// Print the proposal state.
  Status,
  /// Fast-forward the live checkout to one reviewed proposal commit.
  Land(LandArgs),
}

#[derive(Args)]
struct LandArgs {
  /// The reviewed proposal commit SHA.
  sha: String,
}

enum ProposalError {
  Refusal(String),
  Cli(CliExit),
}

struct GitOutput {
  success: bool,
  stdout: String,
  stderr: String,
}

type ProposalResult<T> = Result<T, ProposalError>;

/// Parse the proposal arguments and run one local store operation.
pub(super) fn main(args: &[OsString]) -> Result<(), CliExit> {
  let command = with_infer_long_args(ProposalCli::command());
  let argv = std::iter::once(OsString::from("charliebot memory proposal")).chain(args.iter().cloned());
  let matches = match command.try_get_matches_from(argv) {
    Ok(matches) => matches,
    Err(error) => error.exit(),
  };
  let cli = ProposalCli::from_arg_matches(&matches).expect("try_get_matches_from validated the arguments");

  let home_dir = home::charliebot_home_dir().map_err(|error| CliExit::error(error.to_string()))?;
  let memory_dir = home_dir.join("memory");
  crate::memory::ensure_store(&memory_dir).map_err(|error| CliExit::error(error.to_string()))?;
  let memory_dir = fs::canonicalize(&memory_dir)
    .map_err(|error| CliExit::error(format!("resolve {}: {error}", memory_dir.display())))?;

  match run_proposal(&memory_dir, cli.command) {
    Ok(fields) => {
      for (key, value) in fields {
        println!("{key}: {value}");
      }
      Ok(())
    }
    Err(ProposalError::Refusal(message)) => {
      eprintln!("error: {message}");
      std::process::exit(1);
    }
    Err(ProposalError::Cli(error)) => Err(error),
  }
}

fn run_proposal(memory_dir: &Path, command: ProposalCommand) -> ProposalResult<Vec<(String, String)>> {
  match command {
    ProposalCommand::Open => open_proposal(memory_dir),
    ProposalCommand::Status => status(memory_dir),
    ProposalCommand::Land(args) => land(memory_dir, &args.sha),
  }
}

fn proposal_worktree(memory_dir: &Path) -> PathBuf {
  let name = memory_dir.file_name().expect("the memory store path has a file name");
  let mut worktree_name = name.to_os_string();
  worktree_name.push(PROPOSAL_WORKTREE_SUFFIX);
  memory_dir.with_file_name(worktree_name)
}

fn diff_path(memory_dir: &Path, base: &str, head: &str) -> String {
  format!("/diff?repo={}&base={base}&head={head}", memory_dir.display())
}

fn run_git(cwd: &Path, args: &[&str]) -> ProposalResult<GitOutput> {
  let output = Command::new("git").args(args).current_dir(cwd).output().map_err(|error| {
    ProposalError::Cli(CliExit::error(format!("run git {} in {}: {error}", args.join(" "), cwd.display())))
  })?;
  let stdout = String::from_utf8(output.stdout)
    .map_err(|error| ProposalError::Cli(CliExit::error(format!("git stdout is not UTF-8: {error}"))))?;
  let stderr = String::from_utf8(output.stderr)
    .map_err(|error| ProposalError::Cli(CliExit::error(format!("git stderr is not UTF-8: {error}"))))?;
  Ok(GitOutput { success: output.status.success(), stdout, stderr })
}

fn git(cwd: &Path, args: &[&str]) -> ProposalResult<String> {
  let output = run_git(cwd, args)?;
  if !output.success {
    return Err(ProposalError::Refusal(format!(
      "git {} failed in {}: {}",
      args.join(" "),
      cwd.display(),
      output.stderr.trim()
    )));
  }
  Ok(output.stdout)
}

fn base_branch(memory_dir: &Path) -> ProposalResult<String> {
  let result = run_git(memory_dir, &["symbolic-ref", "--short", "HEAD"])?;
  if !result.success {
    return Err(ProposalError::Refusal(format!(
      "the live memory checkout is not on a branch ({}); check out the base branch first",
      result.stderr.trim()
    )));
  }
  Ok(result.stdout.trim().to_string())
}

fn ensure_branch(memory_dir: &Path, base: &str) -> ProposalResult<()> {
  let result = run_git(memory_dir, &["rev-parse", "--verify", "--quiet", "refs/heads/proposal"])?;
  if !result.success {
    let base_sha = git(memory_dir, &["rev-parse", base])?;
    git(memory_dir, &["branch", PROPOSAL_BRANCH, base_sha.trim()])?;
  }
  Ok(())
}

fn validate_worktree(memory_dir: &Path, worktree: &Path) -> ProposalResult<()> {
  let head_ref = git(worktree, &["symbolic-ref", "--short", "HEAD"])?;
  let head_ref = head_ref.trim();
  if head_ref != PROPOSAL_BRANCH {
    return Err(ProposalError::Refusal(format!(
      "{} is a worktree on '{head_ref}', not '{PROPOSAL_BRANCH}'",
      worktree.display()
    )));
  }
  let worktree_common_dir = git(worktree, &["rev-parse", "--git-common-dir"])?;
  let worktree_common = fs::canonicalize(worktree.join(worktree_common_dir.trim()))
    .map_err(|error| ProposalError::Cli(CliExit::error(format!("resolve git common directory: {error}"))))?;
  let live_common_dir = git(memory_dir, &["rev-parse", "--git-common-dir"])?;
  let live_common = fs::canonicalize(memory_dir.join(live_common_dir.trim()))
    .map_err(|error| ProposalError::Cli(CliExit::error(format!("resolve git common directory: {error}"))))?;
  if worktree_common != live_common {
    return Err(ProposalError::Refusal(format!(
      "{} exists but is not a worktree of the memory repo at {}",
      worktree.display(),
      memory_dir.display()
    )));
  }
  Ok(())
}

fn ensure_worktree(memory_dir: &Path, worktree: &Path) -> ProposalResult<()> {
  if !worktree.exists() {
    let worktree_path =
      worktree.to_str().ok_or_else(|| ProposalError::Cli(CliExit::error("the proposal worktree path is not UTF-8")))?;
    git(memory_dir, &["worktree", "add", worktree_path, PROPOSAL_BRANCH])?;
  }
  Ok(())
}

fn ahead_count(memory_dir: &Path, base: &str) -> ProposalResult<u64> {
  let range = format!("{base}..{PROPOSAL_BRANCH}");
  let count = git(memory_dir, &["rev-list", "--count", &range])?;
  count.trim().parse().map_err(|error| {
    ProposalError::Cli(CliExit::error(format!("git rev-list --count returned an invalid count: {error}")))
  })
}

fn open_proposal(memory_dir: &Path) -> ProposalResult<Vec<(String, String)>> {
  let worktree = proposal_worktree(memory_dir);
  let uncommitted = run_git(memory_dir, &["status", "--porcelain", "--untracked-files=no"])?;
  if !uncommitted.stdout.trim().is_empty() {
    return Err(ProposalError::Refusal(format!(
      "the live memory checkout has uncommitted changes; commit or discard them first:\n{}",
      uncommitted.stdout.trim_end()
    )));
  }
  let base = base_branch(memory_dir)?;
  if worktree.exists() {
    validate_worktree(memory_dir, &worktree)?;
  }
  ensure_branch(memory_dir, &base)?;
  ensure_worktree(memory_dir, &worktree)?;
  let ahead = ahead_count(memory_dir, &base)?;
  if ahead == 0 {
    git(&worktree, &["merge", "--ff-only", &base])?;
  } else {
    let ancestry = run_git(memory_dir, &["merge-base", "--is-ancestor", &base, PROPOSAL_BRANCH])?;
    if !ancestry.success {
      let worktree_status = run_git(&worktree, &["status", "--porcelain"])?;
      if !worktree_status.stdout.trim().is_empty() {
        return Err(ProposalError::Refusal(format!(
          "{base} moved and the proposal worktree has uncommitted changes; commit or discard them so the branch can rebase"
        )));
      }
      let rebase = run_git(&worktree, &["rebase", &base])?;
      if !rebase.success {
        let abort = run_git(&worktree, &["rebase", "--abort"])?;
        let mut detail = rebase.stderr.trim().to_string();
        if !abort.success {
          detail.push_str(&format!("; rebase --abort also failed: {}", abort.stderr.trim()));
        }
        return Err(ProposalError::Refusal(format!(
          "rebasing '{PROPOSAL_BRANCH}' onto {base} conflicted; the branch keeps its commits: {detail}"
        )));
      }
    }
  }
  status(memory_dir)
}

fn status(memory_dir: &Path) -> ProposalResult<Vec<(String, String)>> {
  let base = base_branch(memory_dir)?;
  let missing = run_git(memory_dir, &["rev-parse", "--verify", "--quiet", "refs/heads/proposal"])?;
  if !missing.success {
    return Err(ProposalError::Refusal(format!(
      "no '{PROPOSAL_BRANCH}' branch in {}; run 'charliebot memory proposal open' first",
      memory_dir.display()
    )));
  }
  let head = git(memory_dir, &["rev-parse", PROPOSAL_BRANCH])?;
  let head = head.trim().to_string();
  let ahead = ahead_count(memory_dir, &base)?;
  let opened = if ahead > 0 {
    let range = format!("{base}..{PROPOSAL_BRANCH}");
    let commits = git(memory_dir, &["rev-list", &range])?;
    let oldest = commits.lines().last().expect("an ahead proposal has a commit");
    let date = git(memory_dir, &["show", "--no-patch", "--date=short", "--format=%cd", oldest])?;
    date.trim().to_string()
  } else {
    "-".to_string()
  };
  let worktree = proposal_worktree(memory_dir);
  let dirty = if worktree.exists() {
    let result = run_git(&worktree, &["status", "--porcelain"])?;
    if result.stdout.trim().is_empty() { "no" } else { "yes" }.to_string()
  } else {
    "-".to_string()
  };
  Ok(vec![
    ("worktree".to_string(), worktree.display().to_string()),
    ("base".to_string(), base.clone()),
    ("head".to_string(), head.clone()),
    ("ahead".to_string(), ahead.to_string()),
    ("opened".to_string(), opened),
    ("dirty".to_string(), dirty),
    ("diff_path".to_string(), diff_path(memory_dir, &base, &head)),
  ])
}

fn land(memory_dir: &Path, sha: &str) -> ProposalResult<Vec<(String, String)>> {
  let base = base_branch(memory_dir)?;
  let uncommitted = run_git(memory_dir, &["status", "--porcelain"])?;
  if !uncommitted.stdout.trim().is_empty() {
    return Err(ProposalError::Refusal(format!(
      "the live memory checkout is not clean; land needs it clean:\n{}",
      uncommitted.stdout.trim_end()
    )));
  }
  let requested_commit = format!("{sha}^{{commit}}");
  let resolved = run_git(memory_dir, &["rev-parse", "--verify", "--quiet", &requested_commit])?;
  if !resolved.success {
    return Err(ProposalError::Refusal(format!("'{sha}' does not resolve to a commit in {}", memory_dir.display())));
  }
  let commit_sha = resolved.stdout.trim();
  let on_proposal = run_git(memory_dir, &["merge-base", "--is-ancestor", commit_sha, PROPOSAL_BRANCH])?;
  if !on_proposal.success {
    return Err(ProposalError::Refusal(format!("'{sha}' is not on the '{PROPOSAL_BRANCH}' branch")));
  }
  let base_is_ancestor = run_git(memory_dir, &["merge-base", "--is-ancestor", &base, commit_sha])?;
  if !base_is_ancestor.success {
    return Err(ProposalError::Refusal(format!(
      "{base} is not an ancestor of '{sha}'; the land would move the branch backwards"
    )));
  }
  lint_tree(memory_dir, commit_sha)?;
  let old_base = git(memory_dir, &["rev-parse", &base])?;
  let old_base = old_base.trim().to_string();
  git(memory_dir, &["merge", "--ff-only", commit_sha])?;
  let range = format!("{old_base}..{commit_sha}");
  let commits = git(memory_dir, &["rev-list", "--count", &range])?;
  let commits = commits.trim().to_string();
  let remaining_range = format!("{commit_sha}..{PROPOSAL_BRANCH}");
  let remaining = git(memory_dir, &["rev-list", "--count", &remaining_range])?;
  let remaining = remaining.trim().to_string();
  let old_base_short = git(memory_dir, &["rev-parse", "--short", &old_base])?;
  let old_base_short = old_base_short.trim();
  let commit_short = git(memory_dir, &["rev-parse", "--short", commit_sha])?;
  let commit_short = commit_short.trim();
  Ok(vec![
    ("landed".to_string(), format!("{old_base_short}..{commit_short}")),
    ("commits".to_string(), commits),
    ("remaining".to_string(), remaining),
    ("diff_path".to_string(), diff_path(memory_dir, &old_base, commit_sha)),
  ])
}

fn lint_tree(memory_dir: &Path, commit_sha: &str) -> ProposalResult<()> {
  let temporary = tempfile::tempdir()
    .map_err(|error| ProposalError::Cli(CliExit::error(format!("create temporary lint tree: {error}"))))?;
  let tree = temporary.path().join("tree");
  fs::create_dir(&tree)
    .map_err(|error| ProposalError::Cli(CliExit::error(format!("create {}: {error}", tree.display()))))?;
  let archive = temporary.path().join("tree.tar");
  let archive_path = archive
    .to_str()
    .ok_or_else(|| ProposalError::Cli(CliExit::error("the proposal lint archive path is not UTF-8")))?;
  git(memory_dir, &["archive", "--format=tar", "--output", archive_path, commit_sha])?;
  let archive_file = fs::File::open(&archive)
    .map_err(|error| ProposalError::Cli(CliExit::error(format!("open {}: {error}", archive.display()))))?;
  tar::Archive::new(archive_file)
    .unpack(&tree)
    .map_err(|error| ProposalError::Cli(CliExit::error(format!("extract {}: {error}", archive.display()))))?;
  let (violations, _warnings) = super::store::lint_store(&tree).map_err(ProposalError::Cli)?;
  if !violations.is_empty() {
    return Err(ProposalError::Refusal(format!("the proposed tree fails the store lint:\n{}", violations.join("\n"))));
  }
  Ok(())
}
