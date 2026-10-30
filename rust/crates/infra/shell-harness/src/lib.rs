//! Runs the repository's shell scripts for real, against stub commands, for the Rust integration tests.
//!
//! A test makes a `Sandbox`: a temporary root whose `bin/` holds its stubs and comes first on PATH. The test adds
//! stubs with `Sandbox::stub`, builds a command with `command` and runs it with `run`; `spawn` starts a background
//! process. The README section "Shell-script tests" shows the calls.

use std::collections::BTreeMap;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::Duration;

use wait_timeout::ChildExt;

/// The variables one command sees, all of them: a command built from it runs with exactly these.
pub type Env = BTreeMap<String, String>;

/// How long one `run` may take before the test fails.
pub const RUN_TIMEOUT: Duration = Duration::from_secs(120);

/// How many times `spawn` retries ETXTBSY, about two seconds at 10 ms each.
const ETXTBSY_ATTEMPTS: u32 = 200;

/// The agents' slice's `MemoryMax=80%`, the limit the committed unit file declares.
const AGENTS_MEMORY_PERCENT: u64 = 80;

const SYSTEMCTL: &str = r#"case "$*" in
  *charliebot-agents.slice*)
    echo "ActiveState=${SYSTEMCTL_ACTIVE_STATE:-active}"
    echo "MemoryMax=${SYSTEMCTL_MEMORY_MAX:-{memory_max}}"
    echo "MemorySwapMax=${SYSTEMCTL_MEMORY_SWAP_MAX:-0}"
    ;;
  *)
    exit 1
    ;;
esac
"#;

const SYSTEMD_RUN: &str = r#"if [ "${SYSTEMD_RUN_REFUSE_ALL:-0}" = "1" ]; then
  echo "stub systemd-run: start refused by test" >&2
  exit 1
fi
arguments=("$@")
separator=-1
for index in "${!arguments[@]}"; do
  if [ "${arguments[index]}" = "--" ]; then
    separator=$index
    break
  fi
done
unit_name=""
for argument in "$@"; do
  case "$argument" in
    --unit=*) unit_name=${argument#--unit=} ;;
  esac
done
records="${BASH_SOURCE[0]%/*}/systemd-run.records"
if [ -f "$records" ] && cut -f1 "$records" | grep -Fxq -- "$unit_name"; then
  echo "Failed to start transient scope unit: Unit $unit_name.scope was already loaded or has a fragment file." >&2
  exit 1
fi
printf '%s' "$unit_name" >> "$records"
for argument in "$@"; do
  printf '\t%s' "$argument" >> "$records"
done
printf '\n' >> "$records"
exec "${arguments[@]:separator+1}"
"#;

/// The repository root. This crate sits at rust/crates/infra/shell-harness, four levels below it.
pub fn repo_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../..").canonicalize().expect("the repository root exists")
}

/// A temporary root with `bin/`, the stub directory, first on PATH.
pub struct Sandbox {
  root: tempfile::TempDir,
  bin: PathBuf,
}

/// One scope the systemd-run stub started: its unit name and every argument it was given.
pub struct ScopeRecord {
  pub unit_name: String,
  pub arguments: Vec<String>,
}

/// What a finished command printed and how it ended.
pub struct Run {
  pub code: i32,
  pub stdout: String,
  pub stderr: String,
}

impl Run {
  /// The exit code, stdout and stderr, for an assertion message.
  pub fn describe(&self) -> String {
    format!("rc={}\nstdout:\n{}\nstderr:\n{}", self.code, self.stdout, self.stderr)
  }
}

impl Default for Sandbox {
  fn default() -> Self {
    Self::new()
  }
}

impl Sandbox {
  /// A new sandbox with an empty stub directory.
  pub fn new() -> Self {
    let root = tempfile::Builder::new().prefix("charliebot-script-").tempdir().expect("a temporary root");
    let bin = root.path().join("bin");
    fs::create_dir(&bin).expect("the stub directory");
    Self { root, bin }
  }

  /// The temporary root, removed when the sandbox is dropped.
  pub fn root(&self) -> &Path {
    self.root.path()
  }

  /// The stub directory.
  pub fn bin(&self) -> &Path {
    &self.bin
  }

  /// Creates `relative` under the root, with its parents, and returns its path.
  pub fn dir(&self, relative: &str) -> PathBuf {
    let path = self.root().join(relative);
    fs::create_dir_all(&path).unwrap_or_else(|error| panic!("create {}: {error}", path.display()));
    path
  }

