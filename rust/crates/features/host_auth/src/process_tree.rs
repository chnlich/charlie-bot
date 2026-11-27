//! A readback's process tree: the process group its root leads, and the descendants that left that group. A readback that
//! its timeout ends takes the whole tree with it.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::process::Command;
use std::thread;
use std::time::Duration;

use charliebot_core::proc_identity::{parse_stat_fields, read_stat_text};

pub const SUPERVISOR_ARG: &str = "--host-auth-readback-supervisor";
pub const ROUTE_ERROR_PREFIX: &str = "\0host-auth-readback-route-error: ";

/// The helper entry the server uses to run a readback as a child subreaper.
pub fn helper_entry(args: &[OsString]) -> Option<u8> {
  if args.first().is_some_and(|arg| arg == SUPERVISOR_ARG) {
    Some(match supervise(&args[1..]) {
      Ok(code) => code,
      Err(error) => {
        eprintln!("{ROUTE_ERROR_PREFIX}supervisor failed: {error}");
        1
      }
    })
  } else {
    None
  }
}

/// The readback command's run, adopting any descendant whose own parent exits.
fn supervise(args: &[OsString]) -> io::Result<u8> {
  let Some((program, arguments)) = args.split_first() else {
    eprintln!("readback supervisor needs a command");
    return Ok(2);
  };
  // SAFETY: PR_SET_CHILD_SUBREAPER changes only this dedicated helper's process state.
  if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } != 0 {
    return Err(io::Error::last_os_error());
  }
  let status = match Command::new(program).args(arguments).status() {
    Ok(status) => status,
    Err(error) => {
      eprintln!("{ROUTE_ERROR_PREFIX}readback command could not start: {error}");
      return Ok(127);
    }
  };
  reap_adopted_children()?;
  let code = match status.code() {
    Some(code) => code,
    None => 128 + status.signal().expect("a signaled process has a signal number"),
  };
  Ok(code as u8)
}

/// The orphaned descendants reaped after the readback command ends.
fn reap_adopted_children() -> io::Result<()> {
  loop {
    let mut status = 0;
    // SAFETY: waitpid only updates the local status for a child of this dedicated helper.
    let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
    if pid > 0 {
      continue;
    }
    if pid == 0 {
      thread::sleep(Duration::from_millis(10));
      continue;
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ECHILD) {
      return Ok(());
    }
    return Err(error);
  }
}

/// SIGKILL to every descendant of `root` and the process group `root` leads.
///
/// `root` must lead its own process group, as `spawn` makes it. The descendants are listed from `/proc` before any signal
/// goes out, because a killed parent hands its children to another process. A target already gone is not an error.
pub fn kill_tree(root: u32) -> io::Result<()> {
  let members = descendants(root);
  for pid in members? {
    send_sigkill(pid as libc::pid_t, false)?;
  }
  send_sigkill(root as libc::pid_t, true)?;
  Ok(())
}

/// SIGKILL to the process group `target` when `group`, else to the process `target`.
fn send_sigkill(target: libc::pid_t, group: bool) -> io::Result<()> {
  // SAFETY: kill and killpg send one signal. They touch only the kernel's process table.
  let sent = unsafe { if group { libc::killpg(target, libc::SIGKILL) } else { libc::kill(target, libc::SIGKILL) } };
  if sent == -1 {
    let error = io::Error::last_os_error();
    if error.raw_os_error() != Some(libc::ESRCH) {
      return Err(error);
    }
  }
  Ok(())
}

/// Every process below `root` by the parent links `/proc` reports, `root` excluded.
fn descendants(root: u32) -> io::Result<Vec<u32>> {
  let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
  for entry in fs::read_dir("/proc")? {
    let name = entry?.file_name();
    let Some(pid) = name.to_str().and_then(|name| name.parse::<u32>().ok()) else {
      continue;
    };
    if let Some(parent) = parent_of(pid)? {
      children.entry(parent).or_default().push(pid);
    }
  }
  let mut found = Vec::new();
  let mut frontier = vec![root];
  while let Some(parent) = frontier.pop() {
    for &child in children.get(&parent).into_iter().flatten() {
      found.push(child);
      frontier.push(child);
    }
  }
  Ok(found)
}

/// Whether a read of a `/proc/<pid>` entry failed because the process is gone. The entry is missing once the process is
/// reaped, and procfs answers ESRCH to a read of an entry whose task exited after the open. Every other error is a
/// failure for the caller.
fn is_gone(error: &io::Error) -> bool {
  error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH)
}

/// The parent of `pid` from `/proc/<pid>/stat`, or `None` once the process is gone.
fn parent_of(pid: u32) -> io::Result<Option<u32>> {
  let stat = match read_stat_text(pid) {
    Ok(stat) => stat,
    Err(error) if is_gone(&error) => return Ok(None),
    Err(error) => return Err(error),
  };
  let fields =
    parse_stat_fields(&stat).map_err(|error| io::Error::other(format!("the stat of process {pid} {error}")))?;
  Ok(Some(fields.parent_pid))
}

