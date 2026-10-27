use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;

const PROCESS_TIMEOUT: Duration = Duration::from_secs(20);
const TEST_PATH: &str = "/usr/bin:/bin";
const AUTHOR_NAME: &str = "Memory Proposal Test";
const AUTHOR_EMAIL: &str = "memory-proposal@example.test";
const FIXED_DATE: &str = "2026-10-27T12:34:56+00:00";
const MEMORY_TOPICS: &str =
  "profile resident\ncommunication resident\nworkflow resident\nrulings resident\nhost resident\ncharliebot\n";

struct Sandbox {
  _temp: TempDir,
  root: PathBuf,
  home: PathBuf,
  git_home: PathBuf,
}

struct Run {
  code: i32,
  stdout: String,
  stderr: String,
}

impl Sandbox {
  fn new() -> Self {
    let temp = tempfile::tempdir().expect("the test temp directory is created");
    let root = temp.path().to_path_buf();
    let home = root.join("home");
    let git_home = root.join("git-home");
    fs::create_dir_all(&home).expect("the CharlieBot home is created");
    fs::create_dir_all(&git_home).expect("the Git home is created");
    Self { _temp: temp, root, home, git_home }
  }

  fn store(&self) -> PathBuf {
    self.home.join("memory")
  }

  fn build_store(&self) -> PathBuf {
    let store = self.store();
    fs::create_dir_all(store.join("entries/profile")).expect("the entry directory is created");
    fs::create_dir_all(store.join("staging")).expect("the staging directory is created");
    fs::write(store.join("topics"), MEMORY_TOPICS).expect("the topics file is written");
    fs::write(store.join(".gitignore"), "staging/\n").expect("the Git ignore file is written");
    fs::write(
      store.join("entries/profile/dark-mode.md"),
      "---\nscope: user\ntopic: profile\naudience: master, worker\ntitle: Dark Mode\n---\nbody for dark-mode\n",
    )
    .expect("the base entry is written");
    self.git(&store, &["init", "-q", "-b", "main"]);
    self.git(&store, &["config", "user.email", "t@example.com"]);
    self.git(&store, &["config", "user.name", "t"]);
    self.git(&store, &["add", "-A"]);
    self.git(&store, &["commit", "-qm", "base"]);
    store
  }

  fn run_cli(&self, args: &[&str]) -> Run {
    self.run_cli_from(&self.root, args)
  }

  fn run_cli_from(&self, cwd: &Path, args: &[&str]) -> Run {
    let mut command = self.command(env!("CARGO_BIN_EXE_charliebot"), cwd);
    command.arg("memory").args(args).env("CHARLIEBOT_HOME", &self.home);
    run_bounded(command)
  }

  /// One `proposal commit` call; the message file is written under the home, outside the store and its worktree.
  fn commit(&self, path: &str, message: &str, extra: &[&str]) -> Run {
    let message_file = self.message_file(message);
    let message_file = message_file.to_str().expect("the message file path is UTF-8");
    let mut args = vec!["proposal", "commit", path, "--message-file", message_file];
    args.extend_from_slice(extra);
    self.run_cli(&args)
  }

  fn message_file(&self, message: &str) -> PathBuf {
    let directory = self.home.join("messages");
    fs::create_dir_all(&directory).expect("the message directory is created");
    let mut hasher = DefaultHasher::new();
    message.hash(&mut hasher);
    let path = directory.join(format!("{:x}.txt", hasher.finish()));
    fs::write(&path, message).expect("the message file is written");
    path
  }

  /// The head commit's full message, as `git log --format=%B` prints it.
  fn head_message(&self, worktree: &Path) -> String {
    self.git_output(worktree, &["log", "-1", "--format=%B"]).stdout
  }

  /// The commits the proposal holds ahead of the base.
  fn pr_commit_count(&self, store: &Path) -> usize {
    self.git(store, &["rev-list", "main..proposal"]).lines().count()
  }

  fn git(&self, cwd: &Path, args: &[&str]) -> String {
    let output = self.git_output(cwd, args);
    assert_eq!(output.code, 0, "git {} failed in {}: {}", args.join(" "), cwd.display(), output.stderr);
    output.stdout.trim().to_string()
  }

  fn git_output(&self, cwd: &Path, args: &[&str]) -> Run {
    let mut command = self.command("git", cwd);
    command.args(args);
    run_bounded(command)
  }

  fn command(&self, program: &str, cwd: &Path) -> Command {
    let mut command = Command::new(program);
    command
      .current_dir(cwd)
      .env_clear()
      .env("HOME", &self.git_home)
      .env("GIT_CONFIG_NOSYSTEM", "1")
      .env("GIT_AUTHOR_NAME", AUTHOR_NAME)
      .env("GIT_AUTHOR_EMAIL", AUTHOR_EMAIL)
      .env("GIT_COMMITTER_NAME", AUTHOR_NAME)
      .env("GIT_COMMITTER_EMAIL", AUTHOR_EMAIL)
      .env("GIT_AUTHOR_DATE", FIXED_DATE)
      .env("GIT_COMMITTER_DATE", FIXED_DATE)
      .env("PATH", TEST_PATH);
    command
  }

  fn proposal_worktree(store: &Path) -> PathBuf {
    let name = store.file_name().expect("the store has a file name").to_string_lossy();
    store.with_file_name(format!("{name}-proposal"))
  }

  fn entry_text(slug: &str, body: &str) -> String {
    let title = slug
      .split('-')
      .map(|part| {
        let mut chars = part.chars();
        let first = chars.next().expect("an entry slug has a character").to_uppercase().to_string();
        format!("{first}{}", chars.as_str())
      })
      .collect::<Vec<_>>()
      .join(" ");
    format!("---\nscope: user\ntopic: profile\ntitle: {title}\naudience: master, worker\n---\n{body}")
  }

  fn proposal_commit(&self, store: &Path, relative: &Path, text: &str, message: &str) -> String {
    let worktree = Self::proposal_worktree(store);
    self.write_commit(&worktree, relative, text, message)
  }

  fn live_commit(&self, store: &Path, relative: &Path, text: &str, message: &str) -> String {
    self.write_commit(store, relative, text, message)
  }

