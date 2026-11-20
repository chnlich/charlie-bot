//! The leak gate the improve round and the repository's git hooks run:
//! `dev/improve/github-post.sh`, `dev/check-skills-host-leak.sh`, `dev/leak-patterns.sh` and
//! `dev/git-hooks/`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use charliebot_shell_harness::{self as harness, Env, Run, Sandbox};

const HOME_PATH_TEXT: &str = "the notes live under /home/someone/notes";
const TAILNET_TEXT: &str = "measured on example.ts.net";
const PATTERN_TERM: &str = "fictionalmarker";
const PATTERN_FILE_COMMENT: &str = "# local leak patterns";
const CLEAN_BODY: &str = "## Evidence\nbefore 259 ms, after 130 ms (n=50).\n";
const PR_URL: &str = "https://github.com/chnlich/charlie-bot/pull/5046";
const PATCH_PREFIX: &str = "api -X PATCH repos/chnlich/charlie-bot/pulls/5046";

fn github_post() -> PathBuf {
  harness::repo_root().join("dev/improve/github-post.sh")
}

fn leak_scanner() -> PathBuf {
  harness::repo_root().join("dev/check-skills-host-leak.sh")
}

/// The github-post environment: the `gh` stub records its calls and answers `gh pr view` with the PR's URL,
/// the profile home carries the pattern file, and git identity and config are pinned for the pushes.
fn post_env(sandbox: &Sandbox, home: &Path, gh_log: &Path) -> Env {
  sandbox.stub(
    "gh",
    &format!(
      r#"case "$1 $2" in 'pr view') printf '%s\n' '{PR_URL}' ;; esac
printf '%s\n' "$*" >> "$GH_CALL_LOG"
"#
    ),
  );
  let mut env = sandbox.env();
  env.insert("HOME".to_string(), sandbox.root().display().to_string());
  env.insert("CHARLIEBOT_HOME".to_string(), home.display().to_string());
  env.insert("GH_CALL_LOG".to_string(), gh_log.display().to_string());
  env.insert("GIT_CONFIG_NOSYSTEM".to_string(), "1".to_string());
  env.insert("GIT_CONFIG_GLOBAL".to_string(), "/dev/null".to_string());
  for name in ["GIT_AUTHOR_NAME", "GIT_COMMITTER_NAME"] {
    env.insert(name.to_string(), "Example".to_string());
  }
  for name in ["GIT_AUTHOR_EMAIL", "GIT_COMMITTER_EMAIL"] {
    env.insert(name.to_string(), "123+example@users.noreply.github.com".to_string());
  }
  env
}

/// The profile home with one local pattern term in its pattern file.
fn pattern_home(sandbox: &Sandbox) -> PathBuf {
  let home = sandbox.dir("charliebot-home");
  std::fs::write(home.join("skills_leak_patterns.local.txt"), format!("{PATTERN_FILE_COMMENT}\n{PATTERN_TERM}\n"))
    .unwrap();
  home
}

fn run(command: &mut Command) -> Run {
  harness::run(command)
}

fn run_in(command: &mut Command, cwd: &Path) -> Run {
  command.current_dir(cwd);
  run(command)
}

fn run_with_stdin(command: &mut Command, cwd: &Path, input: &str) -> Run {
  command.current_dir(cwd).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
  let mut child = harness::spawn(command);
  child.stdin.as_mut().expect("piped stdin").write_all(input.as_bytes()).expect("write stdin");
  let output = child.wait_with_output().expect("wait for the scanner");
  Run {
    code: output.status.code().unwrap_or(-1),
    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
  }
}

fn git(env: &Env, cwd: &Path, args: &[&str]) -> String {
  let mut command = harness::command("git", env);
  command.args(args);
  let run = run_in(&mut command, cwd);
  assert_eq!(run.code, 0, "git {args:?}: {}", run.describe());
  run.stdout.trim().to_string()
}

fn gh_log_lines(gh_log: &Path) -> Vec<String> {
  std::fs::read_to_string(gh_log).unwrap_or_default().lines().map(str::to_string).collect()
}

/// A work clone with a bare repository as origin, one commit ahead of origin/main on
/// `improve/perf/topic-slug`.
fn push_repo(sandbox: &Sandbox, env: &Env) -> PathBuf {
  let origin = sandbox.dir("origin.git");
  let clone = sandbox.dir("clone");
  git(env, sandbox.root(), &["init", "--bare", "-q", origin.to_str().unwrap()]);
  git(env, sandbox.root(), &["init", "-q", "-b", "main", clone.to_str().unwrap()]);
  git(env, &clone, &["remote", "add", "origin", origin.to_str().unwrap()]);
  std::fs::write(clone.join("seed.txt"), "seed\n").unwrap();
  git(env, &clone, &["add", "seed.txt"]);
  git(env, &clone, &["commit", "-q", "-m", "seed commit"]);
  git(env, &clone, &["push", "-q", "origin", "main"]);
  git(env, &clone, &["switch", "-c", "improve/perf/topic-slug"]);
  std::fs::write(clone.join("change.txt"), "change\n").unwrap();
  git(env, &clone, &["add", "change.txt"]);
  git(env, &clone, &["commit", "-q", "-m", "topic change"]);
  clone
}

fn ls_remote(env: &Env, clone: &Path) -> String {
  git(env, clone, &["ls-remote", "origin"])
}

fn body_file(sandbox: &Sandbox, name: &str, text: &str) -> PathBuf {
  let path = sandbox.root().join(name);
  std::fs::write(&path, text).unwrap();
  path
}

// -- github-post.sh --

fn post(sandbox: &Sandbox) -> (Env, PathBuf, PathBuf) {
  let home = pattern_home(sandbox);
  let gh_log = sandbox.root().join("gh-calls.log");
  let env = post_env(sandbox, &home, &gh_log);
  (env, gh_log, home)
}

#[test]
#[ignore = "integration"]
fn a_pr_comment_refuses_a_home_path() {
  let sandbox = Sandbox::new();
  let (env, gh_log, _home) = post(&sandbox);
  let body = body_file(&sandbox, "body.md", &format!("## Evidence\n{HOME_PATH_TEXT}\n"));
  let mut command = harness::command("bash", &env);
  command.arg(github_post()).args(["pr-comment", "--body-file"]).arg(&body).arg("123");
  let run = run_in(&mut command, sandbox.root());
  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(run.stderr.contains("body"), "{}", run.stderr);
  assert!(gh_log_lines(&gh_log).is_empty());
}

#[test]
#[ignore = "integration"]
fn a_pr_comment_refuses_a_tailnet_host() {
  let sandbox = Sandbox::new();
  let (env, gh_log, _home) = post(&sandbox);
  let body = body_file(&sandbox, "body.md", &format!("## Evidence\n{TAILNET_TEXT}\n"));
  let mut command = harness::command("bash", &env);
  command.arg(github_post()).args(["pr-comment", "--body-file"]).arg(&body).arg("123");
  let run = run_in(&mut command, sandbox.root());
  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(gh_log_lines(&gh_log).is_empty());
}