  /// Writes a stub command: an executable bash script with `body`, in the stub directory.
  pub fn stub(&self, name: &str, body: &str) {
    let path = self.bin.join(name);
    fs::write(&path, format!("#!/usr/bin/env bash\n{body}")).unwrap_or_else(|error| panic!("write {name}: {error}"));
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("make the stub executable");
  }

  /// The environment of the test process, with the stub directory first on PATH.
  pub fn env(&self) -> Env {
    let mut env: Env = env::vars().collect();
    let inherited = env.get("PATH").cloned().expect("PATH is set");
    env.insert("PATH".to_string(), format!("{}:{inherited}", self.bin.display()));
    env
  }

  /// Installs the `systemctl` and `systemd-run` stubs the launcher calls. The agents' slice reports the limits its
  /// committed unit file declares, and the systemd-run stub records each scope it starts (see `scope_records`).
  pub fn install_systemd_stubs(&self) {
    let memory_max = memtotal_percent_bytes(AGENTS_MEMORY_PERCENT);
    self.stub("systemctl", &SYSTEMCTL.replace("{memory_max}", &memory_max.to_string()));
    self.stub("systemd-run", SYSTEMD_RUN);
  }

  /// The scopes the systemd-run stub started, oldest first.
  pub fn scope_records(&self) -> Vec<ScopeRecord> {
    let path = self.bin.join("systemd-run.records");
    if !path.exists() {
      return Vec::new();
    }
    let text = fs::read_to_string(&path).expect("read the scope records");
    text
      .lines()
      .map(|line| {
        let mut fields = line.split('\t');
        let unit_name = fields.next().unwrap_or_default().to_string();
        ScopeRecord { unit_name, arguments: fields.map(String::from).collect() }
      })
      .collect()
  }
}

/// A program cargo built for another workspace package, in the target directory that holds this test binary.
///
/// `CARGO_BIN_EXE_<name>` names only the test package's own binaries, so a test that runs another package's program
/// finds it beside the running test binary instead.
pub fn built_binary(name: &str) -> PathBuf {
  let executable = env::current_exe().expect("the test binary path");
  executable
    .ancestors()
    .skip(1)
    .map(|directory| directory.join(name))
    .find(|candidate| candidate.is_file())
    .unwrap_or_else(|| panic!("cargo builds {name} into the target directory that holds the test binary"))
}

/// A command for `program` that sees exactly the variables of `env`.
pub fn command(program: impl AsRef<OsStr>, env: &Env) -> Command {
  let mut command = Command::new(program);
  command.env_clear().envs(env);
  command
}

/// Runs `command` to its end, with an empty standard input and stdout and stderr captured. A command that outlives
/// `RUN_TIMEOUT` is killed, and the test fails.
pub fn run(command: &mut Command) -> Run {
  run_with_stdin(command, Stdio::null())
}

/// Runs `command` like [`run`], with `stdin` as its standard input. A file passed as `stdin` shares its offset with
/// every child that inherits it, so a child that reads the input first leaves nothing for the next one.
pub fn run_with_stdin(command: &mut Command, stdin: Stdio) -> Run {
  command.stdin(stdin).stdout(Stdio::piped()).stderr(Stdio::piped());
  let mut child = spawn(command);
  let mut stdout = child.stdout.take().expect("piped stdout");
  let mut stderr = child.stderr.take().expect("piped stderr");
  let out_reader = thread::spawn(move || read_text(&mut stdout));
  let err_reader = thread::spawn(move || read_text(&mut stderr));
  let Some(status) = child.wait_timeout(RUN_TIMEOUT).expect("wait for the command") else {
    // The kill fails only when the command has ended on its own, and the panic below reports the timeout either way.
    let _ = child.kill();
    let _ = child.wait();
    panic!("{:?} ran past {RUN_TIMEOUT:?}", command.get_program());
  };
  Run {
    code: exit_code(status),
    stdout: out_reader.join().expect("the stdout reader"),
    stderr: err_reader.join().expect("the stderr reader"),
  }
}