  fn write_commit(&self, repo: &Path, relative: &Path, text: &str, message: &str) -> String {
    let target = repo.join(relative);
    fs::create_dir_all(target.parent().expect("the fixture file has a parent"))
      .expect("the fixture parent directory is created");
    fs::write(&target, text).expect("the fixture file is written");
    let relative = relative.to_str().expect("the fixture path is UTF-8");
    self.git(repo, &["add", "--", relative]);
    self.git(repo, &["commit", "-qm", message]);
    self.git(repo, &["rev-parse", "HEAD"])
  }

  fn three_proposal_commits(&self, store: &Path) -> [String; 3] {
    [1, 2, 3].map(|number| {
      let relative = PathBuf::from(format!("entries/profile/numbered-{number}.md"));
      let text = Self::entry_text(&format!("numbered-{number}"), &format!("body {number}\n"));
      self.proposal_commit(store, &relative, &text, &format!("admit numbered {number}"))
    })
  }
}

fn run_bounded(mut command: Command) -> Run {
  let mut child = command
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("the test child process starts");
  let stdout = child.stdout.take().expect("stdout is piped");
  let stderr = child.stderr.take().expect("stderr is piped");
  let stdout_reader = thread::spawn(move || read_output(stdout));
  let stderr_reader = thread::spawn(move || read_output(stderr));
  let status = wait_bounded(&mut child);
  let stdout = stdout_reader.join().expect("the stdout reader finishes");
  let stderr = stderr_reader.join().expect("the stderr reader finishes");
  Run { code: status.code().expect("the process exits normally"), stdout, stderr }
}

fn read_output(mut pipe: impl Read) -> String {
  let mut bytes = Vec::new();
  pipe.read_to_end(&mut bytes).expect("the child output is read");
  String::from_utf8(bytes).expect("the child output is UTF-8")
}

fn wait_bounded(child: &mut Child) -> ExitStatus {
  let started = Instant::now();
  loop {
    if let Some(status) = child.try_wait().expect("the child status is readable") {
      return status;
    }
    if started.elapsed() > PROCESS_TIMEOUT {
      child.kill().expect("the overdue child is killed");
      child.wait().expect("the killed child is reaped");
      panic!("the child process exceeded {} seconds", PROCESS_TIMEOUT.as_secs());
    }
    thread::sleep(Duration::from_millis(10));
  }
}

fn fields(output: &str) -> Vec<(String, String)> {
  output
    .trim()
    .lines()
    .map(|line| {
      let (key, value) = line.split_once(": ").expect("a result line has a key and value");
      (key.to_string(), value.to_string())
    })
    .collect()
}

fn field<'a>(fields: &'a [(String, String)], key: &str) -> &'a str {
  fields.iter().find(|(name, _)| name == key).expect("the result has the requested field").1.as_str()
}

fn write_file(path: &Path, text: &str) {
  fs::create_dir_all(path.parent().expect("the file has a parent")).expect("the file's directory is created");
  fs::write(path, text).expect("the file is written");
}

/// A 30-word sentence: over the 25-word cap the prose check enforces.
fn long_sentence() -> &'static str {
  concat!(
    "The curator drafts each entry inside the proposal worktree so the diff against the base branch ",
    "shows exactly the lines still awaiting review before any change reaches the live store.",
  )
}

/// *count* short one-sentence bullet lines: a canon body of the v5-gap entry's shape.
fn body_lines(count: usize) -> String {
  (1..=count).map(|index| format!("- Body line {index} stands.\n")).collect()
}

/// One bullet line 121 terminal columns wide: the two-column "- " prefix plus 119 characters.
fn wide_line(character: char) -> String {
  format!("- {}", character.to_string().repeat(119))
}

fn negation_warning(word: &str, sentence: &str) -> String {
  format!(
    "negation or contrast '{word}': state the standing reality or the action to take, \
     and keep the fact it carried: {sentence}"
  )
}

fn first_chars(text: &str, count: usize) -> String {
  text.chars().take(count).collect()
}

#[test]
fn test_proposal_verb_on_a_fresh_home_creates_the_scaffold() {
  let sandbox = Sandbox::new();
  let result = sandbox.run_cli(&["proposal", "status"]);
  assert_eq!(result.code, 1);
  let store = sandbox.store();
  assert!(store.join(".git").exists());
  assert!(store.join("topics").is_file());
}

#[test]
fn test_first_open_creates_branch_and_worktree() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  let result = sandbox.run_cli(&["proposal", "open"]);
  assert_eq!(result.code, 0, "{}", result.stderr);
  let fields = fields(&result.stdout);
  assert_eq!(field(&fields, "ahead"), "0");
  assert_eq!(field(&fields, "base"), "main");
  assert_eq!(field(&fields, "dirty"), "no");
  let worktree = Sandbox::proposal_worktree(&store);
  assert_eq!(Path::new(field(&fields, "worktree")), worktree);
  assert!(worktree.is_dir());
  assert_eq!(
    sandbox.git(&store, &["rev-parse", "--verify", "-q", "refs/heads/proposal"]),
    sandbox.git(&store, &["rev-parse", "main"])
  );
  assert_eq!(sandbox.git(&worktree, &["symbolic-ref", "--short", "HEAD"]), "proposal");
}

#[test]
fn test_open_with_commits_ahead_and_unchanged_base_keeps_them() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  sandbox.run_cli(&["proposal", "open"]);
  sandbox.proposal_commit(
    &store,
    Path::new("entries/profile/new-entry.md"),
    &Sandbox::entry_text("new-entry", "body\n"),
    "add new",
  );
  let head_before = sandbox.git(&store, &["rev-parse", "proposal"]);
  let result = sandbox.run_cli(&["proposal", "open"]);
  assert_eq!(result.code, 0, "{}", result.stderr);
  let fields = fields(&result.stdout);
  assert_eq!(field(&fields, "ahead"), "1");
  assert_eq!(field(&fields, "head"), head_before);
}

#[test]
fn test_open_rebases_onto_moved_base() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  sandbox.run_cli(&["proposal", "open"]);
  sandbox.proposal_commit(
    &store,
    Path::new("entries/profile/rebased.md"),
    &Sandbox::entry_text("rebased", "body\n"),
    "add rebased",
  );
  let pr_head = sandbox.git(&store, &["rev-parse", "proposal"]);
  sandbox.live_commit(
    &store,
    Path::new("entries/profile/base-side.md"),
    &Sandbox::entry_text("base-side", "base body\n"),
    "advance base",
  );
  let new_base = sandbox.git(&store, &["rev-parse", "main"]);
  let result = sandbox.run_cli(&["proposal", "open"]);
  assert_eq!(result.code, 0, "{}", result.stderr);
  let fields = fields(&result.stdout);
  assert_eq!(field(&fields, "ahead"), "1");
  assert_eq!(sandbox.git(&store, &["rev-parse", "proposal~1"]), new_base);
  assert_ne!(sandbox.git(&store, &["rev-parse", "proposal"]), pr_head);
}

