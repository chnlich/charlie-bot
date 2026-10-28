//! The switch's test world: one runtime clone, home, release root, log directory and private tmux socket under a sandbox
//! of stub commands. Each test builds its own, so nothing touches ~/.charliebot, ~/charliebot-releases, the default
//! tmux server or the live port. A release binary is a copy of the fake release server, which answers the switch's
//! readback and probe routes; the systemd commands are shell stubs.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use charliebot_shell_harness::{self as harness, Env, Run, Sandbox, ScopeRecord};
use chrono::{SecondsFormat, Utc};
use nix::unistd::getuid;
use regex::Regex;
use serde_json::{Map, Value, json};
use sha1::{Digest as _, Sha1};
use sha2::Sha256;
use uuid::Uuid;

pub const ACCESS_KEY: &str = "switch-test-key";
pub const SESSION: &str = "11111111-2222-3333-4444-555555555555";

const GIT_IDENTITY: [(&str, &str); 4] = [
  ("GIT_AUTHOR_NAME", "switch-test"),
  ("GIT_AUTHOR_EMAIL", "switch-test@example.invalid"),
  ("GIT_COMMITTER_NAME", "switch-test"),
  ("GIT_COMMITTER_EMAIL", "switch-test@example.invalid"),
];

/// The three backends each build registers, with their type and whether that build lets them re-attach.
const BACKENDS_OF_BUILDS: [(&str, &str, bool); 3] =
  [("opencode", "opencode", false), ("antigravity", "antigravity", false), ("claude_code", "cc-claude", true)];

/// The backends that the test config names, with the types the scan maps them to.
const BACKEND_OPTIONS: &str = "backends:\n  options:\n  - id: oc-deepseek-flash\n    type: opencode\n  - id: agy-gemini-flash\n    type: antigravity\n  - id: claude-opus\n    type: cc-claude\n";

/// One test's runtime clone, home, release root, log directory and tmux socket. The clone starts at `old_sha`.
pub struct World {
  pub sandbox: Sandbox,
  pub clone: PathBuf,
  pub home: PathBuf,
  pub releases: PathBuf,
  pub logs: PathBuf,
  pub socket: String,
  pub env: Env,
  /// The build before the crate grouping: the agents crate layout.
  pub pre_sha: String,
  /// The release that `current` names: the runtime/agents layout, where OpenCode and agy cannot re-attach.
  pub old_sha: String,
  /// The target release: one crate per backend, where OpenCode and agy can re-attach.
  pub rust_sha: String,
  /// A later release: agy cannot re-attach.
  pub new_sha: String,
  pub port: u16,
  /// Processes the test started: the live runs' processes, the release and the improve loop's controller.
  pub live: Vec<Child>,
}

/// The layouts a backend registration takes across the builds.
#[derive(Clone, Copy)]
enum Layout {
  /// The agents crate, before the crate grouping.
  Pre,
  /// The runtime/agents crate, after the grouping and before the split.
  Old,
  /// Each backend's own crate.
  New,
}

