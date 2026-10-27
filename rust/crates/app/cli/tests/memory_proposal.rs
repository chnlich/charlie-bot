use std::fs;
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
    let mut command = self.command(env!("CARGO_BIN_EXE_charliebot"), &self.root);
    command.arg("memory").args(args).env("CHARLIEBOT_HOME", &self.home);
    run_bounded(command)
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