#[test]
fn test_open_conflicting_base_keeps_branch_at_old_sha() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  sandbox.run_cli(&["proposal", "open"]);
  let worktree = Sandbox::proposal_worktree(&store);
  fs::write(worktree.join("topics"), "profile resident PR edit\nworkflow resident\n")
    .expect("the proposal topics file is written");
  sandbox.git(&worktree, &["add", "--", "topics"]);
  sandbox.git(&worktree, &["commit", "-qm", "edit topics on the PR"]);
  let pr_head = sandbox.git(&store, &["rev-parse", "proposal"]);
  fs::write(store.join("topics"), "profile resident live edit\nworkflow resident\n")
    .expect("the live topics file is written");
  sandbox.git(&store, &["commit", "-qam", "conflicting base edit"]);
  let result = sandbox.run_cli(&["proposal", "open"]);
  assert_eq!(result.code, 1);
  assert!(result.stderr.contains("conflicted"));
  assert_eq!(sandbox.git(&store, &["rev-parse", "proposal"]), pr_head);
  assert_eq!(sandbox.git_output(&worktree, &["rev-parse", "-q", "--verify", "REBASE_HEAD"]).stdout.trim(), "");
  assert_eq!(sandbox.git(&worktree, &["status", "--porcelain"]), "");
}

#[test]
fn test_open_refuses_uncommitted_live_change_and_touches_nothing() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  fs::write(store.join("topics"), "profile resident\nworkflow resident\nhost resident\n")
    .expect("the live topics file is changed");
  let result = sandbox.run_cli(&["proposal", "open"]);
  assert_eq!(result.code, 1);
  assert!(result.stderr.contains("uncommitted"));
  assert!(!Sandbox::proposal_worktree(&store).exists());
  assert_eq!(sandbox.git_output(&store, &["rev-parse", "--verify", "-q", "refs/heads/proposal"]).stdout.trim(), "");
}

#[test]
fn test_open_refuses_detached_live_checkout() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  let head = sandbox.git(&store, &["rev-parse", "HEAD"]);
  sandbox.git(&store, &["checkout", "-q", "--detach", &head]);
  let result = sandbox.run_cli(&["proposal", "open"]);
  assert_eq!(result.code, 1);
  assert!(result.stderr.contains("not on a branch"));
  assert!(!Sandbox::proposal_worktree(&store).exists());
}

#[test]
fn test_open_foreign_worktree_refuses_and_creates_no_branch() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  let worktree = Sandbox::proposal_worktree(&store);
  fs::create_dir(&worktree).expect("the foreign worktree path is created");
  sandbox.git(&worktree, &["init", "-q", "-b", "main"]);
  sandbox.git(&worktree, &["config", "user.email", "t@example.com"]);
  sandbox.git(&worktree, &["config", "user.name", "t"]);
  fs::write(worktree.join("foreign.txt"), "mine\n").expect("the foreign file is written");
  sandbox.git(&worktree, &["add", "-A"]);
  sandbox.git(&worktree, &["commit", "-qm", "foreign"]);
  let result = sandbox.run_cli(&["proposal", "open"]);
  assert_eq!(result.code, 1);
  assert!(result.stderr.contains("is a worktree on 'main', not 'proposal'"));
  assert_eq!(sandbox.git_output(&store, &["rev-parse", "--verify", "-q", "refs/heads/proposal"]).stdout.trim(), "");
  assert_eq!(fs::read_to_string(worktree.join("foreign.txt")).expect("the foreign file is readable"), "mine\n");
}

#[test]
fn test_land_middle_commit_moves_live_head_and_keeps_the_rest() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  sandbox.run_cli(&["proposal", "open"]);
  let [_first, middle, _last] = sandbox.three_proposal_commits(&store);
  let old_base = sandbox.git(&store, &["rev-parse", "main"]);
  let result = sandbox.run_cli(&["proposal", "land", middle.as_str()]);
  assert_eq!(result.code, 0, "{}", result.stderr);
  let fields = fields(&result.stdout);
  assert_eq!(
    fields.iter().map(|(key, _)| key.as_str()).collect::<Vec<_>>(),
    ["landed", "commits", "remaining", "diff_path"]
  );
  assert_eq!(field(&fields, "commits"), "2");
  assert_eq!(field(&fields, "remaining"), "1");
  let old_short = sandbox.git(&store, &["rev-parse", "--short", &old_base]);
  let middle_short = sandbox.git(&store, &["rev-parse", "--short", &middle]);
  assert_eq!(field(&fields, "landed"), format!("{old_short}..{middle_short}"));
  assert_eq!(field(&fields, "diff_path"), format!("/diff?repo={}&base={old_base}&head={middle}", store.display()));
  assert_eq!(sandbox.git(&store, &["rev-parse", "HEAD"]), middle);
  assert_eq!(sandbox.git(&store, &["symbolic-ref", "--short", "HEAD"]), "main");
  assert_eq!(sandbox.git(&store, &["rev-list", "--count", &format!("{middle}..proposal")]), "1");
  sandbox.git(&store, &["cat-file", "-e", "proposal:entries/profile/numbered-3.md"]);
}

#[test]
fn test_land_refuses_sha_not_on_proposal() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  sandbox.run_cli(&["proposal", "open"]);
  sandbox.three_proposal_commits(&store);
  let live_head = sandbox.live_commit(
    &store,
    Path::new("entries/profile/base-side.md"),
    &Sandbox::entry_text("base-side", "base body\n"),
    "live-side",
  );
  let result = sandbox.run_cli(&["proposal", "land", &live_head]);
  assert_eq!(result.code, 1);
  assert!(result.stderr.contains("not on the 'proposal' branch"));
  assert_eq!(sandbox.git(&store, &["rev-parse", "main"]), live_head);
}