impl World {
  pub fn new() -> Self {
    let sandbox = Sandbox::new();
    let clone = sandbox.dir("clone");
    let scripts = sandbox.dir("clone/scripts");
    let home = sandbox.dir("home");
    fs::create_dir_all(home.join("sessions")).expect("the home's sessions directory");
    let releases = sandbox.dir("releases");
    let logs = sandbox.dir("logs");
    let git_vars = git_env(&sandbox);

    copy_repo_file("scripts/start-server.sh", &scripts.join("start-server.sh"));
    copy_repo_file("scripts/switch-server.sh", &scripts.join("switch-server.sh"));
    // The launcher's preflight reads its expected limits from this directory.
    let systemd = scripts.join("systemd");
    fs::create_dir_all(&systemd).expect("the systemd directory");
    for unit in fs::read_dir(harness::repo_root().join("scripts/systemd")).expect("the unit files") {
      let unit = unit.expect("a unit file");
      harness::copy_file(&unit.path(), &systemd.join(unit.file_name()));
    }

    // The first build predates the crate grouping: one module per backend under the agents crate.
    for (module, backend_type, enabled) in BACKENDS_OF_BUILDS {
      write_registration(&clone, module, backend_type, enabled, Layout::Pre);
    }
    git(&git_vars, &clone, &["init", "-q", "-b", "main"]);
    git(&git_vars, &clone, &["add", "scripts", "rust"]);
    git(&git_vars, &clone, &["commit", "-q", "-m", "fake runtime clone"]);
    let pre_sha = git(&git_vars, &clone, &["rev-parse", "HEAD"]);

    // The grouping moves the agents crate under runtime/; the registrations keep their module layout.
    for (module, _, _) in BACKENDS_OF_BUILDS {
      remove_registration(&clone, module, Layout::Pre);
    }
    write_registration(&clone, "opencode", "opencode", false, Layout::Old);
    write_registration(&clone, "antigravity", "antigravity", false, Layout::Old);
    write_registration(&clone, "claude_code", "cc-claude", true, Layout::Old);
    git(&git_vars, &clone, &["add", "rust"]);
    git(&git_vars, &clone, &["commit", "-q", "-m", "move the crates into group directories"]);
    let old_sha = git(&git_vars, &clone, &["rev-parse", "HEAD"]);

    // The split moves each registration into its own crate, and OpenCode and agy start re-attaching.
    for (module, _, _) in BACKENDS_OF_BUILDS {
      remove_registration(&clone, module, Layout::Old);
    }
    write_registration(&clone, "opencode", "opencode", true, Layout::New);
    write_registration(&clone, "antigravity", "antigravity", true, Layout::New);
    write_registration(&clone, "claude_code", "cc-claude", true, Layout::New);
    git(&git_vars, &clone, &["add", "rust"]);
    git(&git_vars, &clone, &["commit", "-q", "-m", "move the backends into their own crates"]);
    let rust_sha = git(&git_vars, &clone, &["rev-parse", "HEAD"]);

    // A later build turns agy's re-attachment back off inside its own crate, so a switch away from a new-layout build
    // is read there too.
    write_registration(&clone, "antigravity", "antigravity", false, Layout::New);
    git(&git_vars, &clone, &["add", "rust"]);
    git(&git_vars, &clone, &["commit", "-q", "-m", "new release"]);
    let new_sha = git(&git_vars, &clone, &["rev-parse", "HEAD"]);

    let clone_text = clone.display().to_string();
    let origin = sandbox.root().join("origin.git");
    let origin_text = origin.display().to_string();
    git(&git_vars, sandbox.root(), &["clone", "-q", "--bare", clone_text.as_str(), origin_text.as_str()]);
    git(&git_vars, &clone, &["remote", "add", "origin", origin_text.as_str()]);
    git(&git_vars, &clone, &["checkout", "-q", old_sha.as_str()]);
    for sha in [old_sha.as_str(), pre_sha.as_str(), rust_sha.as_str(), new_sha.as_str()] {
      install_release(&releases, sha);
    }
    symlink(&old_sha, releases.join("current")).expect("the current release");

    // The launcher's slice preflight and its systemd-run scope, without the host's user manager.
    sandbox.install_systemd_stubs();
    let port = free_port();
    let config = format!("server:\n  host: 127.0.0.1\n  port: {port}\n{BACKEND_OPTIONS}");
    fs::write(home.join("config.yaml"), config).expect("the config");
    fs::write(home.join("credentials.yaml"), format!("charliebot:\n  access_key: {ACCESS_KEY}\n"))
      .expect("the credentials");

    let mut env = sandbox.env();
    env.insert("CHARLIEBOT_HOME".to_string(), home.display().to_string());
    env.insert("CHARLIEBOT_RELEASES_DIR".to_string(), releases.display().to_string());
    env.insert("CHARLIEBOT_LOG_DIR".to_string(), logs.display().to_string());
    // A long TMPDIR puts the sandbox's private TMUX_TMPDIR past the 108-byte unix socket path limit.
    env.insert("TMUX_TMPDIR".to_string(), "/tmp".to_string());
    let socket = format!("switch-test-{}", &Uuid::new_v4().simple().to_string()[..8]);
    env.insert("CHARLIEBOT_TMUX_SOCKET".to_string(), socket.clone());
    env.insert("CHARLIEBOT_SWITCH_STOP_WAIT_S".to_string(), "3".to_string());
    env.insert("CHARLIEBOT_SWITCH_READY_WAIT_S".to_string(), "30".to_string());
    env.remove("TMUX");
    env.remove("SYSTEMD_RUN_REFUSE_ALL");

    World {
      sandbox,
      clone,
      home,
      releases,
      logs,
      socket,
      env,
      pre_sha,
      old_sha,
      rust_sha,
      new_sha,
      port,
      live: Vec::new(),
    }
  }

  pub fn root(&self) -> &Path {
    self.sandbox.root()
  }

  pub fn switch_script(&self) -> PathBuf {
    self.clone.join("scripts/switch-server.sh")
  }

  /// Runs the clone's own switch command, as the operator does from the runtime clone.
  pub fn switch(&self, args: &[&str]) -> Run {
    let mut command = harness::command("bash", &self.env);
    command.arg(self.switch_script()).args(args);
    harness::run(&mut command)
  }