#[test]
#[ignore = "integration"]
fn a_pr_comment_refuses_a_pattern_file_term() {
  let sandbox = Sandbox::new();
  let (env, gh_log, _home) = post(&sandbox);
  let body = body_file(&sandbox, "body.md", &format!("## Evidence\nthe {PATTERN_TERM} value\n"));
  let mut command = harness::command("bash", &env);
  command.arg(github_post()).args(["pr-comment", "--body-file"]).arg(&body).arg("123");
  let run = run_in(&mut command, sandbox.root());
  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(gh_log_lines(&gh_log).is_empty());
}

#[test]
#[ignore = "integration"]
fn a_pr_comment_sends_a_clean_body() {
  let sandbox = Sandbox::new();
  let (env, gh_log, _home) = post(&sandbox);
  let body = body_file(&sandbox, "body.md", CLEAN_BODY);
  let mut command = harness::command("bash", &env);
  command.arg(github_post()).args(["pr-comment", "--body-file"]).arg(&body).arg("123");
  let run = run_in(&mut command, sandbox.root());
  assert_eq!(run.code, 0, "{}", run.describe());
  let lines = gh_log_lines(&gh_log);
  assert_eq!(lines.len(), 1, "{lines:?}");
  assert!(lines[0].contains("pr comment"), "{}", lines[0]);
  assert!(lines[0].contains("123"), "{}", lines[0]);
}

#[test]
#[ignore = "integration"]
fn a_missing_pattern_file_exits_2() {
  let sandbox = Sandbox::new();
  let (env, gh_log, home) = post(&sandbox);
  std::fs::remove_file(home.join("skills_leak_patterns.local.txt")).unwrap();
  let body = body_file(&sandbox, "body.md", CLEAN_BODY);
  let mut command = harness::command("bash", &env);
  command.arg(github_post()).args(["pr-comment", "--body-file"]).arg(&body).arg("123");
  let run = run_in(&mut command, sandbox.root());
  assert_eq!(run.code, 2, "{}", run.describe());
  assert!(gh_log_lines(&gh_log).is_empty());
}

#[test]
#[ignore = "integration"]
fn a_pr_create_checks_title_and_body() {
  let sandbox = Sandbox::new();
  let (env, gh_log, _home) = post(&sandbox);
  let body = body_file(&sandbox, "body.md", CLEAN_BODY);

  let mut command = harness::command("bash", &env);
  command
    .arg(github_post())
    .args(["pr-create", "--title"])
    .arg(format!("fix {HOME_PATH_TEXT}"))
    .args(["--body-file"])
    .arg(&body)
    .args(["--base", "main"]);
  let run = run_in(&mut command, sandbox.root());
  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(run.stderr.contains("title"), "{}", run.stderr);
  assert!(gh_log_lines(&gh_log).is_empty());

  let mut command = harness::command("bash", &env);
  command
    .arg(github_post())
    .args(["pr-create", "--title", "clean topic", "--body-file"])
    .arg(&body)
    .args(["--base", "main"]);
  let run = run_in(&mut command, sandbox.root());
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(gh_log_lines(&gh_log)[0].contains("pr create"), "{}", gh_log_lines(&gh_log)[0]);
}

#[test]
#[ignore = "integration"]
fn a_pr_close_sends_the_comment_from_its_file() {
  let sandbox = Sandbox::new();
  let (env, gh_log, _home) = post(&sandbox);
  let comment = body_file(&sandbox, "comment.txt", "improve-abandoned: topic — reason\n");
  let mut command = harness::command("bash", &env);
  command.arg(github_post()).args(["pr-close", "--comment-file"]).arg(&comment).args(["--delete-branch", "123"]);
  let run = run_in(&mut command, sandbox.root());
  assert_eq!(run.code, 0, "{}", run.describe());
  let line = &gh_log_lines(&gh_log)[0];
  assert!(line.contains("pr close"), "{line}");
  assert!(line.contains("--delete-branch"), "{line}");
  assert!(line.contains("--comment"), "{line}");
  assert!(line.contains("improve-abandoned: topic"), "{line}");
}

#[test]
#[ignore = "integration"]
fn inline_text_flags_are_refused() {
  let sandbox = Sandbox::new();
  let (env, gh_log, _home) = post(&sandbox);
  let body = body_file(&sandbox, "body.md", CLEAN_BODY);

  let mut inline_edit = harness::command("bash", &env);
  inline_edit.arg(github_post()).args(["pr-edit", "--title", "clean topic", "--body"]).arg(HOME_PATH_TEXT).arg("123");
  assert_eq!(run_in(&mut inline_edit, sandbox.root()).code, 2);

  let mut inline_comment = harness::command("bash", &env);
  inline_comment
    .arg(github_post())
    .args(["pr-comment", "--body-file"])
    .arg(&body)
    .arg(format!("--body={TAILNET_TEXT}"))
    .arg("123");
  assert_eq!(run_in(&mut inline_comment, sandbox.root()).code, 2);

  assert!(gh_log_lines(&gh_log).is_empty());
}

// -- github-post.sh pr-edit --

/// The REST PATCH calls the `gh` stub logged, one line per call.
fn patch_lines(gh_log: &Path) -> Vec<String> {
  gh_log_lines(gh_log).into_iter().filter(|line| line.starts_with("api -X PATCH")).collect()
}

#[test]
#[ignore = "integration"]
fn a_pr_edit_sends_the_title_alone() {
  let sandbox = Sandbox::new();
  let (env, gh_log, _home) = post(&sandbox);
  let mut command = harness::command("bash", &env);
  command.arg(github_post()).args(["pr-edit", "--title", "clean topic", "5046"]);
  let run = run_in(&mut command, sandbox.root());
  assert_eq!(run.code, 0, "{}", run.describe());
  assert_eq!(patch_lines(&gh_log), [format!("{PATCH_PREFIX} -f title=clean topic")]);
}

#[test]
#[ignore = "integration"]
fn a_pr_edit_sends_the_body_alone() {
  let sandbox = Sandbox::new();
  let (env, gh_log, _home) = post(&sandbox);
  let body = body_file(&sandbox, "body.md", CLEAN_BODY);
  let mut command = harness::command("bash", &env);
  command.arg(github_post()).args(["pr-edit", "--body-file"]).arg(&body).arg("5046");
  let run = run_in(&mut command, sandbox.root());
  assert_eq!(run.code, 0, "{}", run.describe());
  assert_eq!(patch_lines(&gh_log), [format!("{PATCH_PREFIX} -F body=@{}", body.display())]);
}