#[test]
fn test_land_refuses_dirty_live_checkout() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  sandbox.run_cli(&["proposal", "open"]);
  let [_first, middle, _last] = sandbox.three_proposal_commits(&store);
  let live_head = sandbox.git(&store, &["rev-parse", "HEAD"]);
  fs::write(store.join("topics"), "dirty edit\n").expect("the live topics file is changed");
  let result = sandbox.run_cli(&["proposal", "land", middle.as_str()]);
  assert_eq!(result.code, 1);
  assert!(result.stderr.contains("not clean"));
  assert_eq!(sandbox.git(&store, &["rev-parse", "HEAD"]), live_head);
}

#[test]
fn test_land_refuses_tree_failing_lint() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  sandbox.run_cli(&["proposal", "open"]);
  let relative = Path::new("entries/profile/no-audience.md");
  let text = Sandbox::entry_text("no-audience", "body\n").replace("audience: master, worker\n", "");
  let pr_head = sandbox.proposal_commit(&store, relative, &text, "admit no-audience");
  let live_head = sandbox.git(&store, &["rev-parse", "HEAD"]);
  let result = sandbox.run_cli(&["proposal", "land", &pr_head]);
  assert_eq!(result.code, 1);
  assert!(result.stderr.contains("store lint"));
  assert_eq!(sandbox.git(&store, &["rev-parse", "HEAD"]), live_head);
}

#[test]
fn test_status_prints_every_field() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  sandbox.run_cli(&["proposal", "open"]);
  sandbox.proposal_commit(
    &store,
    Path::new("entries/profile/statused.md"),
    &Sandbox::entry_text("statused", "body\n"),
    "add statused",
  );
  let result = sandbox.run_cli(&["proposal", "status"]);
  assert_eq!(result.code, 0, "{}", result.stderr);
  let fields = fields(&result.stdout);
  let head = sandbox.git(&store, &["rev-parse", "proposal"]);
  assert_eq!(
    fields.iter().map(|(key, _)| key.as_str()).collect::<Vec<_>>(),
    ["worktree", "base", "head", "ahead", "opened", "dirty", "diff_path"]
  );
  assert_eq!(Path::new(field(&fields, "worktree")), Sandbox::proposal_worktree(&store));
  assert_eq!(field(&fields, "base"), "main");
  assert_eq!(field(&fields, "head"), head);
  assert_eq!(field(&fields, "ahead"), "1");
  assert_eq!(
    field(&fields, "opened"),
    sandbox.git(&store, &["show", "--no-patch", "--date=short", "--format=%cd", &head])
  );
  assert_eq!(field(&fields, "dirty"), "no");
  assert_eq!(field(&fields, "diff_path"), format!("/diff?repo={}&base=main&head={head}", store.display()));
}

#[test]
fn test_status_and_land_build_diff_path_through_one_function() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  sandbox.run_cli(&["proposal", "open"]);
  let [_first, middle, last] = sandbox.three_proposal_commits(&store);
  let old_base = sandbox.git(&store, &["rev-parse", "main"]);
  let status = sandbox.run_cli(&["proposal", "status"]);
  assert_eq!(status.code, 0, "{}", status.stderr);
  let status_fields = fields(&status.stdout);
  assert_eq!(field(&status_fields, "diff_path"), format!("/diff?repo={}&base=main&head={last}", store.display()));
  let land = sandbox.run_cli(&["proposal", "land", middle.as_str()]);
  assert_eq!(land.code, 0, "{}", land.stderr);
  let land_fields = fields(&land.stdout);
  assert_eq!(field(&land_fields, "diff_path"), format!("/diff?repo={}&base={old_base}&head={middle}", store.display()));
}

#[test]
fn test_status_tracks_worktree_dirty_state() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  sandbox.run_cli(&["proposal", "open"]);
  let clean = sandbox.run_cli(&["proposal", "status"]);
  assert_eq!(field(&fields(&clean.stdout), "dirty"), "no");
  let worktree = Sandbox::proposal_worktree(&store);
  fs::write(worktree.join("entries/profile/dirtying.md"), "drafted, not committed\n")
    .expect("the proposal file is changed");
  let dirty = sandbox.run_cli(&["proposal", "status"]);
  assert_eq!(field(&fields(&dirty.stdout), "dirty"), "yes");
}

#[test]
fn test_status_dirty_dash_when_worktree_absent() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  sandbox.run_cli(&["proposal", "open"]);
  sandbox.proposal_commit(
    &store,
    Path::new("entries/profile/detached-worktree.md"),
    &Sandbox::entry_text("detached-worktree", "body\n"),
    "add",
  );
  let worktree = Sandbox::proposal_worktree(&store);
  sandbox.git(&store, &["worktree", "remove", worktree.to_str().expect("the worktree path is UTF-8")]);
  let result = sandbox.run_cli(&["proposal", "status"]);
  assert_eq!(result.code, 0, "{}", result.stderr);
  let fields = fields(&result.stdout);
  assert_eq!(field(&fields, "dirty"), "-");
  assert_eq!(field(&fields, "ahead"), "1");
}

#[test]
fn test_commit_accepts_new_entry_file() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  sandbox.run_cli(&["proposal", "open"]);
  let worktree = Sandbox::proposal_worktree(&store);
  write_file(&worktree.join("entries/profile/new-entry.md"), &Sandbox::entry_text("new-entry", "body\n"));
  let result = sandbox.commit("entries/profile/new-entry.md", "admit profile/new-entry (New Entry)", &[]);
  assert_eq!(result.code, 0, "{}", result.stderr);
  let fields = fields(&result.stdout);
  assert_eq!(field(&fields, "committed"), sandbox.git(&store, &["rev-parse", "proposal"]));
  assert_eq!(
    sandbox.git(&worktree, &["ls-tree", "-r", "HEAD", "--name-only"]).lines().collect::<Vec<_>>(),
    [".gitignore", "entries/profile/dark-mode.md", "entries/profile/new-entry.md", "topics"]
  );
}

#[test]
fn test_commit_accepts_existing_pr_entry_with_lines_appended() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  sandbox.run_cli(&["proposal", "open"]);
  let relative = "entries/profile/appended.md";
  let text = Sandbox::entry_text("appended", "first line\n");
  sandbox.proposal_commit(&store, Path::new(relative), &text, "first");
  let worktree = Sandbox::proposal_worktree(&store);
  write_file(&worktree.join(relative), &format!("{text}second line\n"));
  let result = sandbox.commit(relative, "second", &[]);
  assert_eq!(result.code, 0, "{}", result.stderr);
  assert!(fs::read_to_string(worktree.join(relative)).expect("the entry is readable").contains("second line"));
}