  /// The readback's adoption check, over the run records in `runs_file` and one launch's output.
  pub fn adopted(&self, runs_file: &Path, launch_output: &str) -> Run {
    let (launch_log, work) = self.launch_files(launch_output);
    let mut env = self.env.clone();
    env.insert("RUNS_FILE".to_string(), runs_file.display().to_string());
    env.insert("NEW_LOG".to_string(), launch_log.display().to_string());
    env.insert("NEW_OFFSET".to_string(), "0".to_string());
    env.insert("WORK".to_string(), work.display().to_string());
    let snippet = format!("source \"$1\" rust {} && adopted", self.rust_sha);
    let mut command = harness::command("bash", &env);
    command.args(["-c", snippet.as_str(), "bash"]).arg(repo_switch());
    harness::run(&mut command)
  }

  /// The forward path from the scan through the readback to the exit, with its stop, pane, server and probe steps
  /// stubbed out. `delegation` is the shell command that stands in for the delegation check. A restore touches a
  /// marker file and exits 2, so a test tells a restore from the switch's own exit.
  pub fn switch_forward(&self, run_pid: u32, launch_output: &str, delegation: &str) -> Run {
    let (launch_log, work) = self.launch_files(launch_output);
    let mut env = self.env.clone();
    env.insert("CLONE".to_string(), self.clone.display().to_string());
    env.insert("RUN_PID".to_string(), run_pid.to_string());
    env.insert("RESTORED".to_string(), self.root().join("restored").display().to_string());
    env.insert("RUNS_FILE".to_string(), self.root().join("switch.runs").display().to_string());
    env.insert("WANT".to_string(), format!("rust {}", self.rust_sha));
    env.insert("PORT".to_string(), self.port.to_string());
    env.insert("NEW_LOG".to_string(), launch_log.display().to_string());
    env.insert("NEW_OFFSET".to_string(), "0".to_string());
    env.insert("WORK".to_string(), work.display().to_string());
    env.insert("DELEGATION".to_string(), delegation.to_string());
    let script = r#"
source "$1" rust "$2"
REPO_ROOT="$CLONE"
OLD_IMPL="$3"
stop_old() { kill -KILL "$RUN_PID"; }
start_in_pane() { :; }
wait_ready() { :; }
serving() { :; }
probe_turn() { :; }
delegation_check() { $DELEGATION; }
restore() { touch "$RESTORED"; exit 2; }
switch_forward
"#;
    let mut command = harness::command("bash", &env);
    command.args(["-c", script, "bash"]).arg(repo_switch()).arg(&self.rust_sha).arg(format!("rust {}", self.old_sha));
    harness::run(&mut command)
  }

  /// The switch's probe turn, run in a shell against a stand-in server that answers with `behavior`. Returns the
  /// turn's result and the requests the server served past the version probe. Both probe waits are one second.
  pub fn probe_turn(&self, behavior: &[(&str, Value)]) -> (Run, Vec<String>) {
    let server = self.start_fake_server(behavior);
    let mut env = self.check_env();
    env.insert("CHARLIEBOT_SWITCH_PROBE_WAIT_S".to_string(), "1".to_string());
    env.insert("CHARLIEBOT_SWITCH_PROBE_CANCEL_WAIT_S".to_string(), "1".to_string());
    let snippet = format!("source \"$1\" rust {} && probe_turn", step_target_sha());
    let mut command = harness::command("bash", &env);
    command.args(["-c", snippet.as_str(), "bash"]).arg(repo_switch());
    let run = harness::run(&mut command);
    (run, server.calls())
  }

  /// The switch's delegation check, run in a shell against a stand-in server that answers the delegate, task and
  /// archive routes with `behavior`. The check's REPO_ROOT is the runtime clone, so the worktree the server makes is
  /// a real worktree of it. Returns the check's result and the requests the server served past the version probe.
  /// The delegation wait and the probe cancel wait are one second.
  pub fn delegation_check(&self, behavior: &[(&str, Value)]) -> (Run, Vec<String>) {
    // The check runs on a runtime clone, whose HEAD is attached to its branch; the world's clone is left detached at
    // the old release's commit, so the check puts it on main first.
    self.git(&self.clone, &["checkout", "-q", "main"]);
    let server = self.start_fake_server(behavior);
    let mut env = self.check_env();
    env.insert("CHARLIEBOT_SWITCH_DELEGATION_WAIT_S".to_string(), "1".to_string());
    env.insert("CHARLIEBOT_SWITCH_PROBE_CANCEL_WAIT_S".to_string(), "1".to_string());
    let snippet = r#"source "$1" rust "$2" && REPO_ROOT="$3" && delegation_check"#;
    let mut command = harness::command("bash", &env);
    command.args(["-c", snippet, "bash"]).arg(repo_switch()).arg(step_target_sha()).arg(&self.clone);
    let run = harness::run(&mut command);
    (run, server.calls())
  }

