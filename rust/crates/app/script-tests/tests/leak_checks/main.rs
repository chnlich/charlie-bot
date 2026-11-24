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

/// The credential-carrying environment variables the scanner reads. A test environment drops each so the only
/// credential a run can hold is the one the test plants.
const CREDENTIAL_ENV_NAMES: [&str; 6] = [
  "ANTHROPIC_AUTH_TOKEN",
  "CLAUDE_CODE_OAUTH_TOKEN",
  "CHARLIEBOT_RUN_TOKEN",
  "CHARLIE_CODE_API_KEY",
  "GH_TOKEN",
  "GITHUB_TOKEN",
];

/// The environment without the credential variables: a test that plants none collects zero credential values.
fn without_credential_env(mut env: Env) -> Env {
  for name in CREDENTIAL_ENV_NAMES {
    env.remove(name);
  }
  env
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
  // Empty config directories: the scanner reads the host's credential files only when no directory is named.
  env.insert("CLAUDE_CONFIG_DIR".to_string(), sandbox.dir("claude-config-unset").display().to_string());
  env.insert("CODEX_HOME".to_string(), sandbox.dir("codex-home-unset").display().to_string());
  env.insert("GIT_CONFIG_NOSYSTEM".to_string(), "1".to_string());
  env.insert("GIT_CONFIG_GLOBAL".to_string(), "/dev/null".to_string());
  for name in ["GIT_AUTHOR_NAME", "GIT_COMMITTER_NAME"] {
    env.insert(name.to_string(), "Example".to_string());
  }
  for name in ["GIT_AUTHOR_EMAIL", "GIT_COMMITTER_EMAIL"] {
    env.insert(name.to_string(), "123+example@users.noreply.github.com".to_string());
  }
  // The round's launch directory with an empty in-flight list: the runner writes that list at launch, and the claim
  // check of pr-create and pr-merge reads it.
  let launch = sandbox.dir("round-launch");
  std::fs::write(launch.join("inflight-files.txt"), "").unwrap();
  env.insert("IMPROVE_LAUNCH_DIR".to_string(), launch.display().to_string());
  without_credential_env(env)
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

/// Writes the round's in-flight list as the runner writes it at launch: `path<TAB>branch` per claimed path.
fn write_inflight_list(env: &Env, claims: &[(&str, &str)]) {
  let list: String = claims.iter().map(|(path, branch)| format!("{path}\t{branch}\n")).collect();
  std::fs::write(Path::new(&env["IMPROVE_LAUNCH_DIR"]).join("inflight-files.txt"), list).unwrap();
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
  let clone = push_repo(&sandbox, &env);
  let body = body_file(&sandbox, "body.md", CLEAN_BODY);

  let mut command = harness::command("bash", &env);
  command
    .arg(github_post())
    .args(["pr-create", "--title"])
    .arg(format!("fix {HOME_PATH_TEXT}"))
    .args(["--body-file"])
    .arg(&body)
    .args(["--base", "main"]);
  let run = run_in(&mut command, &clone);
  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(run.stderr.contains("title"), "{}", run.stderr);
  assert!(gh_log_lines(&gh_log).is_empty());

  // Without --head, pr-create takes the current branch, as gh does.
  let mut command = harness::command("bash", &env);
  command
    .arg(github_post())
    .args(["pr-create", "--title", "clean topic", "--body-file"])
    .arg(&body)
    .args(["--base", "main"]);
  let run = run_in(&mut command, &clone);
  assert_eq!(run.code, 0, "{}", run.describe());
  assert!(gh_log_lines(&gh_log)[0].contains("pr create"), "{}", gh_log_lines(&gh_log)[0]);
}

#[test]
#[ignore = "integration"]
fn a_pr_create_refuses_a_claimed_path_and_allows_a_path_only_its_own_branch_claims() {
  let sandbox = Sandbox::new();
  let (env, gh_log, _home) = post(&sandbox);
  let work = push_repo(&sandbox, &env);
  let body = body_file(&sandbox, "body.md", CLEAN_BODY);
  let create = |env: &Env| {
    let mut command = harness::command("bash", env);
    command.arg(github_post()).args(["pr-create", "--title", "clean topic", "--body-file"]).arg(&body);
    command.args(["--base", "main", "--head", HEAD_BRANCH]);
    run_in(&mut command, &work)
  };

  write_inflight_list(&env, &[("change.txt", OTHER_CLAIMANT)]);
  let run = create(&env);
  assert_eq!(run.code, 2, "{}", run.describe());
  assert!(
    run.stderr.lines().any(|line| line.contains("change.txt") && line.contains(OTHER_CLAIMANT)),
    "{}",
    run.stderr
  );
  assert!(gh_log_lines(&gh_log).is_empty(), "no pull request was created");

  write_inflight_list(&env, &[("change.txt", HEAD_BRANCH), ("docs/unrelated.md", OTHER_CLAIMANT)]);
  let run = create(&env);
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
  assert!(run.stdout.contains("message"), "{}", run.describe());
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
  assert!(run.stdout.contains("branch name"), "{}", run.describe());
  assert!(!ls_remote(&env, &clone).contains(&branch));
}

#[test]
#[ignore = "integration"]
fn a_push_scans_every_unpublished_commit_when_origin_main_is_unknown() {
  let sandbox = Sandbox::new();
  let (env, _gh_log, _home) = post(&sandbox);
  let clone = push_repo(&sandbox, &env);
  git(&env, &clone, &["update-ref", "-d", "refs/remotes/origin/main"]);
  std::fs::write(clone.join("bad.txt"), "bad\n").unwrap();
  git(&env, &clone, &["add", "bad.txt"]);
  git(&env, &clone, &["commit", "-q", "-m", &format!("follow {PATTERN_TERM}")]);

  // Without origin/main the check falls back to the commits no remote ref reaches, so the leaky message in the
  // unpublished history still refuses the push.
  let mut command = harness::command("bash", &env);
  command.arg(github_post()).args(["push", "-u", "origin", "improve/perf/topic-slug"]);
  let run = run_in(&mut command, &clone);
  assert_eq!(run.code, 1, "{}", run.describe());
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

/// Fake session ids. The built-in UUID rule matches each, so one passes only as the session value of a View line.
const SESSION_ID: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
const OTHER_ID: &str = "ffffffff-0000-4111-8222-333333333333";

#[test]
#[ignore = "integration"]
fn a_session_id_passes_only_as_the_session_value_of_a_view_line() {
  let sandbox = Sandbox::new();
  let (env, _gh_log, _home) = post(&sandbox);
  let mut scan = harness::command("bash", &env);
  scan.arg(leak_scanner()).arg("--stdin");

  for admitted in [
    format!("View: /?session={SESSION_ID}"),
    format!("View: /?session={SESSION_ID}&tab=status"),
    format!("View: /?tab=status&session={SESSION_ID}"),
  ] {
    let run = run_with_stdin(&mut scan, sandbox.root(), &format!("## Evidence\n{admitted}\n"));
    assert_eq!(run.code, 0, "{admitted}: {}", run.describe());
  }

  for (refused, why) in [
    (format!("View: /?session={SESSION_ID}\nEvidence {SESSION_ID}\n"), "the same id on another line"),
    (format!("View: /?ref={SESSION_ID}\n"), "a View parameter other than session"),
    (format!("View: /{SESSION_ID}\n"), "the path outside the query"),
    (format!("View: https://example.invalid/?session={SESSION_ID}\n"), "a URL with a host"),
    (format!("View: /?session={SESSION_ID}&other={OTHER_ID}\n"), "a second UUID on the same View line"),
    (
      format!("View: /?session={SESSION_ID}\nView: /?ref={OTHER_ID}\n"),
      "a UUID outside the session of a second View line",
    ),
    (format!("  View: /?session={SESSION_ID}\n"), "an indented View line"),
    (format!("{SESSION_ID}\n"), "the id on a line of its own"),
  ] {
    let run = run_with_stdin(&mut scan, sandbox.root(), &refused);
    assert_eq!(run.code, 1, "{why}: {}", run.describe());
  }
}

#[test]
#[ignore = "integration"]
fn a_pr_comment_admits_a_session_view_body_and_refuses_the_id_elsewhere() {
  let sandbox = Sandbox::new();
  let (env, gh_log, _home) = post(&sandbox);
  let admitted = body_file(
    &sandbox,
    "admitted.md",
    &format!("## Evidence\nbefore and after.\n\nView: /?tab=status&session={SESSION_ID}\n"),
  );
  let mut command = harness::command("bash", &env);
  command.arg(github_post()).args(["pr-comment", "--body-file"]).arg(&admitted).arg("123");
  let run = run_in(&mut command, sandbox.root());
  assert_eq!(run.code, 0, "{}", run.describe());
  assert_eq!(gh_log_lines(&gh_log).len(), 1, "the admitted body is sent");

  let refused = body_file(&sandbox, "refused.md", &format!("View: /?session={SESSION_ID}\nEvidence {SESSION_ID}\n"));
  let mut command = harness::command("bash", &env);
  command.arg(github_post()).args(["pr-comment", "--body-file"]).arg(&refused).arg("123");
  let run = run_in(&mut command, sandbox.root());
  assert_eq!(run.code, 1, "{}", run.describe());
  assert_eq!(gh_log_lines(&gh_log).len(), 1, "the refused body sends nothing");
}

// -- the git hooks and the host scan --

/// The leak tools and the two hooks copied into `checkout`, at the paths a real checkout uses.
fn copy_leak_tools(checkout: &Path) {
  let dev = checkout.join("dev");
  std::fs::create_dir_all(dev.join("git-hooks")).unwrap();
  harness::copy_file(&leak_scanner(), &dev.join("check-skills-host-leak.sh"));
  harness::copy_file(&harness::repo_root().join("dev/leak-patterns.sh"), &dev.join("leak-patterns.sh"));
  harness::copy_file(&harness::repo_root().join("dev/push-text-lists.sh"), &dev.join("push-text-lists.sh"));
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
  without_credential_env(env)
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
  assert_eq!(leak_run(&repo, &env, &["add", "notes.txt", "web/features/demo/leak_scan_files.txt"]).code, 0);
  let commit = leak_run(&repo, &env, &["commit", "-m", "add notes"]);
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

/// The `gh` stand-in for `pr-merge` and the verdict comment: it answers the diff and view reads from files the test
/// writes, runs a `--jq` filter over the comments file with jq as gh does, records every call, and copies the
/// `--body-file` of a merge and of a comment aside so a test can read what reached GitHub.
const MERGE_GH_STUB: &str = r#"case "$1 $2" in
  'pr diff')
    cat "$GH_CHANGED_PATHS"
    ;;
  'pr view')
    json=
    previous=
    for argument in "$@"; do
      if [ "$previous" = --json ]; then json=$argument; fi
      previous=$argument
    done
    case "$json" in
      title) cat "$GH_PR_TITLE" ;;
      body) cat "$GH_PR_BODY" ;;
      isDraft) cat "$GH_PR_DRAFT" ;;
      headRefOid) printf '%s\n' "$GH_HEAD_SHA" ;;
      headRefName) printf '%s\n' "$GH_HEAD_BRANCH" ;;
      comments) jq --raw-output "${!#}" "$GH_COMMENTS" ;;
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
if [[ "$1 $2" == 'pr comment' ]]; then
  while [[ $# -gt 0 ]]; do
    if [[ "$1" == --body-file ]]; then cp "$2" "$GH_COMMENT_BODY"; fi
    shift
  done
fi
"#;

/// The head sha of the synthetic pull request, and an older head it was rebased or fixed from.
const HEAD_SHA: &str = "0123456789abcdef0123456789abcdef01234567";
const OLDER_SHA: &str = "89abcdef0123456789abcdef0123456789abcdef";
/// The synthetic pull request's head branch, which is the round's own branch, and two branches of other live work.
const HEAD_BRANCH: &str = "improve/perf/topic-slug";
const OTHER_CLAIMANT: &str = "charliebot/task-1795416635-2e6cf3d4";
const OTHER_CLAIMANT_TWO: &str = "improve/ux/other-topic";
/// The review's stdout for a verdict with no issues, and for one with a finding at the bar of confidence 80.
const CLEAN_REVIEW: &str = "Code review: no issues found.\nScope: commits a1 and files web/static/ui.js.\n";
const FINDING_REVIEW: &str =
  "- The panel drops its first row (confidence 85) — web/static/ui.js: the loop bound is one short.\n";

/// The comment that `pr-comment --verdict-head <sha>` posts: the head line, a blank line, then the review's stdout.
fn verdict_comment(sha: &str, review: &str) -> String {
  format!("Review verdict on {sha}\n\n{review}")
}

/// The skills directory that the round's runner exports as `IMPROVE_SKILLS_DIR`.
const ROUND_SKILLS: &str = "/synthetic/improve/runner/bin/0123456789abcdef/skills";

/// One `Read` call of `path`, as a round's or a review's event log holds it.
fn read_call(path: &str) -> String {
  serde_json::json!({"type": "assistant", "message": {"content": [
    {"type": "tool_use", "id": "toolu_read", "name": "Read", "input": {"file_path": path}}
  ]}})
  .to_string()
}

/// One `Bash` call of `command`, as a round's or a review's event log holds it.
fn bash_call(command: &str) -> String {
  serde_json::json!({"type": "assistant", "message": {"content": [
    {"type": "tool_use", "id": "toolu_bash", "name": "Bash", "input": {"command": command}}
  ]}})
  .to_string()
}

/// The screenshot names that `MergeWorld::write_screenshots(count)` writes, in its order.
fn shot_names(count: usize) -> Vec<String> {
  charliebot_tasks::web_screenshots::WINDOW_SIZES
    .iter()
    .flat_map(|(width, height)| (0..count).map(move |copy| format!("shot-{width}-{height}-{copy}.png")))
    .collect()
}

/// A pull request body that names each of `names` by its file name, then carries `rest`.
fn body_naming(names: &[String], rest: &str) -> String {
  let list: String = names.iter().map(|name| format!("- {name}\n")).collect();
  format!("## Evidence\n{list}{rest}")
}

/// One synthetic pull request for `pr-merge`: the changed paths, the title and body, the draft state, the call log and
/// the copied squash message, plus an evidence directory the caller fills with screenshots.
struct MergeWorld {
  sandbox: Sandbox,
  env: Env,
  gh_log: PathBuf,
  merge_body: PathBuf,
  comment_body: PathBuf,
  comments: PathBuf,
  evidence: PathBuf,
  pr_draft: PathBuf,
  round_events: PathBuf,
}

impl MergeWorld {
  /// A pull request at `HEAD_SHA` whose newest verdict comment reports no issues on that head.
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
    let comment_body = sandbox.root().join("comment-body");
    let comments = sandbox.root().join("comments.json");
    let pr_draft = sandbox.root().join("pr-draft");
    std::fs::write(&changed_paths, changed).unwrap();
    std::fs::write(&pr_title, "Improve the status panel\n").unwrap();
    std::fs::write(&pr_body_file, pr_body).unwrap();
    std::fs::write(&pr_draft, "false\n").unwrap();
    let mut env = env;
    env.insert("GH_CHANGED_PATHS".to_string(), changed_paths.display().to_string());
    env.insert("GH_PR_TITLE".to_string(), pr_title.display().to_string());
    env.insert("GH_PR_BODY".to_string(), pr_body_file.display().to_string());
    env.insert("GH_PR_DRAFT".to_string(), pr_draft.display().to_string());
    env.insert("GH_MERGE_BODY".to_string(), merge_body.display().to_string());
    env.insert("GH_COMMENT_BODY".to_string(), comment_body.display().to_string());
    env.insert("GH_COMMENTS".to_string(), comments.display().to_string());
    env.insert("GH_HEAD_SHA".to_string(), HEAD_SHA.to_string());
    env.insert("GH_HEAD_BRANCH".to_string(), HEAD_BRANCH.to_string());
    let evidence = sandbox.dir("evidence");
    // The round's launch directory already holds its in-flight list; add an empty event log until a test writes its
    // tool calls.
    let launch = PathBuf::from(env.get("IMPROVE_LAUNCH_DIR").expect("post_env sets IMPROVE_LAUNCH_DIR"));
    let round_events = launch.join("round-events.ndjson");
    std::fs::write(&round_events, "").unwrap();
    env.insert("IMPROVE_SKILLS_DIR".to_string(), ROUND_SKILLS.to_string());
    let world =
      MergeWorld { sandbox, env, gh_log, merge_body, comment_body, comments, evidence, pr_draft, round_events };
    world.set_comments(&[verdict_comment(HEAD_SHA, CLEAN_REVIEW)]);
    world
  }

  /// Replaces the pull request's comments, oldest first.
  fn set_comments(&self, bodies: &[String]) {
    let comments: Vec<serde_json::Value> = bodies.iter().map(|body| serde_json::json!({ "body": body })).collect();
    std::fs::write(&self.comments, serde_json::json!({ "comments": comments }).to_string()).unwrap();
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

  /// Writes the review record of a review that read each of `reads`, as `charliebot backend run` writes its
  /// `events.ndjson`: a `Read` call on an assistant event, answered on the next user event by its result.
  fn review_record(&self, reads: &[String]) -> PathBuf {
    let mut lines = Vec::new();
    for (index, name) in reads.iter().enumerate() {
      let id = format!("toolu_{index}");
      lines.push(
        serde_json::json!({"type": "assistant", "message": {"content": [
          {"type": "tool_use", "id": id.clone(), "name": "Read", "input": {"file_path": format!("/evidence/{name}")}}
        ]}})
        .to_string(),
      );
      lines.push(
        serde_json::json!({"type": "user", "message": {"content": [
          {"type": "tool_result", "tool_use_id": id, "is_error": false, "content": "ok"}
        ]}})
        .to_string(),
      );
    }
    let path = self.sandbox.root().join("events.ndjson");
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();
    path
  }

  /// Writes the round's event log from `lines`: the stream-json of its tool calls, as the round's runner keeps it.
  fn round(&self, lines: &[String]) {
    std::fs::write(&self.round_events, lines.join("\n") + "\n").unwrap();
  }

  /// Writes a review's event log from `lines` in the sandbox and returns its path, for `--review-record`.
  fn review_events(&self, lines: &[String]) -> PathBuf {
    let path = self.sandbox.root().join("review-events.ndjson");
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();
    path
  }

  fn merge(&self, args: &[&str]) -> Run {
    let mut command = harness::command("bash", &self.env);
    command.arg(github_post()).arg("pr-merge").args(args);
    run_in(&mut command, self.sandbox.root())
  }

  fn comment(&self, args: &[&str]) -> Run {
    let mut command = harness::command("bash", &self.env);
    command.arg(github_post()).arg("pr-comment").args(args);
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
  let names = shot_names(2);
  let world = MergeWorld::new("web/static/ui.js\n", &body_naming(&names, "before and after.\n"));
  world.write_screenshots(2);
  let record = world.review_record(&names);

  let run = world.merge(&[
    "--evidence-dir",
    world.evidence.to_str().unwrap(),
    "--review-record",
    record.to_str().unwrap(),
    "5046",
  ]);

  assert_eq!(run.code, 2, "{}", run.describe());
  assert!(
    run.stderr.contains("a `View: <URL>` line naming the full URL, path and query, that opens the changed state"),
    "{}",
    run.stderr
  );
  assert!(world.merge_calls().is_empty(), "nothing may merge: {}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_pr_merge_refuses_a_view_without_a_leading_slash() {
  let names = shot_names(2);
  let world = MergeWorld::new("web/static/ui.js\n", &body_naming(&names, "\nView: diff\n"));
  world.write_screenshots(2);
  let record = world.review_record(&names);

  let run = world.merge(&[
    "--evidence-dir",
    world.evidence.to_str().unwrap(),
    "--review-record",
    record.to_str().unwrap(),
    "5046",
  ]);

  assert_eq!(run.code, 2, "{}", run.describe());
  assert!(run.stderr.contains("PR 5046"), "{}", run.stderr);
  assert!(run.stderr.contains("must start with `/`"), "{}", run.stderr);
  assert!(world.merge_calls().is_empty(), "nothing may merge: {}", run.describe());
  assert!(!world.merge_body.exists(), "no squash message was written");
}

#[test]
#[ignore = "integration"]
fn a_pr_merge_refuses_a_web_page_change_whose_review_record_lacks_a_read_of_a_named_screenshot() {
  let names = shot_names(2);
  let world = MergeWorld::new("web/static/ui.js\n", &body_naming(&names, "\nView: /?session=abc\n"));
  world.write_screenshots(2);
  let record = world.review_record(&names[1..]);

  let run = world.merge(&[
    "--evidence-dir",
    world.evidence.to_str().unwrap(),
    "--review-record",
    record.to_str().unwrap(),
    "5046",
  ]);

  assert_eq!(run.code, 2, "{}", run.describe());
  assert!(run.stderr.contains("shows no read of shot-1536-900-0.png"), "{}", run.describe());
  assert!(world.merge_calls().is_empty(), "nothing may merge: {}", run.describe());
  assert!(!world.merge_body.exists(), "no squash message was written");
}

#[test]
#[ignore = "integration"]
fn a_pr_merge_refuses_a_web_page_change_without_a_review_record() {
  let names = shot_names(2);
  let world = MergeWorld::new("web/static/ui.js\n", &body_naming(&names, "\nView: /?session=abc\n"));
  world.write_screenshots(2);

  let run = world.merge(&["--evidence-dir", world.evidence.to_str().unwrap(), "5046"]);

  assert_eq!(run.code, 2, "{}", run.describe());
  assert!(run.stderr.contains("--review-record"), "{}", run.describe());
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
  let names = shot_names(2);
  let world = MergeWorld::new("web/static/ui.js\n", &body_naming(&names, "\nView: /?session=abc\n"));
  world.write_screenshots(2);
  let record = world.review_record(&names);

  let run = world.merge(&[
    "--evidence-dir",
    world.evidence.to_str().unwrap(),
    "--review-record",
    record.to_str().unwrap(),
    "5046",
  ]);

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

#[test]
#[ignore = "integration"]
fn a_pr_merge_refuses_a_diff_that_changes_a_claimed_path_and_names_each_claimant() {
  let world = MergeWorld::new("rust/crates/x/src/lib.rs\nrust/crates/y/src/main.rs\n", CLEAN_BODY);
  let claims = [("rust/crates/x/src/lib.rs", OTHER_CLAIMANT), ("rust/crates/y/src/main.rs", OTHER_CLAIMANT_TWO)];
  write_inflight_list(&world.env, &claims);

  let run = world.merge(&["5046"]);

  assert_eq!(run.code, 2, "{}", run.describe());
  for (path, claimant) in claims {
    assert!(run.stderr.lines().any(|line| line.contains(path) && line.contains(claimant)), "{}", run.stderr);
  }
  assert!(world.merge_calls().is_empty(), "nothing may merge: {}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_pr_merge_allows_a_diff_claimed_only_by_its_own_branch_or_not_at_all() {
  let world = MergeWorld::new("rust/crates/x/src/lib.rs\n", CLEAN_BODY);
  write_inflight_list(&world.env, &[("rust/crates/x/src/lib.rs", HEAD_BRANCH), ("docs/unrelated.md", OTHER_CLAIMANT)]);

  let run = world.merge(&["5046"]);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert_eq!(world.merge_calls().len(), 1, "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_pr_merge_refuses_a_draft_pull_request() {
  let world = MergeWorld::new("rust/crates/x/src/lib.rs\n", "## Evidence\nmeasurements pending.\n");
  std::fs::write(&world.pr_draft, "true\n").unwrap();

  let run = world.merge(&["5046"]);

  assert_eq!(run.code, 2, "{}", run.describe());
  assert!(run.stderr.contains("a draft waits for its measurements"), "{}", run.describe());
  assert!(world.merge_calls().is_empty(), "nothing may merge: {}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_pr_merge_refuses_a_pr_without_a_verdict_comment() {
  let world = MergeWorld::new("rust/crates/x/src/lib.rs\n", "## Evidence\nno screenshot needed.\n");
  world.set_comments(&["improve-waiting: main repair held by another direction".to_string()]);

  let run = world.merge(&["5046"]);

  assert_eq!(run.code, 2, "{}", run.describe());
  assert!(run.stderr.contains("has no review verdict comment"), "{}", run.describe());
  assert!(world.merge_calls().is_empty(), "nothing may merge: {}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_pr_merge_refuses_a_verdict_that_names_an_older_head() {
  let world = MergeWorld::new("rust/crates/x/src/lib.rs\n", "## Evidence\nno screenshot needed.\n");
  world.set_comments(&[verdict_comment(OLDER_SHA, CLEAN_REVIEW)]);

  let run = world.merge(&["5046"]);

  assert_eq!(run.code, 2, "{}", run.describe());
  assert!(run.stderr.contains(OLDER_SHA) && run.stderr.contains(HEAD_SHA), "{}", run.describe());
  assert!(world.merge_calls().is_empty(), "nothing may merge: {}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_pr_merge_refuses_a_verdict_on_the_head_that_reports_findings() {
  let world = MergeWorld::new("rust/crates/x/src/lib.rs\n", "## Evidence\nno screenshot needed.\n");
  world.set_comments(&[verdict_comment(HEAD_SHA, FINDING_REVIEW)]);

  let run = world.merge(&["5046"]);

  assert_eq!(run.code, 2, "{}", run.describe());
  assert!(run.stderr.contains("reports findings"), "{}", run.describe());
  assert!(world.merge_calls().is_empty(), "nothing may merge: {}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_pr_merge_passes_a_clean_verdict_on_the_head() {
  let world = MergeWorld::new("rust/crates/x/src/lib.rs\n", "## Evidence\nno screenshot needed.\n");

  let run = world.merge(&["5046"]);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert_eq!(world.merge_calls(), ["pr merge --squash 5046"], "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_pr_merge_reads_the_newest_verdict_after_a_fix_and_its_rerun() {
  // The round's fix moves the head to HEAD_SHA, and the rerun of the same review posts a clean verdict on it.
  let world = MergeWorld::new("rust/crates/x/src/lib.rs\n", "## Evidence\nno screenshot needed.\n");
  world.set_comments(&[verdict_comment(OLDER_SHA, FINDING_REVIEW), verdict_comment(HEAD_SHA, CLEAN_REVIEW)]);

  let run = world.merge(&["5046"]);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert_eq!(world.merge_calls(), ["pr merge --squash 5046"], "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_pr_merge_refuses_a_round_whose_event_log_reads_a_skill_outside_its_skills_dir() {
  let world = MergeWorld::new("rust/crates/x/src/lib.rs\n", "## Evidence\nno screenshot needed.\n");
  let stale = "/synthetic/checkout/skills/improve-worker/SKILL.md";
  world.round(&[read_call(stale), read_call(&format!("{ROUND_SKILLS}/writing-style/genres/code.md"))]);

  let run = world.merge(&["5046"]);

  assert_eq!(run.code, 2, "{}", run.describe());
  assert!(run.stderr.contains(&format!("skill read outside {ROUND_SKILLS}: {stale}")), "{}", run.describe());
  assert!(world.merge_calls().is_empty(), "nothing may merge:\n{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_pr_merge_refuses_a_review_record_that_reads_a_skill_outside_its_skills_dir() {
  // The path that `skills/charliebot/SKILL.md` names: a review that follows it reads a host copy.
  let world = MergeWorld::new("rust/crates/x/src/lib.rs\n", "## Evidence\nno screenshot needed.\n");
  world.round(&[read_call(&format!("{ROUND_SKILLS}/writing-style/genres/code.md"))]);
  let stale = "~/.claude/skills/writing-style/genres/commits-and-prs.md";
  let review = world.review_events(&[bash_call(&format!("cat {stale}"))]);

  let run = world.merge(&["--review-record", review.to_str().unwrap(), "5046"]);

  assert_eq!(run.code, 2, "{}", run.describe());
  assert!(run.stderr.contains(&format!("skill read outside {ROUND_SKILLS}: {stale}")), "{}", run.describe());
  assert!(world.merge_calls().is_empty(), "nothing may merge:\n{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_pr_merge_passes_a_round_and_review_that_read_only_their_skills_dir_and_cite_skills_in_a_commit() {
  let world = MergeWorld::new("rust/crates/x/src/lib.rs\n", "## Evidence\nno screenshot needed.\n");
  let kit_rule = format!("{ROUND_SKILLS}/writing-style/genres/code.md");
  world.round(&[
    read_call(&kit_rule),
    bash_call("git commit -m \"docs: cite skills/writing-style/genres/commits-and-prs.md\""),
  ]);
  let review = world.review_events(&[read_call(&kit_rule)]);

  let run = world.merge(&["--review-record", review.to_str().unwrap(), "5046"]);

  assert_eq!(run.code, 0, "{}", run.describe());
  assert_eq!(world.merge_calls(), ["pr merge --squash 5046"], "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_pr_merge_refuses_a_round_whose_event_log_is_missing() {
  let world = MergeWorld::new("rust/crates/x/src/lib.rs\n", "## Evidence\nno screenshot needed.\n");
  std::fs::remove_file(&world.round_events).unwrap();

  let run = world.merge(&["5046"]);

  assert_eq!(run.code, 2, "{}", run.describe());
  assert!(world.merge_calls().is_empty(), "nothing may merge without the round's event log:\n{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_pr_comment_with_a_verdict_head_posts_the_head_line_that_pr_merge_reads() {
  let world = MergeWorld::new("rust/crates/x/src/lib.rs\n", "## Evidence\nno screenshot needed.\n");
  let review = world.sandbox.root().join("review.md");
  std::fs::write(&review, CLEAN_REVIEW).unwrap();

  let run = world.comment(&["--verdict-head", HEAD_SHA, "--body-file", review.to_str().unwrap(), "5046"]);

  assert_eq!(run.code, 0, "{}", run.describe());
  let posted = std::fs::read_to_string(&world.comment_body).expect("the verdict comment was posted");
  assert_eq!(posted, verdict_comment(HEAD_SHA, CLEAN_REVIEW), "{}", run.describe());
  let calls: Vec<String> =
    gh_log_lines(&world.gh_log).into_iter().filter(|line| line.starts_with("pr comment")).collect();
  assert_eq!(calls.len(), 1, "{}", run.describe());
  assert!(!calls[0].contains("--verdict-head"), "gh receives no verdict flag: {}", calls[0]);
  // The comment the round posted is the one the merge reads back: the same head clears the merge.
  world.set_comments(&[posted]);
  let merge = world.merge(&["5046"]);
  assert_eq!(merge.code, 0, "{}", merge.describe());
}

#[test]
#[ignore = "integration"]
fn a_pr_comment_refuses_a_verdict_head_that_is_not_a_full_sha() {
  let world = MergeWorld::new("rust/crates/x/src/lib.rs\n", "## Evidence\nno screenshot needed.\n");
  let review = world.sandbox.root().join("review.md");
  std::fs::write(&review, CLEAN_REVIEW).unwrap();

  let run = world.comment(&["--verdict-head", "0123abc", "--body-file", review.to_str().unwrap(), "5046"]);

  assert_eq!(run.code, 2, "{}", run.describe());
  assert!(run.stderr.contains("full 40-character head sha"), "{}", run.describe());
  assert!(!world.comment_body.exists(), "nothing was posted");
}

// -- the credential check: one scanner for the round's writes and the landing --

/// The fake credential value every case plants. It is long enough to pass the scanner's minimum length.
const CREDENTIAL_VALUE: &str = "fake-credential-value-0042";

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
fn a_credentials_leaf_named_for_a_non_secret_is_left_out_of_the_values() {
  let sandbox = Sandbox::new();
  let (env, _) = planted_env(&sandbox, Kind::CredentialsLeaf);
  // The four skipped names, each as a last segment, and two leaves whose names only start with one of them:
  // `e.model_key` and `f.api_key` are the leaves that stay checked.
  std::fs::write(
    sandbox.dir("charliebot-home").join("credentials.yaml"),
    "a:\n  description: fake-description-a-0001\n\
     b:\n  model: fake-model-name-b-0002\n\
     c:\n  client_id: fake-client-id-c-0003\n\
     d:\n  app_id: fake-app-id-d-0004\n\
     e:\n  model_key: fake-model-key-e-0005\n\
     f:\n  api_key: fake-api-key-f-0006\n",
  )
  .unwrap();
  let mut collect = harness::command("bash", &env);
  collect.arg(leak_scanner()).arg("--credential-values");
  let collected = run(&mut collect);
  assert_eq!(collected.code, 0, "{}", collected.describe());
  let kinds: Vec<&str> = collected.stdout.lines().map(|line| line.split('\t').next().unwrap()).collect();
  assert_eq!(
    kinds,
    ["credentials.yaml leaf e.model_key", "credentials.yaml leaf f.api_key"],
    "{}",
    collected.describe()
  );
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
  harness::copy_file(&root.join("dev/push-text-lists.sh"), &clone.join("dev/push-text-lists.sh"));
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

// -- the scanner's option handling and list modes --

/// A scanner run in the sandbox root with the given arguments and no stdin text.
fn scan(env: &Env, args: &[&str]) -> Run {
  let mut command = harness::command("bash", env);
  command.arg(leak_scanner()).args(args);
  run(&mut command)
}

#[test]
#[ignore = "integration"]
fn an_unknown_option_exits_2() {
  let sandbox = Sandbox::new();
  let env = pinned_git_env(&sandbox);
  for args in [&["--bogus"][..], &["--require-local-patterns", "--bogus"][..]] {
    let run = scan(&env, args);
    assert_eq!(run.code, 2, "{args:?}: {}", run.describe());
    assert!(run.stderr.contains("--bogus"), "{args:?}: {}", run.describe());
  }
}

#[test]
#[ignore = "integration"]
fn require_local_patterns_refuses_a_missing_empty_or_comments_only_file() {
  let sandbox = Sandbox::new();
  let mut env = pinned_git_env(&sandbox);
  let home = sandbox.dir("pattern-home");
  env.insert("CHARLIEBOT_HOME".to_string(), home.display().to_string());
  let file = home.join("skills_leak_patterns.local.txt");

  // Missing: the file was never written.
  let run = scan(&env, &["--require-local-patterns", "--stdin"]);
  assert_eq!(run.code, 2, "missing: {}", run.describe());

  // Empty, then comments-only: no active pattern.
  for contents in ["", "# only a comment\n\n  # another\n"] {
    std::fs::write(&file, contents).unwrap();
    let run = scan(&env, &["--require-local-patterns", "--stdin"]);
    assert_eq!(run.code, 2, "{contents:?}: {}", run.describe());
  }

  // Unreadable: the same refusal.
  std::fs::write(&file, format!("{PATTERN_TERM}\n")).unwrap();
  let mut perms = std::fs::metadata(&file).unwrap().permissions();
  use std::os::unix::fs::PermissionsExt;
  perms.set_mode(0o000);
  std::fs::set_permissions(&file, perms).unwrap();
  let run = scan(&env, &["--require-local-patterns", "--stdin"]);
  assert_eq!(run.code, 2, "unreadable: {}", run.describe());
  let mut perms = std::fs::metadata(&file).unwrap().permissions();
  perms.set_mode(0o644);
  std::fs::set_permissions(&file, perms).unwrap();

  // An active pattern passes the gate and scans.
  let run = scan(&env, &["--require-local-patterns", "--stdin"]);
  assert_eq!(run.code, 0, "active: {}", run.describe());
}

#[test]
#[ignore = "integration"]
fn the_list_modes_refuse_a_missing_list_or_target() {
  let sandbox = Sandbox::new();
  let mut env = pinned_git_env(&sandbox);
  let home = pattern_home(&sandbox);
  env.insert("CHARLIEBOT_HOME".to_string(), home.display().to_string());
  let values = sandbox.root().join("values");
  std::fs::write(&values, "").unwrap();
  let list = sandbox.root().join("list");
  std::fs::write(&list, format!("a field\t{}\n", sandbox.root().join("absent").display())).unwrap();

  for args in [
    vec!["--check-list", "/nonexistent"],
    vec!["--credential-check-list", values.to_str().unwrap(), "/nonexistent"],
    vec!["--check-list", list.to_str().unwrap()],
    vec!["--credential-check-list", values.to_str().unwrap(), list.to_str().unwrap()],
  ] {
    let run = scan(&env, &args);
    assert_eq!(run.code, 2, "{args:?}: {}", run.describe());
  }
}

#[test]
#[ignore = "integration"]
fn a_check_list_names_the_leaking_field() {
  let sandbox = Sandbox::new();
  let mut env = pinned_git_env(&sandbox);
  let home = pattern_home(&sandbox);
  env.insert("CHARLIEBOT_HOME".to_string(), home.display().to_string());
  let clean = body_file(&sandbox, "clean.txt", "clean text\n");
  let leaking = body_file(&sandbox, "leaking.txt", &format!("the {PATTERN_TERM} value\n"));
  let list = sandbox.root().join("list");
  std::fs::write(&list, format!("first\t{}\nsecond\t{}\n", clean.display(), leaking.display())).unwrap();
  let run = scan(&env, &["--require-local-patterns", "--check-list", list.to_str().unwrap()]);
  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(run.stdout.contains("Leak hit in second"), "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn host_patterns_only_skips_the_generic_rules() {
  let sandbox = Sandbox::new();
  let mut env = pinned_git_env(&sandbox);
  let home = pattern_home(&sandbox);
  env.insert("CHARLIEBOT_HOME".to_string(), home.display().to_string());

  // Generic leaks pass: the local patterns alone apply.
  let mut command = harness::command("bash", &env);
  command.arg(leak_scanner()).args(["--require-local-patterns", "--host-patterns-only", "--stdin"]);
  let run = run_with_stdin(&mut command, sandbox.root(), &format!("id {SESSION_ID}\n{HOME_PATH_TEXT}\n"));
  assert_eq!(run.code, 0, "{}", run.describe());

  // A local pattern still hits.
  let mut command = harness::command("bash", &env);
  command.arg(leak_scanner()).args(["--require-local-patterns", "--host-patterns-only", "--stdin"]);
  let run = run_with_stdin(&mut command, sandbox.root(), &format!("the {PATTERN_TERM} value\n"));
  assert_eq!(run.code, 1, "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_diff_path_view_line_is_admitted_and_a_diff_added_view_line_is_refused() {
  let sandbox = Sandbox::new();
  let (env, _gh_log, _home) = post(&sandbox);
  let mut command = harness::command("bash", &env);
  command.arg(leak_scanner()).arg("--stdin");
  let admitted = run_with_stdin(&mut command, sandbox.root(), &format!("View: /diff?session={SESSION_ID}\n"));
  assert_eq!(admitted.code, 0, "{}", admitted.describe());
  let refused = run_with_stdin(&mut command, sandbox.root(), &format!("+View: /diff?session={SESSION_ID}\n"));
  assert_eq!(refused.code, 1, "{}", refused.describe());
}

#[test]
#[ignore = "integration"]
fn a_corrupt_gz_in_file_mode_exits_2() {
  let sandbox = Sandbox::new();
  let (repo, env) = leak_repo(&sandbox);
  std::fs::write(repo.join("fixture.gz"), b"not a gzip member").unwrap();
  let run = scan_in(&env, &repo, &["fixture.gz"]);
  assert_eq!(run.code, 2, "{}", run.describe());
}

/// A scanner run inside `cwd`, for the file mode that reads the repository.
fn scan_in(env: &Env, cwd: &Path, args: &[&str]) -> Run {
  let mut command = harness::command("bash", env);
  command.arg(cwd.join("dev/check-skills-host-leak.sh")).args(args);
  run_in(&mut command, cwd)
}

// -- the hooks' pattern requirements --

/// The hook environment of `leak_repo`, re-pointed at `patterns` as the only pattern file.
fn extra_patterns_env(env: &Env, patterns: &Path) -> Env {
  let mut env = env.clone();
  env.insert("EXTRA_PATTERNS_FILE".to_string(), patterns.display().to_string());
  env
}

#[test]
#[ignore = "integration"]
fn the_commit_hooks_refuse_a_missing_or_empty_pattern_file() {
  let sandbox = Sandbox::new();
  let (repo, env) = leak_repo(&sandbox);
  std::fs::write(repo.join("note.txt"), "clean\n").unwrap();

  for (label, file) in [
    ("missing", sandbox.root().join("absent-patterns")),
    ("empty", body_file(&sandbox, "empty-patterns", "")),
    ("comments-only", body_file(&sandbox, "comment-patterns", "# nothing active\n")),
  ] {
    let env = extra_patterns_env(&env, &file);
    assert_eq!(leak_run(&repo, &env, &["add", "note.txt"]).code, 0, "{label}");
    let commit = leak_run(&repo, &env, &["commit", "-m", "clean message"]);
    assert_ne!(commit.code, 0, "{label}: {}", commit.describe());
    assert!(commit.stderr.contains("patterns file"), "{label}: {}", commit.describe());
    assert_eq!(leak_run(&repo, &env, &["reset", "-q"]).code, 0, "{label}");
  }
}

#[test]
#[ignore = "integration"]
fn the_commit_hooks_catch_a_local_pattern_in_the_diff_and_the_message() {
  let sandbox = Sandbox::new();
  let (repo, env) = leak_repo(&sandbox);

  // `secretmarker` is the pattern file's term.
  std::fs::write(repo.join("note.txt"), "text secretmarker\n").unwrap();
  assert_eq!(leak_run(&repo, &env, &["add", "note.txt"]).code, 0);
  let commit = leak_run(&repo, &env, &["commit", "-m", "clean"]);
  assert_ne!(commit.code, 0, "{}", commit.describe());
  assert!(commit.stderr.contains("note.txt"), "{}", commit.describe());
  assert_eq!(leak_run(&repo, &env, &["reset", "-q"]).code, 0);

  std::fs::write(repo.join("note.txt"), "clean\n").unwrap();
  assert_eq!(leak_run(&repo, &env, &["add", "note.txt"]).code, 0);
  let commit = leak_run(&repo, &env, &["commit", "-m", "about secretmarker"]);
  assert_ne!(commit.code, 0, "{}", commit.describe());
  assert!(commit.stderr.contains("commit message"), "{}", commit.describe());
}

#[test]
#[ignore = "integration"]
fn the_pre_push_hook_refuses_a_missing_or_empty_pattern_file() {
  let sandbox = Sandbox::new();
  let (env, _gh_log, _home) = post(&sandbox);
  let clone = hooked_clone(&sandbox, &env);
  for (label, file) in [
    ("missing", sandbox.root().join("absent-patterns")),
    ("comments-only", body_file(&sandbox, "comment-patterns", "# nothing active\n")),
  ] {
    let env = extra_patterns_env(&env, &file);
    let mut push = harness::command("git", &env);
    push.args(["push", "-q", "origin", "improve/perf/topic-slug"]);
    let run = run_in(&mut push, &clone);
    assert_ne!(run.code, 0, "{label}: {}", run.describe());
    assert!(
      !ls_remote(&extra_patterns_env(&env, &file), &clone).contains("improve/perf/topic-slug"),
      "{label}: nothing was pushed"
    );
  }
}

#[test]
#[ignore = "integration"]
fn the_pre_push_hook_catches_a_local_pattern_in_each_published_text() {
  let sandbox = Sandbox::new();
  let (env, _gh_log, _home) = post(&sandbox);
  // EXTRA_PATTERNS_FILE adds a second term on top of the home file's fictionalmarker.
  let extra = body_file(&sandbox, "extra-patterns", &format!("{PATTERN_TERM}\nextramarker\n"));
  let env = extra_patterns_env(&env, &extra);

  let clone = hooked_clone(&sandbox, &env);
  let push = |env: &Env, clone: &Path| {
    let mut command = harness::command("git", env);
    command.args(["push", "-q", "origin", "HEAD:refs/heads/scanned"]);
    run_in(&mut command, clone)
  };

  // A pattern in the diff.
  std::fs::write(clone.join("note.txt"), "carries extramarker\n").unwrap();
  git(&env, &clone, &["add", "note.txt"]);
  git(&env, &clone, &["commit", "-q", "-m", "clean"]);
  let run = push(&env, &clone);
  assert_ne!(run.code, 0, "diff: {}", run.describe());
  assert!(run.stdout.contains("diff"), "diff: {}", run.describe());

  // A pattern in a `*.gz` blob.
  git(&env, &clone, &["reset", "--hard", "-q", "HEAD~1"]);
  let mut command = harness::command("bash", &env);
  command.args(["-c", "printf 'compressed extramarker' | gzip -n > blob.gz"]);
  run_in(&mut command, &clone);
  git(&env, &clone, &["add", "blob.gz"]);
  git(&env, &clone, &["commit", "-q", "-m", "clean"]);
  let run = push(&env, &clone);
  assert_ne!(run.code, 0, "gz: {}", run.describe());
  assert!(run.stdout.contains("blob.gz"), "gz: {}", run.describe());

  // A pattern in the message.
  git(&env, &clone, &["reset", "--hard", "-q", "HEAD~1"]);
  git(&env, &clone, &["commit", "-q", "--allow-empty", "-m", "about extramarker"]);
  let run = push(&env, &clone);
  assert_ne!(run.code, 0, "message: {}", run.describe());

  // A pattern in the author identity.
  git(&env, &clone, &["reset", "--hard", "-q", "HEAD~1"]);
  let mut bad_env = env.clone();
  bad_env.insert("GIT_AUTHOR_EMAIL".to_string(), "extramarker@example.invalid".to_string());
  git(&bad_env, &clone, &["commit", "-q", "--allow-empty", "-m", "clean"]);
  let run = push(&bad_env, &clone);
  assert_ne!(run.code, 0, "identity: {}", run.describe());
  assert!(run.stdout.contains("identity"), "identity: {}", run.describe());
  git(&env, &clone, &["reset", "--hard", "-q", "HEAD~1"]);

  // A pattern in the pushed ref name.
  let mut bad_ref = harness::command("git", &env);
  bad_ref.args(["push", "-q", "origin", "HEAD:refs/heads/feat-extramarker"]);
  let run = run_in(&mut bad_ref, &clone);
  assert_ne!(run.code, 0, "ref name: {}", run.describe());
  assert!(run.stdout.contains("ref name"), "ref name: {}", run.describe());
  assert!(!ls_remote(&env, &clone).contains("scanned"), "nothing was pushed");
}

#[test]
#[ignore = "integration"]
fn the_pre_push_hook_catches_a_generic_leak_in_a_touched_doc() {
  let sandbox = Sandbox::new();
  let (env, _gh_log, _home) = post(&sandbox);
  let clone = hooked_clone(&sandbox, &env);
  std::fs::create_dir_all(clone.join("skills/demo")).unwrap();
  std::fs::write(clone.join("skills/demo/notes.md"), format!("{HOME_PATH_TEXT}\n")).unwrap();
  git(&env, &clone, &["add", "skills/demo/notes.md"]);
  git(&env, &clone, &["commit", "-q", "-m", "add notes"]);
  let mut push = harness::command("git", &env);
  push.args(["push", "-q", "origin", "HEAD:refs/heads/scanned"]);
  let run = run_in(&mut push, &clone);
  assert_ne!(run.code, 0, "{}", run.describe());
  assert!(run.stdout.contains("doc skills/demo/notes.md"), "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn the_pre_push_hook_refuses_a_corrupt_gz_blob() {
  let sandbox = Sandbox::new();
  let (env, _gh_log, _home) = post(&sandbox);
  let clone = hooked_clone(&sandbox, &env);
  std::fs::write(clone.join("blob.gz"), b"not a gzip member").unwrap();
  git(&env, &clone, &["add", "blob.gz"]);
  git(&env, &clone, &["commit", "-q", "-m", "the fixture"]);
  let mut push = harness::command("git", &env);
  push.args(["push", "-q", "origin", "HEAD:refs/heads/scanned"]);
  let run = run_in(&mut push, &clone);
  assert_ne!(run.code, 0, "{}", run.describe());
  assert!(run.stderr.contains("decompress"), "{}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_pre_push_credential_hit_writes_the_hit_file_and_github_post_passes_3() {
  let sandbox = Sandbox::new();
  let (env, _gh_log, _home) = post(&sandbox);
  let home = sandbox.root().join("charliebot-home");
  std::fs::write(home.join("credentials.yaml"), format!("planted:\n  api_key: {CREDENTIAL_VALUE}\n")).unwrap();
  let clone = hooked_clone(&sandbox, &env);
  std::fs::write(clone.join("bad.txt"), format!("the key is {CREDENTIAL_VALUE}\n")).unwrap();
  git(&env, &clone, &["add", "bad.txt"]);
  git(&env, &clone, &["commit", "-q", "-m", "clean"]);

  let hit_file = sandbox.root().join("credential-hit");
  let mut hooked = env.clone();
  hooked.insert("CHARLIEBOT_CREDENTIAL_HIT_FILE".to_string(), hit_file.display().to_string());
  let mut push = harness::command("git", &hooked);
  push.args(["push", "-q", "origin", "improve/perf/topic-slug"]);
  let refused = run_in(&mut push, &clone);
  assert_ne!(refused.code, 0, "{}", refused.describe());
  assert_credential_refusal_text(&refused, "credentials.yaml leaf planted.api_key");
  let hit = std::fs::read_to_string(&hit_file).expect("the hit file was written");
  assert!(hit.contains("credentials.yaml leaf planted.api_key"), "{hit}");
  assert!(!hit.contains(CREDENTIAL_VALUE), "the value is not written: {hit}");

  // The round's push passes the hook's credential refusal through as exit 3.
  let mut post = harness::command("bash", &env);
  post.arg(github_post()).args(["push", "-u", "origin", "improve/perf/topic-slug"]);
  let run = run_in(&mut post, &clone);
  assert_eq!(run.code, 3, "{}", run.describe());
  assert!(!ls_remote(&env, &clone).contains("improve/perf/topic-slug"), "nothing was pushed");
}

// -- github-post.sh push and the required pattern file --

#[test]
#[ignore = "integration"]
fn pr_writes_refuse_an_empty_or_comments_only_pattern_file() {
  let sandbox = Sandbox::new();
  let (env, gh_log, home) = post(&sandbox);
  let body = body_file(&sandbox, "body.md", CLEAN_BODY);
  for contents in ["", "# only a comment\n"] {
    std::fs::write(home.join("skills_leak_patterns.local.txt"), contents).unwrap();
    let mut comment = harness::command("bash", &env);
    comment.arg(github_post()).args(["pr-comment", "--body-file"]).arg(&body).arg("5");
    let run = run_in(&mut comment, sandbox.root());
    assert_eq!(run.code, 2, "{contents:?}: {}", run.describe());
    let mut create = harness::command("bash", &env);
    create
      .arg(github_post())
      .args(["pr-create", "--title", "clean", "--body-file"])
      .arg(&body)
      .args(["--base", "main"]);
    let run = run_in(&mut create, sandbox.root());
    assert_eq!(run.code, 2, "{contents:?}: {}", run.describe());
  }
  assert!(gh_log_lines(&gh_log).is_empty(), "nothing was sent");
}

#[test]
#[ignore = "integration"]
fn a_push_checks_a_non_head_refspec_and_its_gz_blobs() {
  let sandbox = Sandbox::new();
  let (env, _gh_log, _home) = post(&sandbox);
  let clone = push_repo(&sandbox, &env);

  // A side branch whose tip holds the pattern in its diff and in a `*.gz` blob; HEAD stays clean.
  git(&env, &clone, &["switch", "-c", "side"]);
  std::fs::write(clone.join("note.txt"), format!("carries {PATTERN_TERM}\n")).unwrap();
  let mut command = harness::command("bash", &env);
  command.args(["-c", "printf 'compressed text' | gzip -n > blob.gz"]);
  run_in(&mut command, &clone);
  git(&env, &clone, &["add", "."]);
  git(&env, &clone, &["commit", "-q", "-m", "the side change"]);
  git(&env, &clone, &["switch", "-q", "improve/perf/topic-slug"]);

  for spec in ["side:side", "side:main"] {
    let run = {
      let mut command = harness::command("bash", &env);
      command.arg(github_post()).args(["push", "origin", spec]);
      run_in(&mut command, &clone)
    };
    assert_eq!(run.code, 1, "{spec}: {}", run.describe());
    assert!(run.stdout.contains("Leak hit in commit"), "{spec}: {}", run.describe());
    assert!(!ls_remote(&env, &clone).contains("refs/heads/side"), "{spec}: nothing was pushed");
  }

  // The same refspec through a gz blob: rewrite the side tip so only the blob carries the term.
  git(&env, &clone, &["switch", "-q", "side"]);
  std::fs::write(clone.join("note.txt"), "clean\n").unwrap();
  let mut command = harness::command("bash", &env);
  command.args(["-c", &format!("printf 'compressed {PATTERN_TERM}' | gzip -n > blob.gz")]);
  run_in(&mut command, &clone);
  git(&env, &clone, &["add", "."]);
  git(&env, &clone, &["commit", "-q", "--amend", "-m", "the side change"]);
  git(&env, &clone, &["switch", "-q", "improve/perf/topic-slug"]);
  let mut command = harness::command("bash", &env);
  command.arg(github_post()).args(["push", "origin", "side:side"]);
  let run = run_in(&mut command, &clone);
  assert_eq!(run.code, 1, "gz: {}", run.describe());
  assert!(run.stdout.contains("blob.gz"), "gz: {}", run.describe());
}

#[test]
#[ignore = "integration"]
fn a_push_with_a_lone_remote_checks_the_current_branch() {
  let sandbox = Sandbox::new();
  let (env, _gh_log, _home) = post(&sandbox);
  let clone = push_repo(&sandbox, &env);
  std::fs::write(clone.join("bad.txt"), "bad\n").unwrap();
  git(&env, &clone, &["add", "bad.txt"]);
  git(&env, &clone, &["commit", "-q", "-m", &format!("follow {PATTERN_TERM}")]);

  let mut command = harness::command("bash", &env);
  command.arg(github_post()).args(["push", "-u", "origin"]);
  let run = run_in(&mut command, &clone);
  assert_eq!(run.code, 1, "{}", run.describe());
  assert!(!ls_remote(&env, &clone).contains("improve/perf/topic-slug"));
}
