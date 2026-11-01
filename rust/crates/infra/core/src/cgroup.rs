//! Shared names and cleanup for per-session process cgroups.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;

pub const SESSION_CGROUP_PREFIX: &str = "charliebot-sess-";
/// The one leaf slice that holds every session cgroup. It carries the host's agent memory limit.
pub const AGENTS_SLICE: &str = "charliebot-agents.slice";
/// The control file that lists a cgroup's processes; a write to it moves a pid into the cgroup.
pub const PROCS_FILE_NAME: &str = "cgroup.procs";
/// The pre-split parent of the session slices. Nothing is created here; a leftover directory is still swept.
pub const RETIRED_APP_SLICE: &str = "app.slice";
/// The `oom_score_adj` of every agent process: a host-wide OOM picks an agent process before the server, which keeps
/// the default 0. The value is positive, which an unprivileged writer may set.
pub const SESSION_CGROUP_OOM_SCORE_ADJ: i64 = 500;
/// The `/proc/<pid>/exe` file names of the browser processes a session sweep ends. `exe` rather than `comm`, which
/// truncates to 15 characters.
const BROWSER_EXE_NAMES: [&str; 6] =
  ["chrome", "chromium", "chromium-browser", "chrome-headless-shell", "headless_shell", "chrome_crashpad_handler"];
/// How long a session browser sweep waits for the killed browsers to leave `cgroup.procs`, as the renderer close waits.
const SESSION_BROWSER_EXIT_WAIT: Duration = Duration::from_secs(5);

/// The test base a unit test installed over the real cgroup base, which tests cannot write.
static TEST_BASE_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

/// The delegated cgroup base that the session browser sweep and the cgroup cleanup resolve: this user's real base, or
/// the fake tree a unit test installed with [`use_test_cgroup_base_dir`].
pub fn session_cgroup_base_dir() -> PathBuf {
  TEST_BASE_DIR
    .lock()
    .expect("the test cgroup base lock is not poisoned")
    .clone()
    .unwrap_or_else(system_cgroup_base_dir)
}

/// Point [`session_cgroup_base_dir`] at `base_dir` until the returned guard drops: a unit test's private fake tree
/// takes the place of the user's delegated cgroup base.
#[doc(hidden)]
pub fn use_test_cgroup_base_dir(base_dir: PathBuf) -> TestBaseDirGuard {
  let previous = TEST_BASE_DIR.lock().expect("the test cgroup base lock is not poisoned").replace(base_dir);
  assert!(previous.is_none(), "a test cgroup base is already installed");
  TestBaseDirGuard
}

/// Restores the real cgroup base when dropped.
#[doc(hidden)]
pub struct TestBaseDirGuard;

impl Drop for TestBaseDirGuard {
  fn drop(&mut self) {
    *TEST_BASE_DIR.lock().expect("the test cgroup base lock is not poisoned") = None;
  }
}

/// Return this user's delegated systemd service directory, which holds the session slices.
pub fn system_cgroup_base_dir() -> PathBuf {
  let uid = unsafe { libc::getuid() };
  PathBuf::from(format!("/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service"))
}

/// Return the cgroup directory systemd gives the user slice `unit` under `base_dir`.
///
/// systemd nests a slice inside the slice each dash prefix of its name names (`a-b.slice` lives in
/// `a.slice/a-b.slice`). So the agents' leaf slice sits inside `charliebot.slice`, not beside it.
pub fn user_slice_dir(base_dir: &Path, unit: &str) -> PathBuf {
  let name = unit.strip_suffix(".slice").unwrap_or(unit);
  let parts: Vec<&str> = name.split('-').collect();
  let mut dir = base_dir.to_path_buf();
  for end in 1..=parts.len() {
    dir.push(format!("{}.slice", parts[..end].join("-")));
  }
  dir
}

/// Return the directory of the agents' leaf slice under `base_dir`, which holds every session cgroup.
pub fn agents_slice_dir(base_dir: &Path) -> PathBuf {
  user_slice_dir(base_dir, AGENTS_SLICE)
}

/// Return every directory a session cgroup may live under: the agents' slice and the retired slice.
pub fn session_slice_dirs(base_dir: &Path) -> [PathBuf; 2] {
  [agents_slice_dir(base_dir), user_slice_dir(base_dir, RETIRED_APP_SLICE)]
}

/// Return the cgroup directory name for one session.
pub fn session_cgroup_name(session_id: &str) -> String {
  let prefix: String = session_id.chars().take(8).collect();
  format!("{SESSION_CGROUP_PREFIX}{prefix}")
}