  /// The environment of one check that the script serves over the port: the home, the port, the key and the work
  /// directory, whose `body` file holds each answer.
  fn check_env(&self) -> Env {
    let work = self.root().join("work");
    fs::create_dir_all(&work).expect("the work directory");
    let mut env = self.env.clone();
    env.insert("PORT".to_string(), self.port.to_string());
    env.insert("KEY".to_string(), ACCESS_KEY.to_string());
    env.insert("WORK".to_string(), work.display().to_string());
    env
  }

  /// Starts one stand-in release server on the port over the home, with `behavior` on top of the defaults, and waits
  /// until it answers the version route. Its binary is a copy in the server directory, where its calls.log lands.
  pub fn start_fake_server(&self, behavior: &[(&str, Value)]) -> FakeServer {
    let dir = self.root().join("server");
    fs::create_dir_all(&dir).expect("the server directory");
    let binary = dir.join("charliebot-server");
    harness::copy_file(&fake_release_server(), &binary);
    write_behavior(&dir, behavior);
    let mut command = harness::command(&binary, &self.env);
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let child = harness::spawn(&mut command);
    wait_until(|| self.answers_version(), Duration::from_secs(10));
    FakeServer { child, dir }
  }

  /// Whether the version route answers 200 over the port with the access key. A port nobody listens on reads as no.
  fn answers_version(&self) -> bool {
    match TcpStream::connect(("127.0.0.1", self.port)) {
      Ok(_) => self.version_status() == 200,
      Err(_) => false,
    }
  }

  /// The worktrees beyond the clone and the branches beyond main that the clone still holds.
  pub fn left_behind(&self) -> Vec<String> {
    let clone = self.clone.canonicalize().expect("the clone").display().to_string();
    let worktrees = self.git(&self.clone, &["worktree", "list", "--porcelain"]);
    let mut left: Vec<String> = worktrees
      .lines()
      .filter(|line| line.starts_with("worktree ") && line != &format!("worktree {clone}"))
      .map(String::from)
      .collect();
    let branches = self.git(&self.clone, &["branch", "--format=%(refname:short)"]);
    left.extend(branches.lines().filter(|name| *name != "main").map(String::from));
    left
  }

  /// The switch's preflight, with the listener, the pane and the tmux step stubbed out: the listener runs
  /// `running_impl`, and the preflight takes `mode` and `target_sha`. Its live runs go to `preflight.runs`.
  pub fn stubbed_preflight(&self, running_impl: &str, mode: &str, target_sha: &str) -> Run {
    let script = r#"source "$1" "$2" ${3:+"$3"}
OLD_SOURCE="$4"
RUNS_FILE="$5"
listener_pids() { echo 999999; }
impl_of_pid() { echo "$OLD_SOURCE"; }
pane_of_pid() { echo '%1'; }
tm() { echo 999999; }
preflight
"#;
    let mut command = harness::command("bash", &self.env);
    command
      .args(["-c", script, "bash"])
      .arg(self.switch_script())
      .arg(mode)
      .arg(target_sha)
      .arg(running_impl)
      .arg(self.root().join("preflight.runs"));
    harness::run(&mut command)
  }

  /// One mode of the home helper. The scan takes the running build as `rust <sha>` words, as the test always passed
  /// them. The sourced script's REPO_ROOT is the clone, where the builds' registrations live.
  pub fn home_helper(&self, mode: &str) -> Run {
    let scan_impl = if mode == "scan" { format!(" rust {}", self.old_sha) } else { String::new() };
    let script = format!("source \"$1\" rust {} && REPO_ROOT=\"$3\" && home_helper \"$2\"{scan_impl}", self.rust_sha);
    let mut env = self.sandbox.env();
    env.insert("CHARLIEBOT_HOME".to_string(), self.home.display().to_string());
    // The helper binary is chosen from the release root, as the switch chooses it.
    env.insert("CHARLIEBOT_RELEASES_DIR".to_string(), self.releases.display().to_string());
    let mut command = harness::command("bash", &env);
    command.args(["-c", script.as_str(), "bash"]).arg(repo_switch()).arg(mode).arg(&self.clone);
    let run = harness::run(&mut command);
    assert_eq!(run.code, 0, "{}", run.describe());
    run
  }

  /// The live lines of a real scan of the home, recorded while the runs' processes still ran.
  pub fn scanned_runs_file(&self) -> PathBuf {
    let scan = self.home_helper("scan");
    let lines: Vec<String> = scan.stdout.lines().filter(|line| line.starts_with("live\t")).map(String::from).collect();
    self.runs_file(&lines)
  }

  /// The runs file the scan writes: one live line per recorded run.
  pub fn runs_file(&self, lines: &[String]) -> PathBuf {
    let path = self.root().join("switch.runs");
    let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
    fs::write(&path, text).expect("the runs file");
    path
  }