/// Whether `pid` still runs. A zombie has ended, so it does not count.
#[cfg(test)]
pub(crate) fn is_running(pid: u32) -> bool {
  match read_stat_text(pid) {
    Ok(stat) => parse_stat_fields(&stat).is_ok_and(|fields| fields.state != 'Z'),
    Err(error) if is_gone(&error) => false,
    Err(error) => panic!("reading the stat of process {pid} failed: {error}"),
  }
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::time::Duration;

  use tempfile::TempDir;

  use super::*;

  /// The pid a script wrote to `path`, once it has written it.
  async fn pid_in(path: &std::path::Path) -> u32 {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
      if let Ok(text) = fs::read_to_string(path)
        && let Ok(pid) = text.trim().parse()
      {
        return pid;
      }
      assert!(tokio::time::Instant::now() < deadline, "the script did not write {}", path.display());
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn kill_tree_ends_the_group_and_a_descendant_that_left_it() {
    let dir = TempDir::new().expect("temporary directory exists");
    let script = dir.path().join("tree.sh");
    // A background child stays in the group. A `setsid` child moves to a session of its own. Both outlive the shell.
    fs::write(&script, "sleep 600 & echo $! > \"$0.group\"\nsetsid sleep 600 & echo $! > \"$0.session\"\nwait\n")
      .expect("the script writes");
    let mut root = tokio::process::Command::new("/bin/sh")
      .arg(&script)
      .process_group(0)
      .stdin(std::process::Stdio::null())
      .stdout(std::process::Stdio::null())
      .stderr(std::process::Stdio::null())
      .spawn()
      .expect("the shell starts");
    let root_pid = root.id().expect("a child not yet waited for has a pid");
    let group_pid = pid_in(&dir.path().join("tree.sh.group")).await;
    let session_pid = pid_in(&dir.path().join("tree.sh.session")).await;
    assert!(is_running(group_pid) && is_running(session_pid), "both children run before the kill");

    kill_tree(root_pid).expect("the tree is killed");
    root.wait().await.expect("the shell is reaped");
    for pid in [group_pid, session_pid] {
      let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
      while is_running(pid) {
        assert!(tokio::time::Instant::now() < deadline, "process {pid} survived the kill");
        tokio::time::sleep(Duration::from_millis(10)).await;
      }
    }
  }

  const SUPERVISOR_TEST_ARGS: &str = "CHARLIEBOT_HOST_AUTH_SUPERVISOR_TEST_ARGS";

  #[test]
  fn supervisor_test_entry() {
    let Ok(encoded) = std::env::var(SUPERVISOR_TEST_ARGS) else {
      return;
    };
    let args: Vec<String> = serde_json::from_str(&encoded).expect("the supervisor test arguments decode");
    let args = args.into_iter().map(OsString::from).collect::<Vec<_>>();
    match supervise(&args) {
      Ok(code) => std::process::exit(code.into()),
      Err(error) => {
        eprintln!("readback supervisor test failed: {error}");
        std::process::exit(1);
      }
    }
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn kill_tree_ends_a_detached_descendant_after_its_parent_exits() {
    let dir = TempDir::new().expect("temporary directory exists");
    let pid_file = dir.path().join("detached.pid");
    let command = format!("setsid sh -c 'echo $$ > \"$1\"; exec sleep 600' sh '{}' & exit 0", pid_file.display());
    let encoded = serde_json::to_string(&["/bin/sh", "-c", &command]).expect("the supervisor arguments encode");
    let executable = std::env::current_exe().expect("the test executable exists");
    let mut helper = tokio::process::Command::new(executable)
      .args(["--exact", "process_tree::tests::supervisor_test_entry", "--nocapture"])
      .env(SUPERVISOR_TEST_ARGS, encoded)
      .process_group(0)
      .stdin(std::process::Stdio::null())
      .stdout(std::process::Stdio::null())
      .stderr(std::process::Stdio::null())
      .spawn()
      .expect("the subreaper helper starts");
    let helper_pid = helper.id().expect("a child not yet waited for has a pid");
    let child_pid = pid_in(&pid_file).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while parent_of(child_pid).expect("the parent reads") != Some(helper_pid) {
      assert!(tokio::time::Instant::now() < deadline, "the helper did not adopt detached child {child_pid}");
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(is_running(child_pid), "the detached child outlives its shell parent");

    kill_tree(helper_pid).expect("the helper and its adopted descendant are killed");
    helper.wait().await.expect("the helper is reaped");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while is_running(child_pid) {
      assert!(tokio::time::Instant::now() < deadline, "process {child_pid} survived the kill");
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  }

  #[test]
  fn a_read_error_marks_a_process_gone_only_for_a_missing_entry_or_esrch() {
    assert!(is_gone(&io::Error::from_raw_os_error(libc::ESRCH)), "ESRCH means the process exited");
    assert!(is_gone(&io::Error::from(io::ErrorKind::NotFound)), "a missing entry means the process was reaped");
    assert!(!is_gone(&io::Error::from_raw_os_error(libc::EACCES)), "a denied read is a failure");
    assert!(!is_gone(&io::Error::from_raw_os_error(libc::EIO)), "an I/O error is a failure");
    assert!(!is_gone(&io::Error::from(io::ErrorKind::PermissionDenied)), "a permission error is a failure");
  }
}