/// Join a session cgroup name to its base directory.
pub fn session_cgroup_path(base_dir: &Path, session_id: &str) -> PathBuf {
  base_dir.join(session_cgroup_name(session_id))
}

/// End every browser process in the session's cgroups and return how many were SIGKILLed; other processes in the
/// cgroup stay. The sweep waits at most [`SESSION_BROWSER_EXIT_WAIT`] for the killed pids to leave `cgroup.procs`,
/// and logs `session_browsers_ended` with the count.
pub fn end_session_browsers(base_dir: &Path, session_id: &str) -> usize {
  end_session_browsers_with_wait(base_dir, session_id, SESSION_BROWSER_EXIT_WAIT)
}

fn end_session_browsers_with_wait(base_dir: &Path, session_id: &str, wait: Duration) -> usize {
  let mut killed = Vec::new();
  for pid in session_procs(base_dir, session_id) {
    if killed.contains(&pid) || !is_browser_process(pid) {
      continue;
    }
    match kill_browser(pid) {
      Ok(()) => killed.push(pid),
      Err(error) => {
        tracing::warn!(target: "charliebot_agents", session_id, pid, error = %error, "session_browser_kill_failed")
      }
    }
  }
  let deadline = Instant::now() + wait;
  while Instant::now() < deadline && browsers_still_listed(base_dir, session_id, &killed) {
    thread::sleep(Duration::from_millis(50));
  }
  tracing::info!(target: "charliebot_agents", session_id, count = killed.len(), "session_browsers_ended");
  killed.len()
}

/// The pids in the session's `cgroup.procs` under every slice. A slice without the session's cgroup contributes none;
/// any other read error is logged, and that slice contributes none.
fn session_procs(base_dir: &Path, session_id: &str) -> Vec<u32> {
  let mut pids = Vec::new();
  for slice in session_slice_dirs(base_dir) {
    let procs_path = session_cgroup_path(&slice, session_id).join(PROCS_FILE_NAME);
    match fs::read_to_string(&procs_path) {
      Ok(procs) => pids.extend(procs.split_whitespace().filter_map(|entry| entry.parse::<u32>().ok())),
      Err(error) if error.kind() == io::ErrorKind::NotFound => {}
      Err(error) => {
        let path = procs_path.display();
        tracing::warn!(target: "charliebot_agents", session_id, %path, error = %error, "session_procs_read_failed")
      }
    }
  }
  pids
}

/// Whether `pid`'s `/proc/<pid>/exe` names one of [`BROWSER_EXE_NAMES`], with the kernel's ` (deleted)` marker of an
/// unlinked binary removed. A process that has no readable exe, such as one that just exited, is no browser.
fn is_browser_process(pid: u32) -> bool {
  fs::read_link(format!("/proc/{pid}/exe"))
    .ok()
    .and_then(|target| target.file_name()?.to_str().map(str::to_owned))
    .is_some_and(|name| {
      let name = name.strip_suffix(" (deleted)").unwrap_or(&name);
      BROWSER_EXE_NAMES.contains(&name)
    })
}

/// SIGKILL to `pid`; a process that is already gone is not an error.
fn kill_browser(pid: u32) -> io::Result<()> {
  let encoded = i32::try_from(pid).map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
  match signal::kill(Pid::from_raw(encoded), Signal::SIGKILL) {
    Ok(()) | Err(Errno::ESRCH) => Ok(()),
    Err(error) => Err(io::Error::other(error.to_string())),
  }
}

/// Whether any of `pids` is still listed in the session's `cgroup.procs`. A pid that no longer runs — a kill takes
/// effect even before its parent reaps it — has left for good.
fn browsers_still_listed(base_dir: &Path, session_id: &str, pids: &[u32]) -> bool {
  session_procs(base_dir, session_id).into_iter().any(|pid| pids.contains(&pid) && !has_exited(pid))
}

/// Whether `pid` no longer runs: its `/proc/<pid>/stat` is gone, or the kernel keeps the entry only as a zombie.
fn has_exited(pid: u32) -> bool {
  match crate::proc_identity::read_pid_stat(pid) {
    Some(stat) => stat.state == "Z",
    None => true,
  }
}