#[test]
fn test_commit_resolves_a_relative_message_file_against_the_caller_cwd() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  sandbox.run_cli(&["proposal", "open"]);
  let relative = "entries/profile/relative-message.md";
  let worktree = Sandbox::proposal_worktree(&store);
  write_file(&worktree.join(relative), &Sandbox::entry_text("relative-message", "body\n"));
  let caller = sandbox.root.join("caller");
  fs::create_dir_all(&caller).expect("the caller directory is created");
  fs::write(caller.join("message.txt"), "admit via a relative message file\n").expect("the message file is written");
  let result = sandbox.run_cli_from(&caller, &["proposal", "commit", relative, "--message-file", "message.txt"]);
  assert_eq!(result.code, 0, "{}", result.stderr);
  assert_eq!(sandbox.head_message(&worktree).trim(), "admit via a relative message file");
}

#[test]
fn test_commit_refuses_reworded_pr_line_and_lists_it() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  sandbox.run_cli(&["proposal", "open"]);
  let relative = "entries/profile/reworded.md";
  let text = Sandbox::entry_text("reworded", "alpha bravo\n");
  sandbox.proposal_commit(&store, Path::new(relative), &text, "first");
  let worktree = Sandbox::proposal_worktree(&store);
  write_file(&worktree.join(relative), &text.replace("alpha bravo", "alpha CHARLIE"));
  let result = sandbox.commit(relative, "reword", &[]);
  assert_eq!(result.code, 1);
  assert!(result.stderr.contains("alpha bravo"), "{}", result.stderr);
  assert_eq!(sandbox.pr_commit_count(&store), 1);
}

#[test]
fn test_commit_refuses_line_reworded_to_the_base_wording() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  sandbox.run_cli(&["proposal", "open"]);
  let relative = "entries/profile/swap.md";
  let base_text = Sandbox::entry_text("swap", "original body\n");
  sandbox.live_commit(&store, Path::new(relative), &base_text, "base side of swap");
  let worktree = Sandbox::proposal_worktree(&store);
  let reworded = Sandbox::entry_text("swap", "reworded body\n");
  sandbox.proposal_commit(&store, Path::new(relative), &reworded, "pr rewrites the line");
  write_file(&worktree.join(relative), &base_text);
  let result = sandbox.commit(relative, "restore", &[]);
  assert_eq!(result.code, 1);
  assert!(result.stderr.contains("reworded body"), "{}", result.stderr);
  assert_eq!(sandbox.pr_commit_count(&store), 1);
}

#[test]
fn test_commit_counts_line_occurrences_as_a_multiset() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  sandbox.run_cli(&["proposal", "open"]);
  let relative = "entries/profile/doubled.md";
  let text = Sandbox::entry_text("doubled", "dup line\ndup line\n");
  sandbox.proposal_commit(&store, Path::new(relative), &text, "first");
  let worktree = Sandbox::proposal_worktree(&store);
  write_file(&worktree.join(relative), &Sandbox::entry_text("doubled", "dup line\n"));
  let result = sandbox.commit(relative, "drop one", &[]);
  assert_eq!(result.code, 1);
  assert!(result.stderr.contains("dup line"), "{}", result.stderr);
}

#[test]
fn test_commit_refuses_deleted_pr_created_entry() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  sandbox.run_cli(&["proposal", "open"]);
  let relative = "entries/profile/doomed.md";
  sandbox.proposal_commit(&store, Path::new(relative), &Sandbox::entry_text("doomed", "keep me\n"), "first");
  let worktree = Sandbox::proposal_worktree(&store);
  fs::remove_file(worktree.join(relative)).expect("the entry is removed from the worktree");
  let result = sandbox.commit(relative, "delete", &[]);
  assert_eq!(result.code, 1);
  assert!(result.stderr.contains("keep me"), "{}", result.stderr);
  assert_eq!(sandbox.pr_commit_count(&store), 1);
}

#[test]
fn test_commit_refuses_a_new_sentence_over_25_words() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  sandbox.run_cli(&["proposal", "open"]);
  let relative = "entries/profile/long-sentence.md";
  let sentence = long_sentence();
  let worktree = Sandbox::proposal_worktree(&store);
  write_file(&worktree.join(relative), &Sandbox::entry_text("long-sentence", &format!("- {sentence}\n")));
  let result = sandbox.commit(relative, "admit", &[]);
  assert_eq!(result.code, 1);
  assert!(result.stderr.contains("prose check failed:"), "{}", result.stderr);
  assert!(result.stderr.contains(&format!("sentence over 25 words (30): {sentence}")), "{}", result.stderr);
  assert_eq!(sandbox.pr_commit_count(&store), 0);
}

#[test]
fn test_commit_ignores_an_unchanged_long_sentence_in_an_old_entry() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  let relative = "entries/profile/legacy-long.md";
  let (first, second) = long_sentence().split_once(" shows ").expect("the long sentence has a shows");
  let base_text = Sandbox::entry_text("legacy-long", &format!("- {first}\n  shows {second}\n"));
  sandbox.live_commit(&store, Path::new(relative), &base_text, "base holds the long sentence");
  sandbox.run_cli(&["proposal", "open"]);
  let worktree = Sandbox::proposal_worktree(&store);
  write_file(&worktree.join(relative), &format!("{base_text}- Short added line.\n"));
  let result = sandbox.commit(relative, "append", &[]);
  assert_eq!(result.code, 0, "{}", result.stderr);
  assert!(fs::read_to_string(worktree.join(relative)).expect("the entry is readable").contains("Short added line"));
}

#[test]
fn test_commit_refuses_a_new_sentence_with_a_semicolon() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  sandbox.run_cli(&["proposal", "open"]);
  let relative = "entries/profile/semicolon.md";
  let bullet = "The curator drafts the entry; the reviewer commits it.";
  let worktree = Sandbox::proposal_worktree(&store);
  write_file(&worktree.join(relative), &Sandbox::entry_text("semicolon", &format!("- {bullet}\n")));
  let result = sandbox.commit(relative, "admit", &[]);
  assert_eq!(result.code, 1);
  assert!(result.stderr.contains(&format!("semicolon joins clauses: {bullet}")), "{}", result.stderr);
  assert_eq!(sandbox.pr_commit_count(&store), 0);
}