  /// Starts a sleeping process as a live run's process, in a process group of its own, and returns its pid.
  pub fn spawn_live(&mut self) -> u32 {
    let mut command = harness::command("sleep", &self.env);
    command.arg("300").process_group(0).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let child = harness::spawn(&mut command);
    let pid = child.id();
    self.live.push(child);
    pid
  }

  /// A Run record whose process is alive, as the server left it before a restart.
  pub fn seed_live_run(&mut self, run_id: &str, backend: Option<&str>) {
    let pid = self.spawn_live();
    let mut record = Map::new();
    record.insert("id".to_string(), json!(run_id));
    record.insert("session_id".to_string(), json!(SESSION));
    record.insert("kind".to_string(), json!("work"));
    record.insert("pid".to_string(), json!(pid));
    record.insert("pid_start".to_string(), json!(pid_start(pid)));
    record.insert("started_at".to_string(), json!(Utc::now().to_rfc3339_opts(SecondsFormat::Micros, false)));
    record.insert("ended_at".to_string(), Value::Null);
    if let Some(backend) = backend {
      record.insert("backend".to_string(), json!(backend));
    }
    let run_dir = self.home.join("sessions").join(SESSION).join("data/runs").join(run_id);
    fs::create_dir_all(&run_dir).expect("the run directory");
    fs::write(run_dir.join("metadata.json"), Value::Object(record).to_string()).expect("the run record");
  }

  /// The release that `current` names at the old sha, started as the server on the port, so the preflight knows the
  /// running build that the scan reads.
  pub fn start_release(&mut self) {
    let mut command = harness::command(self.releases.join(&self.old_sha).join("charliebot-server"), &self.env);
    command.stdout(Stdio::null()).stderr(Stdio::null());
    let child = harness::spawn(&mut command);
    self.live.push(child);
    wait_until(|| !self.listener_pids().is_empty(), Duration::from_secs(10));
  }

  /// Starts a pane on the private tmux server, with a shell in the clone, and returns the pane id.
  pub fn start_pane(&self) -> String {
    let clone = self.clone.display().to_string();
    self.tmux(&[
      "new-session",
      "-d",
      "-s",
      "switch",
      "-c",
      clone.as_str(),
      "-P",
      "-F",
      "#{pane_id}",
      "bash --noprofile --norc",
    ])
  }

  /// Types the clone's launcher into the pane, as the operator does, and waits for its server_ready line.
  pub fn start_server(&self, pane: &str) {
    let command = format!(
      "cd {} && env CHARLIEBOT_HOME={} CHARLIEBOT_RELEASES_DIR={} CHARLIEBOT_LOG_DIR={} ./scripts/start-server.sh",
      quote(&self.clone),
      quote(&self.home),
      quote(&self.releases),
      quote(&self.logs),
    );
    self.tmux(&["send-keys", "-t", pane, command.as_str(), "Enter"]);
    wait_until(|| self.latest_log().contains("server_ready"), Duration::from_secs(30));
  }

  /// The newest server log, which the launcher's symlink names.
  pub fn latest_log(&self) -> String {
    let latest = self.logs.join("server-latest.log");
    if !latest.exists() {
      return String::new();
    }
    String::from_utf8_lossy(&fs::read(latest).expect("the server log")).into_owned()
  }

  /// The pids that listen on the port.
  pub fn listener_pids(&self) -> BTreeSet<u32> {
    let filter = format!("sport = :{}", self.port);
    let mut command = harness::command("ss", &self.env);
    command.args(["-Hltnp", filter.as_str()]);
    let run = harness::run(&mut command);
    assert_eq!(run.code, 0, "{}", run.describe());
    let pid = Regex::new(r"pid=(\d+)").expect("the pid pattern");
    pid.captures_iter(&run.stdout).map(|found| found[1].parse().expect("a pid")).collect()
  }

  /// Whether the one listener on the port runs the binary of the `sha` release.
  pub fn serves_release(&self, sha: &str) -> bool {
    let binary = self.releases.join(sha).join("charliebot-server").canonicalize().expect("the release binary");
    let pids = self.listener_pids();
    assert_eq!(pids.len(), 1, "expected one listener on port {}, found {pids:?}", self.port);
    let pid = pids.first().expect("one listener");
    let raw = fs::read(format!("/proc/{pid}/cmdline")).expect("the listener's argv");
    raw.split(|byte| *byte == 0).filter(|arg| !arg.is_empty()).any(|arg| {
      let text = String::from_utf8_lossy(arg);
      Path::new(text.as_ref()).canonicalize().ok().as_deref() == Some(binary.as_path())
    })
  }

