//! The leak gate the improve round and the repository's git hooks run:
//! `tools/improve/github-post.sh`, `tools/check-skills-host-leak.sh`, `tools/leak-patterns.sh` and
//! `tools/git-hooks/`. Ports `tests/test_github_post.py` and `tests/test_host_leak_hooks.py`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use charliebot_shell_harness::{self as harness, Env, Run, Sandbox};

const HOME_PATH_TEXT: &str = "the notes live under /home/someone/notes";
const TAILNET_TEXT: &str = "measured on example.ts.net";
const PATTERN_TERM: &str = "fictionalmarker";
const PATTERN_FILE_COMMENT: &str = "# local leak patterns";
const CLEAN_BODY: &str = "## Evidence\nbefore 259 ms, after 130 ms (n=50).\n";

fn github_post() -> PathBuf {
  harness::repo_root().join("tools/improve/github-post.sh")
}

fn leak_scanner() -> PathBuf {
  harness::repo_root().join("tools/check-skills-host-leak.sh")
}

/// The github-post environment: the `gh` stub records its calls, the profile home carries the
/// pattern file, and git identity and config are pinned for the pushes.
fn post_env(sandbox: &Sandbox, home: &Path, gh_log: &Path) -> Env {
  sandbox.stub("gh", "printf '%s\\n' \"$*\" >> \"$GH_CALL_LOG\"\n");
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
  let mut child = command.spawn().expect("spawn the scanner");
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

// -- github-post.sh (tests/test_github_post.py) --

fn post(sandbox: &Sandbox) -> (Env, PathBuf, PathBuf) {
  let home = pattern_home(sandbox);
  let gh_log = sandbox.root().join("gh-calls.log");
  let env = post_env(sandbox, &home, &gh_log);
  (env, gh_log, home)
}

#[test]
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

#[test]
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
fn a_tilde_profile_home_names_the_pattern_file_under_home() {
  let sandbox = Sandbox::new();
  let (mut env, gh_log, _home) = post(&sandbox);
  // The server's home rule expands a leading `~` to HOME; the pattern file sits at `<root>/charliebot-home`.
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

// -- the git hooks and the host scan (tests/test_host_leak_hooks.py) --

/// A scratch repository carrying a copy of the leak tools and the hooks path installed.
fn leak_repo(sandbox: &Sandbox) -> (PathBuf, Env) {
  let repo = sandbox.dir("leak-repo");
  let tools = repo.join("tools");
  std::fs::create_dir_all(tools.join("git-hooks")).unwrap();
  harness::copy_file(&leak_scanner(), &tools.join("check-skills-host-leak.sh"));
  harness::copy_file(&harness::repo_root().join("tools/leak-patterns.sh"), &tools.join("leak-patterns.sh"));
  for hook in ["pre-commit", "commit-msg"] {
    harness::copy_file(&harness::repo_root().join("tools/git-hooks").join(hook), &tools.join("git-hooks").join(hook));
  }
  let patterns = sandbox.dir("patterns");
  std::fs::write(patterns.join("secretmarker.txt"), "secretmarker\n").unwrap();

  let mut env = sandbox.env();
  env.insert("HOME".to_string(), sandbox.root().display().to_string());
  env.insert("EXTRA_PATTERNS_FILE".to_string(), patterns.join("secretmarker.txt").display().to_string());
  env.insert("GIT_CONFIG_NOSYSTEM".to_string(), "1".to_string());
  env.insert("GIT_CONFIG_GLOBAL".to_string(), "/dev/null".to_string());
  for name in ["GIT_AUTHOR_NAME", "GIT_COMMITTER_NAME"] {
    env.insert(name.to_string(), "Example".to_string());
  }
  for name in ["GIT_AUTHOR_EMAIL", "GIT_COMMITTER_EMAIL"] {
    env.insert(name.to_string(), "123+example@users.noreply.github.com".to_string());
  }
  git(&env, sandbox.root(), &["init", "-q", "-b", "main", repo.to_str().unwrap()]);
  git(&env, &repo, &["config", "core.hooksPath", "tools/git-hooks"]);
  (repo, env)
}

fn leak_run(repo: &Path, env: &Env, args: &[&str]) -> Run {
  let mut command = harness::command("git", env);
  command.args(args);
  run_in(&mut command, repo)
}

#[test]
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
  repo.join("tools/check-skills-host-leak.sh")
}