#[test]
fn test_commit_allows_a_semicolon_inside_backticks() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  sandbox.run_cli(&["proposal", "open"]);
  let relative = "entries/profile/backtick-semicolon.md";
  let worktree = Sandbox::proposal_worktree(&store);
  write_file(&worktree.join(relative), &Sandbox::entry_text("backtick-semicolon", "- Run `a; b` twice.\n"));
  let result = sandbox.commit(relative, "admit", &[]);
  assert_eq!(result.code, 0, "{}", result.stderr);
  assert!(!result.stderr.contains("prose check"), "{}", result.stderr);
}

#[test]
fn test_commit_refuses_a_long_sentence_in_the_lead() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  sandbox.run_cli(&["proposal", "open"]);
  let relative = "entries/profile/long-lead.md";
  let sentence = long_sentence();
  let worktree = Sandbox::proposal_worktree(&store);
  write_file(
    &worktree.join(relative),
    &Sandbox::entry_text("long-lead", &format!("{sentence}\n- Short bullet stands.\n")),
  );
  let result = sandbox.commit(relative, "admit", &[]);
  assert_eq!(result.code, 1);
  assert!(result.stderr.contains(&format!("sentence over 25 words (30): {sentence}")), "{}", result.stderr);
  assert_eq!(sandbox.pr_commit_count(&store), 0);
}

#[test]
fn test_commit_refuses_a_semicolon_in_the_lead() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  sandbox.run_cli(&["proposal", "open"]);
  let relative = "entries/profile/semicolon-lead.md";
  let lead = "The curator drafts the entry; the reviewer commits it.";
  let worktree = Sandbox::proposal_worktree(&store);
  write_file(
    &worktree.join(relative),
    &Sandbox::entry_text("semicolon-lead", &format!("{lead}\n- Short bullet stands.\n")),
  );
  let result = sandbox.commit(relative, "admit", &[]);
  assert_eq!(result.code, 1);
  assert!(result.stderr.contains(&format!("semicolon joins clauses: {lead}")), "{}", result.stderr);
  assert_eq!(sandbox.pr_commit_count(&store), 0);
}

#[test]
fn test_commit_refuses_a_long_sentence_in_an_entry_without_bullets() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  sandbox.run_cli(&["proposal", "open"]);
  let relative = "entries/profile/no-bullets.md";
  let sentence = long_sentence();
  let (first, second) = sentence.split_once(" so the diff ").expect("the long sentence has the split point");
  let worktree = Sandbox::proposal_worktree(&store);
  write_file(&worktree.join(relative), &Sandbox::entry_text("no-bullets", &format!("{first}\nso the diff {second}\n")));
  let result = sandbox.commit(relative, "admit", &[]);
  assert_eq!(result.code, 1);
  assert!(result.stderr.contains(&format!("sentence over 25 words (30): {sentence}")), "{}", result.stderr);
  assert_eq!(sandbox.pr_commit_count(&store), 0);
}

#[test]
fn test_commit_ignores_an_unchanged_long_sentence_in_an_old_lead() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  let relative = "entries/profile/legacy-lead.md";
  let (first, second) = long_sentence().split_once(" shows ").expect("the long sentence has a shows");
  let base_text = Sandbox::entry_text("legacy-lead", &format!("{first}\nshows {second}\n- Legacy bullet stands.\n"));
  sandbox.live_commit(&store, Path::new(relative), &base_text, "base lead holds the long sentence");
  sandbox.run_cli(&["proposal", "open"]);
  let worktree = Sandbox::proposal_worktree(&store);
  write_file(&worktree.join(relative), &format!("{base_text}- Short added line.\n"));
  let result = sandbox.commit(relative, "append", &[]);
  assert_eq!(result.code, 0, "{}", result.stderr);
  assert!(fs::read_to_string(worktree.join(relative)).expect("the entry is readable").contains("Short added line"));
}

#[test]
fn test_commit_keeps_the_lead_apart_from_the_first_bullet() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  sandbox.run_cli(&["proposal", "open"]);
  let relative = "entries/profile/compliant-lead.md";
  let body = concat!(
    "Entries live under the store root. Each entry states one fact about the host and names the file that owns it\n",
    "- Run the store lint before each commit so a broken entry never reaches the live store.\n",
    "- Keep each bullet short.\n",
  );
  let worktree = Sandbox::proposal_worktree(&store);
  write_file(&worktree.join(relative), &Sandbox::entry_text("compliant-lead", body));
  let result = sandbox.commit(relative, "admit", &[]);
  assert_eq!(result.code, 0, "{}", result.stderr);
  assert!(!result.stderr.contains("prose check"), "{}", result.stderr);
}

#[test]
fn test_commit_refuses_a_body_grown_past_12_lines() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  let relative = "entries/profile/grown.md";
  let legacy: String = (1..=11).map(|index| format!("- Legacy line {index} stands.\n")).collect();
  let base_text = Sandbox::entry_text("grown", &legacy);
  sandbox.live_commit(&store, Path::new(relative), &base_text, "base body of 11 lines");
  sandbox.run_cli(&["proposal", "open"]);
  let worktree = Sandbox::proposal_worktree(&store);
  write_file(&worktree.join(relative), &format!("{base_text}- New line one stands.\n- New line two stands.\n"));
  let result = sandbox.commit(relative, "grow", &[]);
  assert_eq!(result.code, 1);
  assert!(result.stderr.contains("body has 13 lines (limit 12)"), "{}", result.stderr);
  assert_eq!(sandbox.pr_commit_count(&store), 0);
}

#[test]
fn test_commit_refuses_a_15_line_body_that_stays_at_15_lines() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  let relative = "entries/profile/rewritten-long.md";
  let legacy: String = (1..=15).map(|index| format!("- Legacy line {index} stands.\n")).collect();
  let base_text = Sandbox::entry_text("rewritten-long", &legacy);
  sandbox.live_commit(&store, Path::new(relative), &base_text, "base body of 15 lines");
  sandbox.run_cli(&["proposal", "open"]);
  let worktree = Sandbox::proposal_worktree(&store);
  write_file(
    &worktree.join(relative),
    &base_text.replace("- Legacy line 7 stands.\n", "- Line seven stands rewritten.\n"),
  );
  let result = sandbox.commit(relative, "rewrite", &[]);
  assert_eq!(result.code, 1);
  assert!(result.stderr.contains("body has 15 lines (limit 12)"), "{}", result.stderr);
  assert!(!result.stderr.contains("Line seven stands rewritten"), "{}", result.stderr);
  assert_eq!(sandbox.pr_commit_count(&store), 0);
}