#[test]
#[ignore = "integration"]
fn a_pr_edit_sends_the_title_and_the_body_to_the_repo_it_names() {
  let sandbox = Sandbox::new();
  let (env, gh_log, _home) = post(&sandbox);
  let body = body_file(&sandbox, "body.md", CLEAN_BODY);
  let mut command = harness::command("bash", &env);
  command.arg(github_post()).args(["pr-edit", "--title", "clean topic", "--body-file"]).arg(&body).args([
    "--repo",
    "chnlich/charlie-bot",
    "5046",
  ]);
  let run = run_in(&mut command, sandbox.root());
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(
    gh_log_lines(&gh_log).contains(&"pr view 5046 --repo chnlich/charlie-bot --json url --jq .url".to_string()),
    "{:?}",
    gh_log_lines(&gh_log)
  );
  assert_eq!(patch_lines(&gh_log), [format!("{PATCH_PREFIX} -f title=clean topic -F body=@{}", body.display())]);
}

#[test]
#[ignore = "integration"]
fn a_pr_edit_leak_hit_sends_nothing() {
  let sandbox = Sandbox::new();
  let (env, gh_log, _home) = post(&sandbox);

  let mut title_hit = harness::command("bash", &env);
  title_hit.arg(github_post()).args(["pr-edit", "--title"]).arg(format!("fix {HOME_PATH_TEXT}")).arg("5046");
  let run = run_in(&mut title_hit, sandbox.root());
  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(run.stderr.contains("title"), "{}", run.stderr);

  let leaky_body = body_file(&sandbox, "leaky.md", &format!("## Evidence\nthe {PATTERN_TERM} value\n"));
  let mut body_hit = harness::command("bash", &env);
  body_hit.arg(github_post()).args(["pr-edit", "--body-file"]).arg(&leaky_body).arg("5046");
  let run = run_in(&mut body_hit, sandbox.root());
  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(run.stderr.contains("body"), "{}", run.stderr);

  assert!(gh_log_lines(&gh_log).is_empty());
}

#[test]
#[ignore = "integration"]
fn a_pr_edit_refuses_an_argument_it_does_not_send() {
  let sandbox = Sandbox::new();
  let (env, gh_log, _home) = post(&sandbox);
  let mut command = harness::command("bash", &env);
  command.arg(github_post()).args(["pr-edit", "--title", "clean topic", "--add-label", "bug", "5046"]);
  let run = run_in(&mut command, sandbox.root());
  assert_eq!(run.code, 2, "{}", run.describe());
  assert!(run.stderr.contains("--add-label"), "{}", run.stderr);
  assert!(gh_log_lines(&gh_log).is_empty());
}

#[test]
#[ignore = "integration"]
fn a_push_refuses_a_leaky_commit_message() {
  let sandbox = Sandbox::new();
  let (env, _gh_log, _home) = post(&sandbox);
  let clone = push_repo(&sandbox, &env);
  std::fs::write(clone.join("bad.txt"), "bad\n").unwrap();
  git(&env, &clone, &["add", "bad.txt"]);
  git(&env, &clone, &["commit", "-q", "-m", &format!("follow {HOME_PATH_TEXT}")]);

  let mut command = harness::command("bash", &env);
  command.arg(github_post()).args(["push", "-u", "origin", "improve/perf/topic-slug"]);
  let run = run_in(&mut command, &clone);
  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(run.stderr.contains("commit"), "{}", run.stderr);
  assert!(!ls_remote(&env, &clone).contains("improve/perf/topic-slug"));
}

#[test]
#[ignore = "integration"]
fn a_push_refuses_a_leaky_branch_name() {
  let sandbox = Sandbox::new();
  let (env, _gh_log, _home) = post(&sandbox);
  let clone = push_repo(&sandbox, &env);
  let branch = format!("improve/perf/{PATTERN_TERM}-topic");
  git(&env, &clone, &["switch", "-c", &branch]);
  std::fs::write(clone.join("change2.txt"), "change2\n").unwrap();
  git(&env, &clone, &["add", "change2.txt"]);
  git(&env, &clone, &["commit", "-q", "-m", "topic change two"]);

  let mut command = harness::command("bash", &env);
  command.arg(github_post()).args(["push", "-u", "origin"]).arg(&branch);
  let run = run_in(&mut command, &clone);
  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(run.stderr.contains("branch name"), "{}", run.stderr);
  assert!(!ls_remote(&env, &clone).contains(&branch));
}

#[test]
#[ignore = "integration"]
fn a_push_refuses_when_origin_main_is_unknown() {
  let sandbox = Sandbox::new();
  let (env, _gh_log, _home) = post(&sandbox);
  let clone = push_repo(&sandbox, &env);
  git(&env, &clone, &["update-ref", "-d", "refs/remotes/origin/main"]);
  std::fs::write(clone.join("bad.txt"), "bad\n").unwrap();
  git(&env, &clone, &["add", "bad.txt"]);
  git(&env, &clone, &["commit", "-q", "-m", &format!("follow {HOME_PATH_TEXT}")]);

  let mut command = harness::command("bash", &env);
  command.arg(github_post()).args(["push", "-u", "origin", "improve/perf/topic-slug"]);
  let run = run_in(&mut command, &clone);
  assert_eq!(run.code, 2, "{}", run.describe());
  assert!(!ls_remote(&env, &clone).contains("improve/perf/topic-slug"));
}

#[test]
#[ignore = "integration"]
fn a_push_updates_the_bare_repository() {
  let sandbox = Sandbox::new();
  let (env, _gh_log, _home) = post(&sandbox);
  let clone = push_repo(&sandbox, &env);

  let mut command = harness::command("bash", &env);
  command.arg(github_post()).args(["push", "-u", "origin", "improve/perf/topic-slug"]);
  let run = run_in(&mut command, &clone);
  assert_eq!(run.code, 0, "{}", run.describe());

  let head = git(&env, &clone, &["rev-parse", "HEAD"]);
  let refs = git(&env, &clone, &["ls-remote", "origin", "refs/heads/improve/perf/topic-slug"]);
  assert!(refs.contains(&head), "{refs}");
}

#[test]
#[ignore = "integration"]
fn a_tilde_profile_home_names_the_pattern_file_under_home() {
  let sandbox = Sandbox::new();
  let (mut env, gh_log, _home) = post(&sandbox);
  // The server's home rule expands a leading `~` to HOME. The pattern file sits at `<root>/charliebot-home`.
  env.insert("CHARLIEBOT_HOME".to_string(), "~/charliebot-home".to_string());
  let body = body_file(&sandbox, "body.md", &format!("## Evidence\nthe {PATTERN_TERM} value\n"));
  let mut command = harness::command("bash", &env);
  command.arg(github_post()).args(["pr-comment", "--body-file"]).arg(&body).arg("123");
  let run = run_in(&mut command, sandbox.root());
  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(gh_log_lines(&gh_log).is_empty());

  env.remove("EXTRA_PATTERNS_FILE");
  let mut scan = harness::command("bash", &env);
  scan.arg(leak_scanner()).arg("--stdin");
  let scan = run_with_stdin(&mut scan, sandbox.root(), &format!("the {PATTERN_TERM} value\n"));
  assert_eq!(scan.code, 1, "{}", scan.describe());
}