  /// The status of the version route, asked over the port with the access key.
  pub fn version_status(&self) -> u16 {
    let mut stream = TcpStream::connect(("127.0.0.1", self.port)).expect("connect to the server");
    stream.set_read_timeout(Some(Duration::from_secs(5))).expect("the read timeout");
    write!(
      stream,
      "GET /api/internal/version HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {ACCESS_KEY}\r\nConnection: close\r\n\r\n"
    )
    .expect("send the version request");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("the version response");
    response.split_whitespace().nth(1).expect("a status line").parse().expect("a numeric status")
  }

  /// Nothing changed: `current` names the old release, the clone is where it was, and the old server answers.
  pub fn assert_untouched(&self) {
    assert_eq!(fs::read_link(self.releases.join("current")).expect("current"), PathBuf::from(&self.old_sha));
    assert_eq!(self.git(&self.clone, &["rev-parse", "HEAD"]), self.old_sha);
    assert!(self.serves_release(&self.old_sha), "the old release does not serve");
    assert_eq!(self.version_status(), 200);
  }

  /// The scopes of the server launches: one unit per launch, each with the slice and the swap limit.
  pub fn assert_server_scope_records(&self, expected: usize) {
    let records = self.sandbox.scope_records();
    let server: Vec<&ScopeRecord> =
      records.iter().filter(|record| record.unit_name.starts_with("charliebot-server-")).collect();
    let names: BTreeSet<&str> = server.iter().map(|record| record.unit_name.as_str()).collect();
    assert_eq!(server.len(), expected, "server scopes");
    assert_eq!(names.len(), server.len(), "server scope names repeat");
    let unit = Regex::new(r"^charliebot-server-\d{8}_\d{6}-\d+$").expect("the unit pattern");
    for record in server {
      assert!(unit.is_match(&record.unit_name), "unit name {}", record.unit_name);
      assert!(record.arguments.contains(&format!("--unit={}", record.unit_name)), "{}", record.unit_name);
      assert!(record.arguments.iter().any(|argument| argument == "--slice=charliebot.slice"), "{}", record.unit_name);
      let limit = record.arguments.iter().position(|argument| argument == "-p").expect("the -p argument");
      assert_eq!(record.arguments[limit + 1], "MemorySwapMax=0", "{}", record.unit_name);
    }
  }

  /// Every file under the home, with its bytes.
  pub fn home_snapshot(&self) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    collect_files(&self.home, &self.home, &mut files);
    files
  }

  /// The run_finished fact that the readback reads for a run whose process had ended.
  pub fn record_outcome(&self, run_id: &str, outcome: &str) {
    record_outcome(&self.home, run_id, outcome);
  }

  /// A run's exit code, stdout and stderr, with the newest switch log.
  pub fn report(&self, run: &Run) -> String {
    let mut logs: Vec<PathBuf> = fs::read_dir(&self.logs)
      .expect("the log directory")
      .map(|entry| entry.expect("a log entry").path())
      .filter(|path| is_switch_log(path))
      .collect();
    logs.sort();
    let switch_log = match logs.last() {
      Some(path) => fs::read_to_string(path).expect("the switch log"),
      None => "(no switch log)".to_string(),
    };
    format!("{}\nswitch log:\n{switch_log}", run.describe())
  }

  /// Every live process the test started still runs.
  pub fn assert_live_running(&mut self) {
    for child in &mut self.live {
      assert!(child.try_wait().expect("poll a live process").is_none(), "a live process ended");
    }
  }

  /// A git command in `cwd`, with the fixtures' commit identity. A failing command fails the test.
  pub fn git(&self, cwd: &Path, args: &[&str]) -> String {
    git(&git_env(&self.sandbox), cwd, args)
  }

  fn tmux(&self, args: &[&str]) -> String {
    let mut command = harness::command("tmux", &self.env);
    command.arg("-L").arg(&self.socket).args(args);
    let run = harness::run(&mut command);
    assert_eq!(run.code, 0, "tmux {args:?}: {}", run.describe());
    run.stdout.trim().to_string()
  }

  /// The launch output the readback and the forward path read, and the work directory they write to.
  fn launch_files(&self, launch_output: &str) -> (PathBuf, PathBuf) {
    let launch_log = self.root().join("server.log");
    fs::write(&launch_log, launch_output).expect("the launch log");
    let work = self.root().join("work");
    fs::create_dir_all(&work).expect("the work directory");
    (launch_log, work)
  }
}