#[test]
fn test_commit_refuses_a_body_line_over_120_columns() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  sandbox.run_cli(&["proposal", "open"]);
  let relative = "entries/profile/wide-line.md";
  let line = wide_line('a');
  let worktree = Sandbox::proposal_worktree(&store);
  write_file(&worktree.join(relative), &Sandbox::entry_text("wide-line", &format!("{line}\n")));
  let result = sandbox.commit(relative, "admit", &[]);
  assert_eq!(result.code, 1);
  assert!(result.stderr.contains("prose check failed:"), "{}", result.stderr);
  assert!(
    result.stderr.contains(&format!("body line over 120 columns (121): {}", first_chars(&line, 60))),
    "{}",
    result.stderr
  );
  assert_eq!(sandbox.pr_commit_count(&store), 0);
}

#[test]
fn test_commit_allows_a_body_line_of_exactly_120_columns() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  sandbox.run_cli(&["proposal", "open"]);
  let relative = "entries/profile/at-the-limit.md";
  let worktree = Sandbox::proposal_worktree(&store);
  write_file(&worktree.join(relative), &Sandbox::entry_text("at-the-limit", &format!("- {}\n", "b".repeat(118))));
  let result = sandbox.commit(relative, "admit", &[]);
  assert_eq!(result.code, 0, "{}", result.stderr);
  assert!(!result.stderr.contains("prose check"), "{}", result.stderr);
}

#[test]
fn test_commit_counts_a_cjk_character_as_two_columns() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  sandbox.run_cli(&["proposal", "open"]);
  let relative = "entries/profile/cjk-line.md";
  let line = "汉".repeat(61);
  assert_eq!(line.chars().count(), 61);
  let worktree = Sandbox::proposal_worktree(&store);
  write_file(&worktree.join(relative), &Sandbox::entry_text("cjk-line", &format!("{line}\n")));
  let result = sandbox.commit(relative, "admit", &[]);
  assert_eq!(result.code, 1);
  assert!(
    result.stderr.contains(&format!("body line over 120 columns (122): {}", first_chars(&line, 60))),
    "{}",
    result.stderr
  );
  assert_eq!(sandbox.pr_commit_count(&store), 0);
}

#[test]
fn test_commit_refuses_a_base_verbatim_line_over_120_columns() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  let relative = "entries/profile/legacy-wide.md";
  let line = wide_line('c');
  let base_text = Sandbox::entry_text("legacy-wide", &format!("{line}\n"));
  sandbox.live_commit(&store, Path::new(relative), &base_text, "base holds the wide line");
  sandbox.run_cli(&["proposal", "open"]);
  let worktree = Sandbox::proposal_worktree(&store);
  write_file(&worktree.join(relative), &format!("{base_text}- Short added line.\n"));
  let result = sandbox.commit(relative, "append", &[]);
  assert_eq!(result.code, 1);
  assert!(
    result.stderr.contains(&format!("body line over 120 columns (121): {}", first_chars(&line, 60))),
    "{}",
    result.stderr
  );
  assert_eq!(sandbox.pr_commit_count(&store), 0);
}

#[test]
fn test_commit_allows_a_wide_line_folded_into_continuation_lines() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  sandbox.run_cli(&["proposal", "open"]);
  let relative = "entries/profile/folded-line.md";
  let words = ["word"; 24];
  let first = format!("- {}", words[..12].join(" "));
  let second = format!("  {}", words[12..].join(" "));
  let worktree = Sandbox::proposal_worktree(&store);
  write_file(&worktree.join(relative), &Sandbox::entry_text("folded-line", &format!("{first}\n{second}\n")));
  let result = sandbox.commit(relative, "admit", &[]);
  assert_eq!(result.code, 0, "{}", result.stderr);
  assert!(!result.stderr.contains("prose check"), "{}", result.stderr);
}

#[test]
fn test_commit_allows_a_pure_deletion_from_a_50_line_body() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  let relative = "entries/profile/v5-gap.md";
  let base_text = Sandbox::entry_text("v5-gap", &body_lines(50));
  sandbox.live_commit(&store, Path::new(relative), &base_text, "canon body of 50 lines");
  sandbox.run_cli(&["proposal", "open"]);
  let worktree = Sandbox::proposal_worktree(&store);
  write_file(&worktree.join(relative), &base_text.replace("- Body line 25 stands.\n", ""));
  let result = sandbox.commit(relative, "trim", &[]);
  assert_eq!(result.code, 0, "{}", result.stderr);
  assert!(!result.stderr.contains("prose check"), "{}", result.stderr);
  assert_eq!(sandbox.pr_commit_count(&store), 1);
}

#[test]
fn test_commit_refuses_a_deletion_that_also_adds_a_line() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  let relative = "entries/profile/swapped-line.md";
  let base_text = Sandbox::entry_text("swapped-line", &body_lines(50));
  sandbox.live_commit(&store, Path::new(relative), &base_text, "canon body of 50 lines");
  sandbox.run_cli(&["proposal", "open"]);
  let worktree = Sandbox::proposal_worktree(&store);
  let edited = base_text.replace("- Body line 25 stands.\n", "- The replacement line stands.\n");
  write_file(&worktree.join(relative), &edited);
  let result = sandbox.commit(relative, "swap", &[]);
  assert_eq!(result.code, 1);
  assert!(result.stderr.contains("body has 50 lines (limit 12)"), "{}", result.stderr);
  assert_eq!(sandbox.pr_commit_count(&store), 0);
}

#[test]
fn test_commit_allows_trimming_a_13_line_body_to_12() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  let relative = "entries/profile/trim-to-limit.md";
  let base_text = Sandbox::entry_text("trim-to-limit", &body_lines(13));
  sandbox.live_commit(&store, Path::new(relative), &base_text, "canon body of 13 lines");
  sandbox.run_cli(&["proposal", "open"]);
  let worktree = Sandbox::proposal_worktree(&store);
  write_file(&worktree.join(relative), &base_text.replace("- Body line 7 stands.\n", ""));
  let result = sandbox.commit(relative, "trim", &[]);
  assert_eq!(result.code, 0, "{}", result.stderr);
  assert!(!result.stderr.contains("prose check"), "{}", result.stderr);
  assert_eq!(sandbox.pr_commit_count(&store), 1);
}