/// Starts `command` and returns the child. A file that another test thread is still writing can refuse to run with
/// ETXTBSY until that thread's fork has exec'd. The start therefore retries that error for a moment.
///
/// The child dies with the thread that starts it: `PR_SET_PDEATHSIG` makes the kernel send it SIGKILL when that
/// thread exits. A test thread that panics or is cancelled while its process still runs therefore leaves nothing
/// behind. Spawn from the test's own thread, not a helper thread that ends early.
pub fn spawn(command: &mut Command) -> Child {
  let parent = std::process::id() as libc::pid_t;
  // SAFETY: prctl, getppid and _exit are async-signal-safe, which is all a hook between fork and exec may call.
  unsafe {
    command.pre_exec(move || {
      if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
        return Err(std::io::Error::last_os_error());
      }
      // The kernel sends the signal only if the parent is still there at prctl. A child whose spawning process has
      // already exited was reparented without it, so it leaves now instead of outliving the test.
      if libc::getppid() != parent {
        libc::_exit(1);
      }
      Ok(())
    });
  }
  let mut attempts = 0;
  loop {
    match command.spawn() {
      Ok(child) => return child,
      Err(error) if error.raw_os_error() == Some(libc::ETXTBSY) && attempts < ETXTBSY_ATTEMPTS => {
        attempts += 1;
        thread::sleep(Duration::from_millis(10));
      }
      Err(error) => panic!("cannot start {:?}: {error}", command.get_program()),
    }
  }
}

/// Copies a file to `to`, keeping its mode: a built program stays executable.
pub fn copy_file(from: &Path, to: &Path) {
  fs::copy(from, to).unwrap_or_else(|error| panic!("copy {} to {}: {error}", from.display(), to.display()));
}

fn read_text(reader: &mut impl Read) -> String {
  let mut bytes = Vec::new();
  reader.read_to_end(&mut bytes).expect("read the command's output");
  String::from_utf8_lossy(&bytes).into_owned()
}

fn exit_code(status: ExitStatus) -> i32 {
  // A command that a signal ended reports 128 plus the signal number, as a shell does.
  status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

/// A percentage of host memory in bytes, computed the way systemd rounds it: down to a whole 4096-byte page, where a
/// page is MemTotal kB / 4.
pub fn memtotal_percent_bytes(percent: u64) -> u64 {
  let meminfo = fs::read_to_string("/proc/meminfo").expect("read /proc/meminfo");
  let total_kb: u64 = meminfo
    .lines()
    .find_map(|line| line.strip_prefix("MemTotal:"))
    .and_then(|rest| rest.split_whitespace().next())
    .and_then(|kb| kb.parse().ok())
    .expect("/proc/meminfo has a MemTotal line");
  ((total_kb / 4) * percent / 100) * 4096
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::mpsc;
  use std::time::Instant;

  /// libtest runs each test on a thread named for it, and `spawn`'s parent-death signal relies on that. The kernel
  /// signals a process when the thread that started it exits.
  #[test]
  fn a_test_runs_on_a_thread_named_for_it() {
    assert_eq!(thread::current().name(), Some("tests::a_test_runs_on_a_thread_named_for_it"));
  }

  /// A process started through `spawn` ends when the thread that started it ends, here by panicking.
  #[test]
  fn a_process_ends_when_the_thread_that_started_it_panics() {
    let (pid_tx, pid_rx) = mpsc::channel();
    let starter = thread::spawn(move || {
      let mut command = Command::new("sleep");
      command.arg("300").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
      pid_tx.send(spawn(&mut command).id()).expect("the thread reports its child");
      panic!("the starting thread ends while its process runs");
    });
    let pid = pid_rx.recv().expect("the child's pid");
    assert!(starter.join().is_err(), "the starting thread panicked");
    assert!(ends_within(pid, Duration::from_secs(2)), "pid {pid} outlived the thread that started it");
  }

  /// Whether `pid` exits within `timeout`. A process still running then is killed and reaped, so a failed test
  /// leaves nothing behind.
  fn ends_within(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
      let mut status = 0;
      // SAFETY: `pid` is a child of this process and `status` is a valid pointer.
      let reaped = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
      if reaped == pid as libc::pid_t {
        return true;
      }
      if Instant::now() > deadline {
        // SAFETY: `pid` is the process this test started.
        unsafe {
          libc::kill(pid as libc::pid_t, libc::SIGKILL);
          libc::waitpid(pid as libc::pid_t, &mut status, 0);
        }
        return false;
      }
      thread::sleep(Duration::from_millis(20));
    }
  }
}