impl Drop for World {
  fn drop(&mut self) {
    // The server may never have started, so kill-server can fail; the socket file goes either way.
    let mut command = harness::command("tmux", &self.env);
    command.args(["-L", self.socket.as_str(), "kill-server"]).stdout(Stdio::null()).stderr(Stdio::null());
    if let Err(error) = command.status() {
      eprintln!("cannot run tmux kill-server: {error}");
    }
    let tmux_dir = Path::new(&self.env["TMUX_TMPDIR"]).join(format!("tmux-{}", getuid()));
    match fs::remove_file(tmux_dir.join(&self.socket)) {
      Ok(()) => {}
      Err(error) if error.kind() == ErrorKind::NotFound => {}
      Err(error) => eprintln!("cannot remove the tmux socket: {error}"),
    }
    for child in &mut self.live {
      stop(child);
    }
  }
}

/// One stand-in release server a test started: its process and the server directory, where its calls.log lands.
/// Dropping it stops the process.
pub struct FakeServer {
  child: Child,
  dir: PathBuf,
}

impl FakeServer {
  /// The requests the server served past the version probe, as "METHOD PATH" lines, oldest first.
  pub fn calls(&self) -> Vec<String> {
    let path = self.dir.join("calls.log");
    if !path.is_file() {
      return Vec::new();
    }
    fs::read_to_string(path).expect("the calls log").lines().map(String::from).collect()
  }
}

impl Drop for FakeServer {
  fn drop(&mut self) {
    stop(&mut self.child);
  }
}

/// Kills a process the test started and reaps it. A process that already ended is not an error.
pub fn stop(child: &mut Child) {
  if let Err(error) = child.kill() {
    eprintln!("cannot stop pid {}: {error}", child.id());
  }
  if let Err(error) = child.wait() {
    eprintln!("cannot reap pid {}: {error}", child.id());
  }
}

/// The repository's switch script: the tests source it for its functions and run it for its exit codes.
pub fn repo_switch() -> PathBuf {
  harness::repo_root().join("scripts/switch-server.sh")
}

pub fn fake_release_server() -> PathBuf {
  PathBuf::from(env!("CARGO_BIN_EXE_fake-release-server"))
}

pub fn fake_pane_server() -> PathBuf {
  PathBuf::from(env!("CARGO_BIN_EXE_fake-pane-server"))
}

/// The release's `charliebot` under test: the built binary that serves `switch-helper`.
pub fn fake_charliebot() -> PathBuf {
  PathBuf::from(env!("CARGO_BIN_EXE_fake-charliebot"))
}

/// The run_finished fact that a run's stop leaves in the session's chat events.
pub fn record_outcome(home: &Path, run_id: &str, outcome: &str) {
  let event = json!({
    "id": Uuid::new_v4().to_string(),
    "type": "run_finished",
    "actor": "system",
    "source_session_id": SESSION,
    "run_id": run_id,
    "outcome": outcome,
  });
  let path = home.join("sessions").join(SESSION).join("data/chat_events.jsonl");
  let mut events = OpenOptions::new().create(true).append(true).open(&path).expect("the chat events");
  writeln!(events, "{event}").expect("append the chat event");
}

/// Field 22 of /proc/<pid>/stat: the start time that pins one process instance.
pub fn pid_start(pid: u32) -> String {
  let stat = fs::read_to_string(format!("/proc/{pid}/stat")).expect("the process stat");
  stat.rsplit_once(')').expect("a stat line").1.split_whitespace().nth(19).expect("the start time").to_string()
}

/// Polls `condition` until it holds, and fails the test once `timeout` has passed.
pub fn wait_until(mut condition: impl FnMut() -> bool, timeout: Duration) {
  let deadline = Instant::now() + timeout;
  while !condition() {
    assert!(Instant::now() <= deadline, "condition not met within {timeout:?}");
    thread::sleep(Duration::from_millis(50));
  }
}

/// Writes a release's behavior: the defaults, with `overrides` on top.
pub fn write_behavior(dir: &Path, overrides: &[(&str, Value)]) {
  let mut behavior = behavior_defaults();
  for (name, value) in overrides {
    behavior.insert((*name).to_string(), value.clone());
  }
  fs::write(dir.join("behavior.json"), Value::Object(behavior).to_string()).expect("the behavior file");
}

fn behavior_defaults() -> Map<String, Value> {
  let Value::Object(defaults) = json!({
    "check_config": "ok",
    "adopt": true,
    "run_state": "running",
    "output": "message",
    "output_after_polls": 0,
    "cancel": "settles",
    "followup": "none",
    "message_status": 202,
    "archive_runs_delay": 0,
    "archive_runs_delay_after": 0,
    "ignore_sigterm": false,
    "delegate": "accepts",
    "task_state": "success",
    "worktree": "created",
    "task_error": "",
  }) else {
    unreachable!("an object literal");
  };
  defaults
}