#[test]
fn test_commit_replace_pr_lines_commits_and_lists_the_replaced_lines() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  sandbox.run_cli(&["proposal", "open"]);
  let relative = "entries/profile/replaced.md";
  let text = Sandbox::entry_text("replaced", "alpha bravo\n");
  sandbox.proposal_commit(&store, Path::new(relative), &text, "first");
  let worktree = Sandbox::proposal_worktree(&store);
  write_file(&worktree.join(relative), &text.replace("alpha bravo", "alpha CHARLIE"));
  let result = sandbox.commit(relative, "replace", &["--replace-pr-lines"]);
  assert_eq!(result.code, 0, "{}", result.stderr);
  let fields = fields(&result.stdout);
  assert_eq!(field(&fields, "committed"), sandbox.git(&store, &["rev-parse", "proposal"]));
  assert_eq!(sandbox.head_message(&worktree), "replace\nReplaced PR lines:\n  alpha bravo\n\n");
}

#[test]
fn test_commit_refuses_a_prose_violation_even_with_replace_pr_lines() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  sandbox.run_cli(&["proposal", "open"]);
  let relative = "entries/profile/flagged.md";
  let text = Sandbox::entry_text("flagged", "alpha bravo\n");
  sandbox.proposal_commit(&store, Path::new(relative), &text, "first");
  let worktree = Sandbox::proposal_worktree(&store);
  let rewritten = Sandbox::entry_text("flagged", &format!("alpha CHARLIE\n- {}\n", long_sentence()));
  write_file(&worktree.join(relative), &rewritten);
  let result = sandbox.commit(relative, "replace", &["--replace-pr-lines"]);
  assert_eq!(result.code, 1);
  assert!(result.stderr.contains("prose check failed:"), "{}", result.stderr);
  assert!(result.stderr.contains("sentence over 25 words"), "{}", result.stderr);
  assert_eq!(sandbox.pr_commit_count(&store), 1);
}

#[test]
fn test_commit_warns_on_a_new_sentence_with_a_negation_word() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  sandbox.run_cli(&["proposal", "open"]);
  let relative = "entries/profile/warned.md";
  let sentence = "The proposal branch holds the draft, not the live store.";
  let worktree = Sandbox::proposal_worktree(&store);
  write_file(&worktree.join(relative), &Sandbox::entry_text("warned", &format!("- {sentence}\n")));
  let result = sandbox.commit(relative, "admit", &[]);
  assert_eq!(result.code, 0, "{}", result.stderr);
  let fields = fields(&result.stdout);
  assert_eq!(field(&fields, "committed"), sandbox.git(&store, &["rev-parse", "proposal"]));
  assert_eq!(result.stderr, format!("warning: {}\n", negation_warning("not", sentence)));
  assert_eq!(
    sandbox.head_message(&worktree),
    format!("admit\nNegation warnings:\n  {}\n\n", negation_warning("not", sentence))
  );
}

#[test]
fn test_commit_warns_on_a_changed_title_with_a_negation_word() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  sandbox.run_cli(&["proposal", "open"]);
  let relative = "entries/profile/retitled.md";
  let worktree = Sandbox::proposal_worktree(&store);
  write_file(
    &worktree.join(relative),
    "---\nscope: user\ntopic: profile\ntitle: The Draft Is Not The Store\n\
     audience: master, worker\n---\nbody stands.\n",
  );
  let result = sandbox.commit(relative, "admit", &[]);
  assert_eq!(result.code, 0, "{}", result.stderr);
  assert_eq!(result.stderr, format!("warning: {}\n", negation_warning("Not", "The Draft Is Not The Store")));
  assert!(sandbox.head_message(&worktree).contains("Negation warnings:"));
}

#[test]
fn test_commit_ignores_a_negation_word_inside_backticks() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  sandbox.run_cli(&["proposal", "open"]);
  let relative = "entries/profile/backtick-negation.md";
  let worktree = Sandbox::proposal_worktree(&store);
  write_file(&worktree.join(relative), &Sandbox::entry_text("backtick-negation", "- Run `git not-a-flag` twice.\n"));
  let result = sandbox.commit(relative, "admit", &[]);
  assert_eq!(result.code, 0, "{}", result.stderr);
  assert_eq!(result.stderr, "");
  assert!(!sandbox.head_message(&worktree).contains("Negation warnings"));
}

#[test]
fn test_commit_ignores_a_base_verbatim_sentence_with_a_negation_word() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  let relative = "entries/profile/legacy-not.md";
  let base_text = Sandbox::entry_text("legacy-not", "- The live store never holds the draft.\n");
  sandbox.live_commit(&store, Path::new(relative), &base_text, "base holds the negation");
  sandbox.run_cli(&["proposal", "open"]);
  let worktree = Sandbox::proposal_worktree(&store);
  write_file(&worktree.join(relative), &format!("{base_text}- Short added line.\n"));
  let result = sandbox.commit(relative, "append", &[]);
  assert_eq!(result.code, 0, "{}", result.stderr);
  assert_eq!(result.stderr, "");
}

#[test]
fn test_commit_warns_on_a_rewritten_pr_line_and_lists_both_trailer_sections() {
  let sandbox = Sandbox::new();
  let store = sandbox.build_store();
  sandbox.run_cli(&["proposal", "open"]);
  let relative = "entries/profile/replaced-warned.md";
  let old_line = "- The draft lives in the worktree, not the live store.";
  let new_line = "- The draft never lives in the live store.";
  sandbox.proposal_commit(
    &store,
    Path::new(relative),
    &Sandbox::entry_text("replaced-warned", &format!("{old_line}\n")),
    "first",
  );
  let worktree = Sandbox::proposal_worktree(&store);
  write_file(&worktree.join(relative), &Sandbox::entry_text("replaced-warned", &format!("{new_line}\n")));
  let result = sandbox.commit(relative, "replace", &["--replace-pr-lines"]);
  assert_eq!(result.code, 0, "{}", result.stderr);
  let fields = fields(&result.stdout);
  assert_eq!(field(&fields, "committed"), sandbox.git(&store, &["rev-parse", "proposal"]));
  let sentence = new_line.strip_prefix("- ").expect("the bullet has its marker");
  assert_eq!(result.stderr, format!("warning: {}\n", negation_warning("never", sentence)));
  assert_eq!(
    sandbox.head_message(&worktree),
    format!(
      "replace\nReplaced PR lines:\n  {old_line}\nNegation warnings:\n  {}\n\n",
      negation_warning("never", sentence)
    )
  );
}