#[test]
#[ignore = "integration"]
fn the_stdin_mode_exit_codes() {
  let sandbox = Sandbox::new();
  let (mut env, _gh_log, home) = post(&sandbox);
  env.insert("EXTRA_PATTERNS_FILE".to_string(), home.join("skills_leak_patterns.local.txt").display().to_string());

  let mut hit = harness::command("bash", &env);
  hit.arg(leak_scanner()).arg("--stdin");
  let hit = run_with_stdin(&mut hit, sandbox.root(), &format!("{HOME_PATH_TEXT}\n"));
  assert_eq!(hit.code, 1, "{}", hit.describe());
  assert!(hit.stdout.contains("1:"), "{}", hit.stdout);

  let mut term = harness::command("bash", &env);
  term.arg(leak_scanner()).arg("--stdin");
  let term = run_with_stdin(&mut term, sandbox.root(), &format!("the {PATTERN_TERM} value\n"));
  assert_eq!(term.code, 1, "{}", term.describe());
  assert!(!term.stdout.contains(PATTERN_FILE_COMMENT), "{}", term.stdout);

  let mut clean = harness::command("bash", &env);
  clean.arg(leak_scanner()).arg("--stdin");
  let clean = run_with_stdin(&mut clean, sandbox.root(), "clean text\n");
  assert_eq!(clean.code, 0, "{}", clean.describe());
}

// -- the git hooks and the host scan --

/// The leak tools and the two hooks copied into `checkout`, at the paths a real checkout uses.
fn copy_leak_tools(checkout: &Path) {
  let dev = checkout.join("dev");
  std::fs::create_dir_all(dev.join("git-hooks")).unwrap();
  harness::copy_file(&leak_scanner(), &dev.join("check-skills-host-leak.sh"));
  harness::copy_file(&harness::repo_root().join("dev/leak-patterns.sh"), &dev.join("leak-patterns.sh"));
  for hook in ["pre-commit", "commit-msg"] {
    harness::copy_file(&harness::repo_root().join("dev/git-hooks").join(hook), &dev.join("git-hooks").join(hook));
  }
}

/// The git variables that keep a test's commits independent of the host configuration and identity.
fn pinned_git_env(sandbox: &Sandbox) -> Env {
  let mut env = sandbox.env();
  env.insert("HOME".to_string(), sandbox.root().display().to_string());
  env.insert("GIT_CONFIG_NOSYSTEM".to_string(), "1".to_string());
  env.insert("GIT_CONFIG_GLOBAL".to_string(), "/dev/null".to_string());
  for name in ["GIT_AUTHOR_NAME", "GIT_COMMITTER_NAME"] {
    env.insert(name.to_string(), "Example".to_string());
  }
  for name in ["GIT_AUTHOR_EMAIL", "GIT_COMMITTER_EMAIL"] {
    env.insert(name.to_string(), "123+example@users.noreply.github.com".to_string());
  }
  env
}

/// A scratch repository carrying a copy of the leak tools and the hooks path installed.
fn leak_repo(sandbox: &Sandbox) -> (PathBuf, Env) {
  let repo = sandbox.dir("leak-repo");
  copy_leak_tools(&repo);
  let patterns = sandbox.dir("patterns");
  std::fs::write(patterns.join("secretmarker.txt"), "secretmarker\n").unwrap();

  let mut env = pinned_git_env(sandbox);
  env.insert("EXTRA_PATTERNS_FILE".to_string(), patterns.join("secretmarker.txt").display().to_string());
  git(&env, sandbox.root(), &["init", "-q", "-b", "main", repo.to_str().unwrap()]);
  git(&env, &repo, &["config", "core.hooksPath", "dev/git-hooks"]);
  (repo, env)
}

/// A repository to commit to whose `core.hooksPath` is an absolute path into a second checkout that holds the
/// leak tools and the hooks. The repository itself has no `dev/` directory.
fn cross_checkout_repo(sandbox: &Sandbox) -> (PathBuf, Env) {
  let hooks_checkout = sandbox.dir("hooks-checkout");
  copy_leak_tools(&hooks_checkout);
  let repo = sandbox.dir("committer-repo");
  let patterns = sandbox.dir("patterns");
  std::fs::write(patterns.join("secretmarker.txt"), "secretmarker\n").unwrap();

  let mut env = pinned_git_env(sandbox);
  env.insert("CHARLIEBOT_HOME".to_string(), sandbox.dir("committer-home").display().to_string());
  env.insert("EXTRA_PATTERNS_FILE".to_string(), patterns.join("secretmarker.txt").display().to_string());
  git(&env, sandbox.root(), &["init", "-q", "-b", "main", repo.to_str().unwrap()]);
  git(&env, &repo, &["config", "core.hooksPath", hooks_checkout.join("dev/git-hooks").to_str().unwrap()]);
  (repo, env)
}

fn leak_run(repo: &Path, env: &Env, args: &[&str]) -> Run {
  let mut command = harness::command("git", env);
  command.args(args);
  run_in(&mut command, repo)
}

#[test]
#[ignore = "integration"]
fn the_hooks_check_staged_bytes_and_commit_fields() {
  let sandbox = Sandbox::new();
  let (repo, env) = leak_repo(&sandbox);

  std::fs::write(repo.join("ordinary.bin"), b"\0SECRETmarker\0").unwrap();
  assert_eq!(leak_run(&repo, &env, &["add", "ordinary.bin"]).code, 0);
  std::fs::write(repo.join("ordinary.bin"), b"clean").unwrap();
  let commit = leak_run(&repo, &env, &["commit", "-m", "clean message"]);
  assert_ne!(commit.code, 0, "{}", commit.describe());
  assert!(commit.stderr.contains("ordinary.bin"), "{}", commit.stderr);

  assert_eq!(leak_run(&repo, &env, &["add", "ordinary.bin"]).code, 0);
  let commit = leak_run(&repo, &env, &["commit", "-m", "SECRETmarker"]);
  assert_ne!(commit.code, 0, "{}", commit.describe());
  assert!(commit.stderr.contains("commit message"), "{}", commit.stderr);
  assert!(!commit.stderr.contains("SECRETmarker"), "{}", commit.stderr);

  let mut bad_env = env.clone();
  bad_env.insert("GIT_AUTHOR_EMAIL".to_string(), "secretmarker@example.invalid".to_string());
  let commit = leak_run(&repo, &bad_env, &["commit", "-m", "clean message"]);
  assert_ne!(commit.code, 0, "{}", commit.describe());
  assert!(commit.stderr.contains("author identity"), "{}", commit.stderr);
  assert!(!commit.stderr.contains("secretmarker@example.invalid"), "{}", commit.stderr);

  let mut bad_env = env.clone();
  bad_env.insert("GIT_COMMITTER_EMAIL".to_string(), "secretmarker@example.invalid".to_string());
  let commit = leak_run(&repo, &bad_env, &["commit", "-m", "clean message"]);
  assert_ne!(commit.code, 0, "{}", commit.describe());
  assert!(commit.stderr.contains("committer identity"), "{}", commit.stderr);

  assert_eq!(leak_run(&repo, &env, &["commit", "-m", "clean message"]).code, 0);
}

