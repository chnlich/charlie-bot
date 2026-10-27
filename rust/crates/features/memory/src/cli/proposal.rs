use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use charliebot_cli_common::args::with_infer_long_args;
use charliebot_cli_common::exit::CliExit;
use charliebot_core::{home, paths, pyfmt, pystr};
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};

use super::proposal_check;
use super::store;

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
  /// Commit exactly one store-relative path on the proposal branch.
  Commit(CommitArgs),
  /// Fast-forward the live checkout to one reviewed proposal commit.
  Land(LandArgs),
}

#[derive(Args)]
struct CommitArgs {
  /// Store-relative path: an entry file or topics.
  path: String,
  /// File holding the commit message.
  #[arg(long)]
  message_file: PathBuf,
  /// Commit even when lines the PR already added are rewritten or gone; the commit message lists each replaced line
  /// under 'Replaced PR lines:'.
  #[arg(long)]
  replace_pr_lines: bool,
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
    Ok(output) => {
      for warning in output.warnings {
        eprintln!("warning: {warning}");
      }
      for (key, value) in output.fields {
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

/// One verb's result: the warnings it prints on stderr, then the `key: value` fields it prints on stdout.
struct Output {
  warnings: Vec<String>,
  fields: Vec<(String, String)>,
}

impl Output {
  fn from_fields(fields: Vec<(String, String)>) -> Self {
    Self { warnings: Vec::new(), fields }
  }
}

fn run_proposal(memory_dir: &Path, command: ProposalCommand) -> ProposalResult<Output> {
  match command {
    ProposalCommand::Open => open_proposal(memory_dir).map(Output::from_fields),
    ProposalCommand::Status => status(memory_dir).map(Output::from_fields),
    ProposalCommand::Commit(args) => {
      let committed = commit(memory_dir, &args.path, &args.message_file, args.replace_pr_lines)?;
      Ok(Output { warnings: committed.warnings, fields: vec![("committed".to_string(), committed.sha)] })
    }
    ProposalCommand::Land(args) => land(memory_dir, &args.sha).map(Output::from_fields),
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

/// A multiset of lines: each distinct line with its occurrence count.
type LineCounts = BTreeMap<String, usize>;

/// The result of one commit: the new head SHA and the negation warnings the committed text drew.
struct Committed {
  sha: String,
  warnings: Vec<String>,
}

/// The store path `path` names, as components. An absolute path, an empty one, or one that escapes the store is a
/// refusal.
fn store_path(path: &str) -> ProposalResult<PathBuf> {
  let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty() && *part != ".").collect();
  if path.starts_with('/') || parts.is_empty() || parts.contains(&"..") {
    return Err(ProposalError::Refusal(format!(
      "path must be store-relative (e.g. entries/<topic>/<slug>.md or topics): {}",
      pyfmt::str_repr(path)
    )));
  }
  Ok(parts.iter().collect())
}

/// The message file as the caller named it: `~` expanded, and a relative path read from the caller's working directory.
fn caller_path(path: &Path) -> ProposalResult<PathBuf> {
  std::path::absolute(paths::expanduser(&path.to_string_lossy()))
    .map_err(|error| ProposalError::Cli(CliExit::error(format!("resolve {}: {error}", path.display()))))
}

/// The text of `<rev>:<path>`, or `None` when the revision holds no such file. Git's output is read with universal
/// newlines, as the Python CLI reads it.
fn rev_text(memory_dir: &Path, rev: &str, path: &str) -> ProposalResult<Option<String>> {
  let listing = run_git(memory_dir, &["ls-tree", rev, "--", path])?;
  if !listing.success {
    return Err(ProposalError::Refusal(format!(
      "git ls-tree {rev} -- {path} failed in {}: {}",
      memory_dir.display(),
      listing.stderr.trim()
    )));
  }
  if listing.stdout.trim().is_empty() {
    return Ok(None);
  }
  let text = git(memory_dir, &["show", &format!("{rev}:{path}")])?;
  Ok(Some(store::normalize_newlines(&text)))
}

fn rev_line_counts(memory_dir: &Path, rev: &str, path: &str) -> ProposalResult<LineCounts> {
  Ok(match rev_text(memory_dir, rev, path)? {
    Some(text) => non_blank_counts(&text),
    None => LineCounts::new(),
  })
}

fn line_counts<'a>(lines: impl IntoIterator<Item = &'a str>) -> LineCounts {
  let mut counts = LineCounts::new();
  for line in lines {
    *counts.entry(line.to_string()).or_default() += 1;
  }
  counts
}

fn non_blank_counts(text: &str) -> LineCounts {
  line_counts(text.split('\n').filter(|line| !pystr::strip(line).is_empty()))
}

fn body_counts(text: &str) -> LineCounts {
  line_counts(store::entry_body(text))
}

/// The multiset difference `left - right`: each line keeps the occurrences that `right` does not cover.
fn counts_minus(left: &LineCounts, right: &LineCounts) -> LineCounts {
  left
    .iter()
    .filter_map(|(line, count)| {
      let remaining = count.saturating_sub(right.get(line).copied().unwrap_or_default());
      (remaining > 0).then(|| (line.clone(), remaining))
    })
    .collect()
}

/// Whether every line occurs in `right` at least as often as in `left`.
fn counts_within(left: &LineCounts, right: &LineCounts) -> bool {
  left.iter().all(|(line, count)| right.get(line).copied().unwrap_or_default() >= *count)
}

/// Each line repeated by its count, in sorted line order.
fn repeated_lines(counts: &LineCounts) -> Vec<String> {
  counts.iter().flat_map(|(line, count)| std::iter::repeat_n(line.clone(), *count)).collect()
}

/// Commit exactly one store-relative path on the proposal branch.
///
/// Every non-blank line the PR already added to the path must still be in the worktree's file, unless
/// `replace_pr_lines` lists the lost ones under `Replaced PR lines:`. The prose check then refuses a commit whose
/// added or rewritten text breaks the writing rules. The negation check never refuses: its hits come back as warnings
/// and land under `Negation warnings:`.
fn commit(memory_dir: &Path, path: &str, message_file: &Path, replace_pr_lines: bool) -> ProposalResult<Committed> {
  let relative = store_path(path)?;
  let message_file = caller_path(message_file)?;
  let worktree = proposal_worktree(memory_dir);
  if !worktree.exists() {
    return Err(ProposalError::Refusal(format!(
      "no proposal worktree at {}; run 'charliebot memory proposal open' first",
      worktree.display()
    )));
  }
  let base = base_branch(memory_dir)?;
  let added =
    counts_minus(&rev_line_counts(memory_dir, PROPOSAL_BRANCH, path)?, &rev_line_counts(memory_dir, &base, path)?);
  let current_file = worktree.join(&relative);
  let current_text =
    if current_file.is_file() { Some(store::read_text(&current_file).map_err(ProposalError::Cli)?) } else { None };
  let current = current_text.as_deref().map(non_blank_counts).unwrap_or_default();
  let replaced = repeated_lines(&counts_minus(&added, &current));
  if !replaced.is_empty() && !replace_pr_lines {
    let listed: Vec<String> = replaced.iter().map(|line| format!("  {line}")).collect();
    return Err(ProposalError::Refusal(format!(
      "{path}: lines the PR already added are gone from the worktree; restore them or resolve the conflict in the \
       report:\n{}",
      listed.join("\n")
    )));
  }
  let mut warnings = Vec::new();
  if let Some(text) = current_text.as_deref()
    && relative.as_path() != Path::new("topics")
  {
    let base_text = rev_text(memory_dir, &base, path)?;
    let head_body = rev_text(memory_dir, PROPOSAL_BRANCH, path)?.as_deref().map(body_counts).unwrap_or_default();
    // A pure deletion skips the body line limit: the worktree's body lines sit inside the proposal head's, so the
    // commit adds no body line. A new file has no head version, so it never skips.
    let pure_deletion = counts_within(&body_counts(text), &head_body);
    let violations = proposal_check::prose_violations(text, base_text.as_deref(), pure_deletion);
    if !violations.is_empty() {
      let listed: Vec<String> = violations.iter().map(|violation| format!("  {violation}")).collect();
      return Err(ProposalError::Refusal(format!("{path}: prose check failed:\n{}", listed.join("\n"))));
    }
    warnings = proposal_check::negation_warnings(text, base_text.as_deref());
  }
  let scratch = tempfile::tempdir()
    .map_err(|error| ProposalError::Cli(CliExit::error(format!("create the commit message directory: {error}"))))?;
  let commit_message = scratch.path().join("message");
  if replaced.is_empty() && warnings.is_empty() {
    fs::copy(&message_file, &commit_message).map_err(|error| {
      ProposalError::Cli(CliExit::error(format!("copy {} to the commit message: {error}", message_file.display())))
    })?;
  } else {
    let mut message = store::read_text(&message_file).map_err(ProposalError::Cli)?;
    if !message.is_empty() && !message.ends_with('\n') {
      message.push('\n');
    }
    if !replaced.is_empty() {
      message.push_str("Replaced PR lines:\n");
      for line in &replaced {
        message.push_str(&format!("  {line}\n"));
      }
    }
    if !warnings.is_empty() {
      message.push_str("Negation warnings:\n");
      for warning in &warnings {
        message.push_str(&format!("  {warning}\n"));
      }
    }
    fs::write(&commit_message, message)
      .map_err(|error| ProposalError::Cli(CliExit::error(format!("write {}: {error}", commit_message.display()))))?;
  }
  let commit_message_path = commit_message
    .to_str()
    .ok_or_else(|| ProposalError::Cli(CliExit::error("the commit message path is not UTF-8")))?;
  git(&worktree, &["add", "--", path])?;
  let committing = run_git(&worktree, &["commit", "--only", "-F", commit_message_path, "--", path])?;
  if !committing.success {
    return Err(ProposalError::Refusal(format!("committing {path} failed: {}", committing.stderr.trim())));
  }
  let sha = git(&worktree, &["rev-parse", "HEAD"])?;
  Ok(Committed { sha: sha.trim().to_string(), warnings })
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