/// A release directory with its binaries, library and checksums, as an install writes them.
pub fn install_release(releases: &Path, sha: &str) {
  let release = releases.join(sha);
  fs::create_dir_all(&release).expect("the release directory");
  harness::copy_file(&fake_release_server(), &release.join("charliebot-server"));
  harness::copy_file(&fake_charliebot(), &release.join("charliebot"));
  fs::write(release.join("libfake.so"), b"fake shared object").expect("the library");
  write_checksums(&release);
  write_behavior(&release, &[]);
}

/// Replaces a release's `charliebot` with a script of `body`, then refreshes the checksums the preflight verifies.
pub fn replace_charliebot(release: &Path, body: &str) {
  let path = release.join("charliebot");
  fs::write(&path, format!("#!/usr/bin/env bash\n{body}")).expect("the charliebot stub");
  fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("make the charliebot stub executable");
  write_checksums(release);
}

/// A release's `sha256sums.txt` over the files it holds.
fn write_checksums(release: &Path) {
  let sums: String = ["charliebot", "charliebot-server", "libfake.so"]
    .iter()
    .filter(|name| release.join(name).is_file())
    .map(|name| format!("{}  {name}\n", sha256_hex(&release.join(name))))
    .collect();
  fs::write(release.join("sha256sums.txt"), sums).expect("the checksums");
}

/// Writes one backend's registration in the layout that build has, with its `restart_reattach` declaration.
fn write_registration(clone: &Path, module: &str, backend_type: &str, enabled: bool, layout: Layout) {
  let path = registration_path(clone, module, layout);
  fs::create_dir_all(path.parent().expect("a parent")).expect("the registration directory");
  let text = format!(
    "pub fn register() {{\n  BackendTypeRegistration {{\n    type_name: \"{backend_type}\",\n    traits: BackendTraits {{\n      restart_reattach: {enabled},\n    }},\n  }};\n}}\n"
  );
  fs::write(path, text).expect("the registration");
}

fn remove_registration(clone: &Path, module: &str, layout: Layout) {
  fs::remove_file(registration_path(clone, module, layout)).expect("the registration to move");
}

fn registration_path(clone: &Path, module: &str, layout: Layout) -> PathBuf {
  match layout {
    Layout::Pre => clone.join("rust/crates/charliebot-agents/src/backends").join(module).join("mod.rs"),
    Layout::Old => clone.join("rust/crates/runtime/agents/src/backends").join(module).join("mod.rs"),
    Layout::New => clone.join("rust/crates/backends").join(module).join("src/lib.rs"),
  }
}

/// A git command in `cwd`. The output is trimmed, and a failing command fails the test.
fn git(env: &Env, cwd: &Path, args: &[&str]) -> String {
  let mut command = harness::command("git", env);
  command.arg("-C").arg(cwd).args(args);
  let run = harness::run(&mut command);
  assert_eq!(run.code, 0, "git {args:?} in {}: {}", cwd.display(), run.describe());
  run.stdout.trim().to_string()
}

fn git_env(sandbox: &Sandbox) -> Env {
  let mut env = sandbox.env();
  for (name, value) in GIT_IDENTITY {
    env.insert(name.to_string(), value.to_string());
  }
  env
}

/// Copies one file of the repository into the test's tree, keeping its mode.
fn copy_repo_file(relative: &str, to: &Path) {
  harness::copy_file(&harness::repo_root().join(relative), to);
}

fn free_port() -> u16 {
  let listener = TcpListener::bind(("127.0.0.1", 0)).expect("a free port");
  listener.local_addr().expect("the bound address").port()
}

fn sha256_hex(path: &Path) -> String {
  let bytes = fs::read(path).expect("a release file");
  format!("{:x}", Sha256::digest(bytes))
}

/// The sha1 of `text`, as a commit-shaped hex string.
pub fn sha1_hex(text: &[u8]) -> String {
  format!("{:x}", Sha1::digest(text))
}

/// The shell quoting of one path. The fixtures' paths hold no quote, so single quotes are exact.
pub fn quote(path: &Path) -> String {
  let text = path.display().to_string();
  assert!(!text.contains('\''), "a path with a quote: {text}");
  format!("'{text}'")
}

fn is_switch_log(path: &Path) -> bool {
  let name = path.file_name().unwrap_or(OsStr::new("")).to_string_lossy();
  name.starts_with("switch_") && name.ends_with(".log")
}

fn collect_files(base: &Path, dir: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
  for entry in fs::read_dir(dir).expect("a home directory") {
    let path = entry.expect("a home entry").path();
    if path.is_dir() {
      collect_files(base, &path, files);
    } else if path.is_file() {
      let relative = path.strip_prefix(base).expect("under the home").to_path_buf();
      files.insert(relative, fs::read(&path).expect("a home file"));
    }
  }
}

/// The step target sha: a shell that runs one switch step parses it, and no git check resolves it.
pub fn step_target_sha() -> String {
  sha1_hex(b"step target under test")
}