#[test]
#[ignore = "integration"]
fn the_installed_checkouts_hooks_check_the_committers_tree() {
  let sandbox = Sandbox::new();
  let (repo, env) = cross_checkout_repo(&sandbox);

  std::fs::write(repo.join("leaked.txt"), "secretmarker\n").unwrap();
  assert_eq!(leak_run(&repo, &env, &["add", "leaked.txt"]).code, 0);
  let commit = leak_run(&repo, &env, &["commit", "-m", "add leaked file"]);
  assert_ne!(commit.code, 0, "{}", commit.describe());
  assert!(commit.stderr.contains("leaked.txt"), "{}", commit.stderr);

  assert_eq!(leak_run(&repo, &env, &["reset", "-q"]).code, 0);
  std::fs::remove_file(repo.join("leaked.txt")).unwrap();
  std::fs::write(repo.join("clean.txt"), "clean\n").unwrap();
  assert_eq!(leak_run(&repo, &env, &["add", "clean.txt"]).code, 0);
  let commit = leak_run(&repo, &env, &["commit", "-m", "add clean file"]);
  assert_eq!(commit.code, 0, "{}", commit.describe());

  std::fs::write(repo.join("clean2.txt"), "clean\n").unwrap();
  assert_eq!(leak_run(&repo, &env, &["add", "clean2.txt"]).code, 0);
  let commit = leak_run(&repo, &env, &["commit", "-m", "note secretmarker"]);
  assert_ne!(commit.code, 0, "{}", commit.describe());
  assert!(commit.stderr.contains("commit message"), "{}", commit.stderr);

  assert_eq!(leak_run(&repo, &env, &["reset", "-q"]).code, 0);
  std::fs::remove_file(repo.join("clean2.txt")).unwrap();
  std::fs::create_dir_all(repo.join("web/features/demo")).unwrap();
  std::fs::write(repo.join("web/features/demo/leak_scan_files.txt"), "notes.txt\n").unwrap();
  std::fs::write(repo.join("notes.txt"), format!("{HOME_PATH_TEXT}\n")).unwrap();
  let mut builtin_env = env.clone();
  builtin_env.remove("EXTRA_PATTERNS_FILE");
  assert_eq!(leak_run(&repo, &builtin_env, &["add", "notes.txt", "web/features/demo/leak_scan_files.txt"]).code, 0);
  let commit = leak_run(&repo, &builtin_env, &["commit", "-m", "add notes"]);
  assert_ne!(commit.code, 0, "{}", commit.describe());
  assert!(commit.stderr.contains("notes.txt"), "{}", commit.stderr);
}

#[test]
#[ignore = "integration"]
fn the_host_scan_covers_tracked_gzip_and_explicit_files() {
  let sandbox = Sandbox::new();
  let (repo, env) = leak_repo(&sandbox);
  let fixture = repo.join("fixture.gz");

  let mut command = harness::command("bash", &env);
  command.args(["-c", "printf 'private SECRETmarker content' | gzip -n > fixture.gz"]);
  run_in(&mut command, &repo);
  assert_eq!(leak_run(&repo, &env, &["add", "fixture.gz"]).code, 0);

  for args in [&[][..], &["fixture.gz"][..]] {
    let mut command = harness::command("bash", &env);
    command.arg(tools_scanner(&repo));
    command.args(args);
    let run = run_in(&mut command, &repo);
    assert_ne!(run.code, 0, "{}", run.describe());
  }

  std::fs::write(&fixture, b"bad gzip").unwrap();
  let mut command = harness::command("bash", &env);
  command.arg(tools_scanner(&repo)).arg("fixture.gz");
  let run = run_in(&mut command, &repo);
  assert_ne!(run.code, 0, "{}", run.describe());
}

fn tools_scanner(repo: &Path) -> PathBuf {
  repo.join("dev/check-skills-host-leak.sh")
}

// -- github-post.sh pr-merge --

/// The `gh` stand-in for `pr-merge`: it answers the diff and view reads from files the test writes, records every call,
/// and copies the squash message's `--body-file` aside so a test can read what reached the merge.
const MERGE_GH_STUB: &str = r#"case "$1 $2" in
  'pr diff')
    cat "$GH_CHANGED_PATHS"
    ;;
  'pr view')
    case " $* " in
      *" title "*) cat "$GH_PR_TITLE" ;;
      *" body "*) cat "$GH_PR_BODY" ;;
      *) printf '%s\n' 'https://github.com/chnlich/charlie-bot/pull/5046' ;;
    esac
    ;;