/// Remove the session's empty cgroup from every slice; a populated or unavailable directory stays in place. Returns
/// whether any directory was removed.
pub fn cleanup_session_cgroup(base_dir: &Path, session_id: &str) -> bool {
  let mut removed = false;
  for slice in session_slice_dirs(base_dir) {
    let path = session_cgroup_path(&slice, session_id);
    match fs::remove_dir(&path) {
      Ok(()) => {
        tracing::info!(target: "charliebot_agents", path = %path.display(), "session_cgroup_removed");
        removed = true;
      }
      Err(error) if error.kind() == io::ErrorKind::NotFound => {}
      Err(error) => {
        tracing::debug!(target: "charliebot_agents", path = %path.display(), error = %error, "session_cgroup_cleanup_retained");
      }
    }
  }
  removed
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::path::PathBuf;
  use std::process::{Child, Command};

  use super::*;

  /// A running copy of the system sleep binary named `name`, whose `/proc/<pid>/exe` file name is `name`. The copy
  /// needs no real cgroup: the sweep reads the fake tree's `cgroup.procs` and the process's `exe` link.
  fn sleep_stub(directory: &Path, name: &str) -> (Child, PathBuf) {
    let source = ["/bin/sleep", "/usr/bin/sleep"]
      .iter()
      .find(|path| Path::new(path).is_file())
      .expect("the sleep binary exists")
      .to_owned();
    let binary = directory.join(name);
    fs::copy(source, &binary).expect("the sleep binary copies");
    let child = charliebot_shell_harness::spawn(Command::new(&binary).arg("300"));
    (child, binary)
  }

  #[test]
  fn the_sweep_ends_the_browser_processes_and_leaves_the_others() {
    let directory = tempfile::tempdir().unwrap();
    let base = directory.path().join("cgroup-base");
    let slice = agents_slice_dir(&base);
    let cgroup = session_cgroup_path(&slice, "synthetic-session");
    fs::create_dir_all(&cgroup).unwrap();
    let (mut browser, _) = sleep_stub(directory.path(), "chrome");
    let (mut bystander, _) = sleep_stub(directory.path(), "sleeper");
    fs::write(cgroup.join("cgroup.procs"), format!("{}\n{}\n", browser.id(), bystander.id())).unwrap();

    let ended = end_session_browsers_with_wait(&base, "synthetic-session", Duration::from_secs(2));

    assert_eq!(ended, 1);
    assert!(browser.try_wait().unwrap().is_some(), "the chrome stub ended");
    assert!(bystander.try_wait().unwrap().is_none(), "the plain sleep stays");
    bystander.kill().unwrap();
    bystander.wait().unwrap();
  }

  #[test]
  fn the_sweep_recognizes_a_browser_whose_binary_the_kernel_marks_deleted() {
    let directory = tempfile::tempdir().unwrap();
    let (mut browser, binary) = sleep_stub(directory.path(), "chrome");
    let (mut bystander, _) = sleep_stub(directory.path(), "sleeper");
    fs::remove_file(&binary).expect("the running binary unlinks");

    assert!(is_browser_process(browser.id()), "the deleted binary's exe link still names chrome");
    assert!(!is_browser_process(bystander.id()), "a plain sleep is no browser");
    assert!(!is_browser_process(u32::MAX - 1), "a process without a readable exe is no browser");

    browser.kill().unwrap();
    browser.wait().unwrap();
    bystander.kill().unwrap();
    bystander.wait().unwrap();
  }

  #[test]
  fn session_names_use_the_first_eight_id_characters() {
    assert_eq!(session_cgroup_name("12345678-ignored"), "charliebot-sess-12345678");
  }

  #[test]
  fn user_slice_dir_nests_under_each_dash_prefix() {
    let root = tempfile::tempdir().unwrap();
    let base = root.path().join("user.slice/user-1001.slice/user@1001.service");
    assert_eq!(user_slice_dir(&base, "app.slice"), base.join("app.slice"));
    assert_eq!(
      user_slice_dir(&base, "charliebot-agents.slice"),
      base.join("charliebot.slice").join("charliebot-agents.slice")
    );
    assert_eq!(user_slice_dir(&base, "a-b-c.slice"), base.join("a.slice").join("a-b.slice").join("a-b-c.slice"));
  }

  #[test]
  fn session_cgroup_oom_priority_is_the_same_for_every_agent_process() {
    assert_eq!(SESSION_CGROUP_OOM_SCORE_ADJ, 500);
  }

  #[test]
  fn cleanup_removes_only_an_empty_session_group() {
    let base = tempfile::tempdir().unwrap();
    let slice = agents_slice_dir(base.path());
    fs::create_dir_all(&slice).unwrap();
    let path = session_cgroup_path(&slice, "synthetic-session");
    fs::create_dir(&path).unwrap();
    assert!(cleanup_session_cgroup(base.path(), "synthetic-session"));
    assert!(!path.exists());

    fs::create_dir(&path).unwrap();
    fs::write(path.join("member"), "occupied").unwrap();
    assert!(!cleanup_session_cgroup(base.path(), "synthetic-session"));
    assert!(path.is_dir());
  }
}