esac
printf '%s\n' "$*" >> "$GH_CALL_LOG"
if [[ "$1 $2" == 'pr merge' ]]; then
  while [[ $# -gt 0 ]]; do
    if [[ "$1" == --body-file ]]; then cp "$2" "$GH_MERGE_BODY"; fi
    shift
  done
fi
"#;

/// One synthetic pull request for `pr-merge`: the changed paths, the title and body, the call log and the copied squash
/// message, plus an evidence directory the caller fills with screenshots.
struct MergeWorld {
  sandbox: Sandbox,
  env: Env,
  gh_log: PathBuf,
  merge_body: PathBuf,
  evidence: PathBuf,
}

impl MergeWorld {
  fn new(changed: &str, pr_body: &str) -> MergeWorld {
    let sandbox = Sandbox::new();
    let home = pattern_home(&sandbox);
    let gh_log = sandbox.root().join("gh-calls.log");
    let env = post_env(&sandbox, &home, &gh_log);
    sandbox.stub("gh", MERGE_GH_STUB);
    let binary = harness::built_binary("charliebot").display().to_string();
    assert!(!binary.contains('\''), "the binary path carries no quote: {binary}");
    sandbox.stub("charliebot", &format!("exec '{binary}' \"$@\"\n"));
    let changed_paths = sandbox.root().join("changed-paths");
    let pr_title = sandbox.root().join("pr-title");
    let pr_body_file = sandbox.root().join("pr-body");
    let merge_body = sandbox.root().join("merge-body");
    std::fs::write(&changed_paths, changed).unwrap();
    std::fs::write(&pr_title, "Improve the status panel\n").unwrap();
    std::fs::write(&pr_body_file, pr_body).unwrap();
    let mut env = env;
    env.insert("GH_CHANGED_PATHS".to_string(), changed_paths.display().to_string());
    env.insert("GH_PR_TITLE".to_string(), pr_title.display().to_string());
    env.insert("GH_PR_BODY".to_string(), pr_body_file.display().to_string());
    env.insert("GH_MERGE_BODY".to_string(), merge_body.display().to_string());
    let evidence = sandbox.dir("evidence");
    MergeWorld { sandbox, env, gh_log, merge_body, evidence }
  }

  /// Writes `count` PNGs at each window size into the evidence directory.
  fn write_screenshots(&self, count: usize) {
    for (width, height) in charliebot_tasks::web_screenshots::WINDOW_SIZES {
      for copy in 0..count {
        let name = format!("shot-{width}-{height}-{copy}.png");
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        std::fs::write(self.evidence.join(name), bytes).unwrap();
      }
    }
  }

  fn merge(&self, args: &[&str]) -> Run {
    let mut command = harness::command("bash", &self.env);
    command.arg(github_post()).arg("pr-merge").args(args);
    run_in(&mut command, self.sandbox.root())
  }

  fn merge_calls(&self) -> Vec<String> {
    gh_log_lines(&self.gh_log).into_iter().filter(|line| line.starts_with("pr merge")).collect()
  }
}

#[test]
#[ignore = "integration"]
fn a_pr_merge_refuses_a_web_page_change_whose_evidence_lacks_a_window_size() {
  let world = MergeWorld::new("web/static/ui.js\nrust/crates/x/src/lib.rs\n", "View: /?session=abc\n");
  world.write_screenshots(1);

  let run = world.merge(&["--evidence-dir", world.evidence.to_str().unwrap(), "5046"]);

  assert_eq!(run.code, 2, "{}", run.describe());
  assert!(run.stderr.contains("screenshot"), "{}", run.stderr);
  assert!(world.merge_calls().is_empty(), "nothing may merge: {}\n{}", world.merge_calls().join("\n"), run.describe());
  assert!(!world.merge_body.exists(), "no squash message was written");
}

#[test]
#[ignore = "integration"]
fn a_pr_merge_refuses_a_web_page_change_without_a_view_line() {
  let world = MergeWorld::new("web/static/ui.js\n", "## Evidence\nbefore and after.\n");
  world.write_screenshots(2);

  let run = world.merge(&["--evidence-dir", world.evidence.to_str().unwrap(), "5046"]);

  assert_eq!(run.code, 2, "{}", run.describe());
  assert!(run.stderr.contains("View:"), "{}", run.stderr);
  assert!(world.merge_calls().is_empty(), "nothing may merge: {}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_pr_merge_of_a_non_web_change_needs_no_evidence_directory() {
  let world = MergeWorld::new("rust/crates/x/src/lib.rs\n", "## Evidence\nno screenshot needed.\n");

  let run = world.merge(&["5046"]);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert_eq!(world.merge_calls(), ["pr merge --squash 5046"], "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_pr_merge_carries_the_view_line_into_the_squash_message() {
  let world = MergeWorld::new("web/static/ui.js\n", "## Evidence\nbefore and after.\n\nView: /?session=abc\n");
  world.write_screenshots(2);

  let run = world.merge(&["--evidence-dir", world.evidence.to_str().unwrap(), "5046"]);

  assert_eq!(run.code, 0, "{}", run.describe());
  let calls = world.merge_calls();
  assert_eq!(calls.len(), 1, "{}", run.describe());
  assert!(calls[0].starts_with("pr merge --squash 5046"), "{}", calls[0]);
  assert!(calls[0].contains("--subject Improve the status panel"), "{}", calls[0]);
  let message = std::fs::read_to_string(&world.merge_body).expect("the squash message was written");
  // The View line moves out of the body copy to the message's end, so the switch's trailer read captures the view once.
  assert_eq!(
    message.lines().filter(|line| line.starts_with("View: ")).count(),
    1,
    "each View line lands once: {message}"
  );
  assert!(message.lines().any(|line| line == "View: /?session=abc"), "{message}");
  assert!(message.contains("## Evidence"), "the pull request body stays in the squash message: {message}");
}

// -- the credential check: one scanner for the round's writes and the landing --

/// The fake credential value every case plants. It is long enough to pass the scanner's minimum length.
const CREDENTIAL_VALUE: &str = "fake-credential-value-0042";
/// The environment variables the scanner reads, each planted alone in one case.
const CREDENTIAL_ENV_NAMES: [&str; 6] = [
  "ANTHROPIC_AUTH_TOKEN",
  "CLAUDE_CODE_OAUTH_TOKEN",
  "CHARLIEBOT_RUN_TOKEN",
  "CHARLIE_CODE_API_KEY",
  "GH_TOKEN",
  "GITHUB_TOKEN",
];

/// The kinds of credential source the scanner reads.
#[derive(Clone, Copy, Debug)]
enum Kind {
  CredentialsLeaf,
  EnvToken,
  ClaudeFile,
  CodexFile,
}

const KINDS: [Kind; 4] = [Kind::CredentialsLeaf, Kind::EnvToken, Kind::ClaudeFile, Kind::CodexFile];

/// The sandbox environment with one credential value of `kind` planted, and the kind label the scanner must print.
fn planted_env(sandbox: &Sandbox, kind: Kind) -> (Env, &'static str) {
  let home = sandbox.dir("charliebot-home");
  std::fs::write(home.join("skills_leak_patterns.local.txt"), format!("{PATTERN_FILE_COMMENT}\n{PATTERN_TERM}\n"))
    .unwrap();
  let gh_log = sandbox.root().join("gh-calls.log");
  let mut env = post_env(sandbox, &home, &gh_log);
  let label = match kind {
    Kind::CredentialsLeaf => {
      std::fs::write(home.join("credentials.yaml"), format!("planted:\n  api_key: {CREDENTIAL_VALUE}\n")).unwrap();
      "credentials.yaml leaf planted.api_key"
    }
    Kind::EnvToken => {
      env.insert("ANTHROPIC_AUTH_TOKEN".to_string(), CREDENTIAL_VALUE.to_string());
      "environment variable ANTHROPIC_AUTH_TOKEN"
    }
    Kind::ClaudeFile => {
      let dir = sandbox.dir("claude-config");
      std::fs::write(
        dir.join(".credentials.json"),
        format!(r#"{{"claudeAiOauth":{{"accessToken":"{CREDENTIAL_VALUE}","scopes":["user:inference"]}}}}"#),
      )
      .unwrap();
      env.insert("CLAUDE_CONFIG_DIR".to_string(), dir.display().to_string());
      "Claude credentials file"
    }
    Kind::CodexFile => {
      let dir = sandbox.dir("codex-home");
      std::fs::write(dir.join("auth.json"), format!(r#"{{"tokens":{{"access_token":"{CREDENTIAL_VALUE}"}}}}"#))
        .unwrap();
      env.insert("CODEX_HOME".to_string(), dir.display().to_string());
      "Codex auth.json"
    }
  };
  (env, label)
}

/// A refused write: exit 3, the kind named on a stream, and the value on neither stream.
fn assert_credential_refusal(run: &Run, label: &str) {
  assert_eq!(run.code, 3, "{}", run.describe());
  assert!(run.stdout.contains(label) || run.stderr.contains(label), "the kind is named: {}", run.describe());
  assert!(
    !run.stdout.contains(CREDENTIAL_VALUE) && !run.stderr.contains(CREDENTIAL_VALUE),
    "the value is never printed: {}",
    run.describe()
  );
}

/// The round's gh shim with its scanner beside it, on a PATH whose real gh stub logs its arguments. Returns the
/// environment, the shim's path and the kind label.
fn shim_setup(sandbox: &Sandbox, kind: Kind) -> (Env, PathBuf, &'static str) {
  let (mut env, label) = planted_env(sandbox, kind);
  let tools = sandbox.dir("tools");
  let read = |path: PathBuf| std::fs::read(path).unwrap();
  harness::write_executable(&tools.join("gh"), read(harness::repo_root().join("dev/improve/gh-shim.sh")));
  harness::write_executable(&tools.join("check-skills-host-leak.sh"), read(leak_scanner()));
  harness::write_executable(&tools.join("leak-patterns.sh"), read(harness::repo_root().join("dev/leak-patterns.sh")));
  let real = sandbox.dir("real-bin");
  harness::write_executable(&real.join("gh"), "printf '%s\\n' \"$*\" >> \"$GH_CALL_LOG\"\n");
  let inherited = env.get("PATH").cloned().expect("PATH is set");
  env.insert("PATH".to_string(), format!("{}:{}:{inherited}", tools.display(), real.display()));
  (env, tools.join("gh"), label)
}

#[test]
#[ignore = "integration"]
fn the_credential_check_names_each_kind_and_never_its_value() {
  let mut cases: Vec<(Kind, Option<&str>)> = KINDS.iter().map(|kind| (*kind, None)).collect();
  cases.extend(CREDENTIAL_ENV_NAMES.iter().map(|name| (Kind::EnvToken, Some(*name))));
  for (kind, env_name) in cases {
    let sandbox = Sandbox::new();
    let (mut env, label) = planted_env(&sandbox, kind);
    let label = match env_name {
      Some(name) => {
        env.remove("ANTHROPIC_AUTH_TOKEN");
        env.insert(name.to_string(), CREDENTIAL_VALUE.to_string());
        format!("environment variable {name}")
      }
      None => label.to_string(),
    };
    let values = sandbox.root().join("values");
    let mut collect = harness::command("bash", &env);
    collect.arg(leak_scanner()).arg("--credential-values");
    let collected = run(&mut collect);
    assert_eq!(collected.code, 0, "{}", collected.describe());
    std::fs::write(&values, &collected.stdout).unwrap();
    let mut check = harness::command("bash", &env);
    check.arg(leak_scanner()).arg("--credential-check").arg(&values).arg("scanned field");
    let refused = run_with_stdin(&mut check, sandbox.root(), &format!("the text holds {CREDENTIAL_VALUE}\n"));
    assert_credential_refusal(&refused, &label);
  }
}

#[test]
#[ignore = "integration"]
fn a_value_under_the_minimum_length_passes_the_credential_check() {
  let sandbox = Sandbox::new();
  let (env, _) = planted_env(&sandbox, Kind::CredentialsLeaf);
  // Eleven characters: under the scanner's minimum, so the value is not checked.
  std::fs::write(sandbox.dir("charliebot-home").join("credentials.yaml"), "planted:\n  api_key: fake-short1\n")
    .unwrap();
  let values = sandbox.root().join("values");
  let mut collect = harness::command("bash", &env);
  collect.arg(leak_scanner()).arg("--credential-values");
  let collected = run(&mut collect);
  assert_eq!(collected.code, 0, "{}", collected.describe());
  assert!(collected.stdout.is_empty(), "no value is collected: {}", collected.describe());
  std::fs::write(&values, &collected.stdout).unwrap();
  let mut check = harness::command("bash", &env);
  check.arg(leak_scanner()).arg("--credential-check").arg(&values).arg("scanned field");
  let passed = run_with_stdin(&mut check, sandbox.root(), "the text holds fake-short1\n");
  assert_eq!(passed.code, 0, "{}", passed.describe());
}

#[test]
#[ignore = "integration"]
fn the_list_mode_checks_every_field_in_one_process_and_names_the_first_hit() {
  let sandbox = Sandbox::new();
  let (env, label) = planted_env(&sandbox, Kind::CredentialsLeaf);
  let clean = sandbox.root().join("clean.txt");
  std::fs::write(&clean, "a clean text\n").unwrap();
  let leaking = sandbox.root().join("leaking.txt");
  std::fs::write(&leaking, format!("the text holds {CREDENTIAL_VALUE}\n")).unwrap();
  let values = sandbox.root().join("values");
  let mut collect = harness::command("bash", &env);
  collect.arg(leak_scanner()).arg("--credential-values");
  let collected = run(&mut collect);
  assert_eq!(collected.code, 0, "{}", collected.describe());
  std::fs::write(&values, &collected.stdout).unwrap();
  let list = sandbox.root().join("list");
  std::fs::write(&list, format!("clean field\t{}\nleaking field\t{}\n", clean.display(), leaking.display())).unwrap();
  let mut check = harness::command("bash", &env);
  check.arg(leak_scanner()).arg("--credential-check-list").arg(&values).arg(&list);
  assert_credential_refusal(&run(&mut check), label);
}

#[test]
#[ignore = "integration"]
fn a_credentials_file_that_does_not_parse_stops_the_check_without_printing_it() {
  let sandbox = Sandbox::new();
  let (env, _) = planted_env(&sandbox, Kind::CredentialsLeaf);
  std::fs::write(sandbox.dir("charliebot-home").join("credentials.yaml"), "planted:\n  - listed-unparsed-value\n")
    .unwrap();
  let mut collect = harness::command("bash", &env);
  collect.arg(leak_scanner()).arg("--credential-values");
  let refused = run(&mut collect);
  assert_eq!(refused.code, 2, "{}", refused.describe());
  assert!(refused.stdout.is_empty(), "nothing is collected: {}", refused.describe());
  assert!(!refused.stderr.contains("listed-unparsed-value"), "the content is not printed: {}", refused.describe());
}

#[test]
#[ignore = "integration"]
fn a_pr_comment_and_a_pr_create_refuse_a_credential_value_for_each_kind() {
  for kind in KINDS {
    let sandbox = Sandbox::new();
    let (env, label) = planted_env(&sandbox, kind);
    let body = body_file(&sandbox, "comment.md", &format!("Evidence with {CREDENTIAL_VALUE}\n"));
    let mut comment = harness::command("bash", &env);
    comment.arg(github_post()).args(["pr-comment", "--body-file"]).arg(&body).arg("5");
    assert_credential_refusal(&run_in(&mut comment, sandbox.root()), label);

    let clean = body_file(&sandbox, "clean.md", "## Evidence\nclean\n");
    let mut create = harness::command("bash", &env);
    create.arg(github_post()).args(["pr-create", "--title", &format!("Title {CREDENTIAL_VALUE}"), "--body-file"]);
    create.arg(&clean).args(["--base", "main", "--head", "improve/perf/topic-slug"]);
    assert_credential_refusal(&run_in(&mut create, sandbox.root()), label);
    assert!(gh_log_lines(&sandbox.root().join("gh-calls.log")).is_empty(), "no gh call was sent");
  }
}

#[test]
#[ignore = "integration"]
fn a_push_refuses_a_credential_value_in_a_commit_message_for_each_kind() {
  for kind in KINDS {
    let sandbox = Sandbox::new();
    let (env, label) = planted_env(&sandbox, kind);
    let clone = push_repo(&sandbox, &env);
    std::fs::write(clone.join("bad.txt"), "bad\n").unwrap();
    git(&env, &clone, &["add", "bad.txt"]);
    git(&env, &clone, &["commit", "-q", "-m", &format!("follow {CREDENTIAL_VALUE}")]);
    let mut command = harness::command("bash", &env);
    command.arg(github_post()).args(["push", "-u", "origin", "improve/perf/topic-slug"]);
    assert_credential_refusal(&run_in(&mut command, &clone), label);
    assert!(!ls_remote(&env, &clone).contains("improve/perf/topic-slug"), "nothing was pushed");
  }
}

/// A clone that runs the repository's pre-push hook, its scanner and its pattern loader from its own dev directory.
fn hooked_clone(sandbox: &Sandbox, env: &Env) -> PathBuf {
  let clone = push_repo(sandbox, env);
  let hooks = clone.join("dev/git-hooks");
  std::fs::create_dir_all(&hooks).unwrap();
  let root = harness::repo_root();
  harness::copy_file(&root.join("dev/git-hooks/pre-push"), &hooks.join("pre-push"));
  harness::copy_file(&leak_scanner(), &clone.join("dev/check-skills-host-leak.sh"));
  harness::copy_file(&root.join("dev/leak-patterns.sh"), &clone.join("dev/leak-patterns.sh"));
  git(env, &clone, &["config", "core.hooksPath", "dev/git-hooks"]);
  clone
}

#[test]
#[ignore = "integration"]
fn a_push_refuses_a_credential_value_in_a_diff_through_the_pre_push_hook_for_each_kind() {
  for kind in KINDS {
    let sandbox = Sandbox::new();
    let (env, label) = planted_env(&sandbox, kind);
    let clone = hooked_clone(&sandbox, &env);
    std::fs::write(clone.join("bad.txt"), format!("the key is {CREDENTIAL_VALUE}\n")).unwrap();
    git(&env, &clone, &["add", "bad.txt"]);
    git(&env, &clone, &["commit", "-q", "-m", "clean message"]);

    // A bare git push: the hook refuses it, and git exits 1 with the kind named.
    let mut bare = harness::command("git", &env);
    bare.args(["push", "-q", "origin", "improve/perf/topic-slug"]);
    let refused = run_in(&mut bare, &clone);
    assert_ne!(refused.code, 0, "{}", refused.describe());
    assert_credential_refusal_text(&refused, label);
    assert!(!ls_remote(&env, &clone).contains("improve/perf/topic-slug"), "nothing was pushed");

    // The round's push: github-post passes the hook's refusal through as exit 3.
    let mut post = harness::command("bash", &env);
    post.arg(github_post()).args(["push", "-u", "origin", "improve/perf/topic-slug"]);
    assert_credential_refusal(&run_in(&mut post, &clone), label);
    assert!(!ls_remote(&env, &clone).contains("improve/perf/topic-slug"), "nothing was pushed");
  }
}

/// A refusal on the text streams only: a git push reports the hook's output on stderr.
fn assert_credential_refusal_text(run: &Run, label: &str) {
  assert!(run.stderr.contains(label), "the kind is named: {}", run.describe());
  assert!(!run.stdout.contains(CREDENTIAL_VALUE) && !run.stderr.contains(CREDENTIAL_VALUE), "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_gh_shim_refuses_a_credential_value_in_an_argument_for_each_kind() {
  for kind in KINDS {
    let sandbox = Sandbox::new();
    let (env, shim, label) = shim_setup(&sandbox, kind);
    let mut command = harness::command(&shim, &env);
    command.args(["pr", "comment", "5", "--body", &format!("Evidence with {CREDENTIAL_VALUE}")]);
    assert_credential_refusal(&run_in(&mut command, sandbox.root()), label);
    assert!(gh_log_lines(&sandbox.root().join("gh-calls.log")).is_empty(), "the real gh did not run");
  }
}

#[test]
#[ignore = "integration"]
fn a_gh_shim_refuses_a_credential_value_in_a_file_argument_for_each_kind() {
  for kind in KINDS {
    let sandbox = Sandbox::new();
    let (env, shim, label) = shim_setup(&sandbox, kind);
    let body = body_file(&sandbox, "comment.md", &format!("Evidence with {CREDENTIAL_VALUE}\n"));
    let mut comment = harness::command(&shim, &env);
    comment.args(["pr", "comment", "5", "--body-file"]).arg(&body);
    assert_credential_refusal(&run_in(&mut comment, sandbox.root()), label);

    let mut api = harness::command(&shim, &env);
    api.args(["api", "-X", "PATCH", "repos/o/r/pulls/5", "-F"]).arg(format!("body=@{}", body.display()));
    assert_credential_refusal(&run_in(&mut api, sandbox.root()), label);

    let mut stdin_body = harness::command(&shim, &env);
    stdin_body.args(["pr", "comment", "5", "--body-file", "-"]);
    let refused = run_with_stdin(&mut stdin_body, sandbox.root(), &format!("stdin holds {CREDENTIAL_VALUE}\n"));
    assert_credential_refusal(&refused, label);
    assert!(gh_log_lines(&sandbox.root().join("gh-calls.log")).is_empty(), "the real gh did not run");
  }
}

#[test]
#[ignore = "integration"]
fn a_gh_api_field_refuses_a_credential_value_for_each_kind() {
  for kind in KINDS {
    let sandbox = Sandbox::new();
    let (env, shim, label) = shim_setup(&sandbox, kind);
    let mut command = harness::command(&shim, &env);
    command.args(["api", "-X", "PATCH", "repos/o/r/pulls/5", "-f"]).arg(format!("title=Title {CREDENTIAL_VALUE}"));
    assert_credential_refusal(&run_in(&mut command, sandbox.root()), label);
    assert!(gh_log_lines(&sandbox.root().join("gh-calls.log")).is_empty(), "the real gh did not run");
  }
}

#[test]
#[ignore = "integration"]
fn a_gh_shim_passes_a_clean_call_to_the_real_gh() {
  let sandbox = Sandbox::new();
  let (env, shim, _) = shim_setup(&sandbox, Kind::CredentialsLeaf);
  let mut command = harness::command(&shim, &env);
  command.args(["pr", "view", "5", "--json", "url"]);
  let passed = run_in(&mut command, sandbox.root());
  assert_eq!(passed.code, 0, "{}", passed.describe());
  assert_eq!(gh_log_lines(&sandbox.root().join("gh-calls.log")), vec!["pr view 5 --json url".to_string()]);
}
