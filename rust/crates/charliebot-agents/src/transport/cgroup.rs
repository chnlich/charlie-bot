//! Session cgroups use one delegated directory per session.

use std::collections::HashSet;
use std::fs;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use charliebot_core::cgroup::SESSION_CGROUP_PREFIX;

static DEGRADED_WARNINGS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

/// The pre-spawn memory counter snapshot for one session cgroup.
#[derive(Clone, Debug)]
pub struct SessionCgroup {
  /// Session cgroup directory.
  pub path: PathBuf,
  /// Configured memory cap in megabytes.
  pub memory_max_mb: i64,
  /// `memory.events` counters recorded before spawn.
  pub events_before: Option<(u64, u64)>,
}

impl SessionCgroup {
  /// Classify an exited process against this spawn's memory event counters.
  pub fn classify_exit(&self, returncode: Option<i32>) -> Option<String> {
    classify_cgroup_exit(returncode, self.events_before, read_memory_events(&self.path), self.memory_max_mb)
  }
}

/// Return the delegated systemd `app.slice` directory for this user.
pub fn system_cgroup_base_dir() -> PathBuf {
  charliebot_core::cgroup::system_cgroup_base_dir()
}

/// Return the session cgroup directory name.
pub fn session_cgroup_name(session_id: &str) -> String {
  charliebot_core::cgroup::session_cgroup_name(session_id)
}

/// Join a session cgroup name to its caller-supplied base directory.
pub fn session_cgroup_path(base_dir: &Path, session_id: &str) -> PathBuf {
  charliebot_core::cgroup::session_cgroup_path(base_dir, session_id)
}

/// Create or refresh cgroup limits, returning `None` with one warning per error string on failure. A negative swap
/// limit is written as its negative number, which the kernel refuses; the spawn then runs without its cgroup.
pub fn ensure_session_cgroup(
  base_dir: &Path,
  session_id: &str,
  memory_max_mb: i64,
  swap_max_mb: i64,
  tasks_max: i64,
) -> Option<PathBuf> {
  let path = session_cgroup_path(base_dir, session_id);
  match fs::create_dir(&path) {
    Ok(()) => tracing::info!(
      target: "charliebot_agents",
      path = %path.display(),
      memory_max_mb,
      swap_max_mb,
      tasks_max,
      "session_cgroup_created"
    ),
    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
    Err(error) => {
      warn_cgroup_unavailable(&path, &error);
      return None;
    }
  }
  let writes = [
    ("memory.max", (memory_max_mb * 1024 * 1024).to_string()),
    ("memory.swap.max", (swap_max_mb * 1024 * 1024).to_string()),
    ("pids.max", if tasks_max > 0 { tasks_max.to_string() } else { "max".to_string() }),
  ];
  for (name, value) in writes {
    if let Err(error) = fs::write(path.join(name), value) {
      warn_cgroup_unavailable(&path, &error);
      return None;
    }
  }
  Some(path)
}

fn warn_cgroup_unavailable(path: &Path, error: &io::Error) {
  let key = error.to_string();
  let seen = DEGRADED_WARNINGS.get_or_init(|| Mutex::new(HashSet::new()));
  if seen.lock().expect("cgroup warning registry poisoned").insert(key.clone()) {
    tracing::warn!(target: "charliebot_agents", path = %path.display(), error = %key, "session_cgroup_unavailable");
  }
}

/// Read `(max, oom_kill)` from `memory.events`, or `None` when either counter is unavailable.
pub fn read_memory_events(cgroup_dir: &Path) -> Option<(u64, u64)> {
  let contents = fs::read_to_string(cgroup_dir.join("memory.events")).ok()?;
  let mut max = None;
  let mut oom_kill = None;
  for line in contents.lines() {
    let (key, raw) = match line.split_once(' ') {
      Some((key, raw)) => (key, raw),
      None if line == "max" || line == "oom_kill" => return None,
      None => continue,
    };
    if key == "max" {
      max = Some(raw.trim().parse().ok()?);
    } else if key == "oom_kill" {
      oom_kill = Some(raw.trim().parse().ok()?);
    }
  }
  Some((max?, oom_kill?))
}

/// Return a session-cap or host-OOM message for a SIGKILL, or `None` for other exits.
pub fn classify_cgroup_exit(
  returncode: Option<i32>,
  before: Option<(u64, u64)>,
  after: Option<(u64, u64)>,
  memory_max_mb: i64,
) -> Option<String> {
  if returncode != Some(-libc::SIGKILL) {
    return None;
  }
  let (Some(before), Some(after)) = (before, after) else {
    return None;
  };
  if after.0 > before.0 {
    Some(format!(
      "session 内存上限触发（上限 {memory_max_mb} MB），重任务请走集群三入口：gpuq＝集群任务队列提交，ssh gate＝用 ssh 在远端跑把关测试，远端启动＝charliebot 在远端启动长任务的命令"
    ))
  } else if after.1 > before.1 {
    Some("进程被宿主机全局 OOM 终止（session 内存上限未触发）".to_string())
  } else {
    None
  }
}

/// Ensure a session cgroup and snapshot its counters; disabled or unavailable cgroups return `None`. A memory limit of
/// zero or below disables the cgroup, and no directory is created.
pub fn prepare_session_cgroup(
  base_dir: &Path,
  session_id: Option<&str>,
  memory_max_mb: i64,
  swap_max_mb: i64,
  tasks_max: i64,
) -> Option<SessionCgroup> {
  let session_id = session_id?;
  if session_id.is_empty() || memory_max_mb <= 0 {
    return None;
  }
  let path = ensure_session_cgroup(base_dir, session_id, memory_max_mb, swap_max_mb, tasks_max)?;
  Some(SessionCgroup { events_before: read_memory_events(&path), path, memory_max_mb })
}

/// Move a spawned process into a cgroup; failures match the Python warning and do not fail spawn.
pub fn move_process_to_cgroup(pid: u32, cgroup: &SessionCgroup) {
  let path = cgroup.path.join("cgroup.procs");
  let result = OpenOptions::new().write(true).open(&path).and_then(|mut file| write!(file, "{pid}"));
  if let Err(error) = result {
    tracing::warn!(target: "charliebot_agents", pid, path = %path.display(), error = %error, "session_cgroup_move_failed");
  }
}

/// Remove an empty session cgroup; a populated or unavailable directory is retained.
pub fn cleanup_session_cgroup(base_dir: &Path, session_id: &str) -> bool {
  charliebot_core::cgroup::cleanup_session_cgroup(base_dir, session_id)
}

/// Remove empty stale session cgroups and retain nonempty ones with a warning.
pub fn sweep_stale_session_cgroups(base_dir: &Path) -> usize {
  let entries = match fs::read_dir(base_dir).and_then(|entries| entries.collect::<Result<Vec<_>, _>>()) {
    Ok(entries) => entries,
    Err(error) => {
      tracing::info!(target: "charliebot_agents", base = %base_dir.display(), error = %error, "session_cgroup_sweep_unavailable");
      return 0;
    }
  };
  let mut removed = 0;
  for entry in entries {
    let path = entry.path();
    let name = entry.file_name();
    if !name.to_string_lossy().starts_with(SESSION_CGROUP_PREFIX) || !path.is_dir() {
      continue;
    }
    match fs::remove_dir(&path) {
      Ok(()) => removed += 1,
      Err(error) => {
        tracing::warn!(target: "charliebot_agents", path = %path.display(), error = %error, "session_cgroup_sweep_retained")
      }
    }
  }
  if removed > 0 {
    tracing::info!(target: "charliebot_agents", count = removed, "session_cgroup_sweep_removed");
  }
  removed
}

/// Log whether session cgroups are enabled for the supplied base and limits. A memory limit of zero or below disables
/// them.
pub fn log_session_cgroup_startup(base_dir: &Path, memory_max_mb: i64, swap_max_mb: i64, tasks_max: i64) {
  if memory_max_mb <= 0 {
    tracing::info!(target: "charliebot_agents", reason = "server.session_memory_max_mb is 0", "session_cgroup_disabled");
  } else if !base_dir.is_dir() {
    tracing::info!(target: "charliebot_agents", reason = %format!("{} not present on this host", base_dir.display()), "session_cgroup_disabled");
  } else {
    let path = std::ffi::CString::new(base_dir.as_os_str().as_encoded_bytes()).expect("path has no NUL byte");
    if unsafe { libc::access(path.as_ptr(), libc::W_OK) } != 0 {
      tracing::info!(target: "charliebot_agents", reason = %format!("{} not writable by this user", base_dir.display()), "session_cgroup_disabled");
    } else {
      tracing::info!(target: "charliebot_agents", memory_max_mb, swap_max_mb, tasks_max, "session_cgroup_enabled");
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const SESSION_ID: &str = "abcd1234-ef56-7890-abcd-ef1234567890";

  #[test]
  fn ensure_writes_and_refreshes_all_three_limit_files() {
    let base = tempfile::tempdir().unwrap();
    let path = ensure_session_cgroup(base.path(), SESSION_ID, 64, 32, 50).unwrap();
    assert_eq!(path, base.path().join("charliebot-sess-abcd1234"));
    assert_eq!(fs::read_to_string(path.join("memory.max")).unwrap(), (64 * 1024 * 1024).to_string());
    assert_eq!(fs::read_to_string(path.join("memory.swap.max")).unwrap(), (32 * 1024 * 1024).to_string());
    assert_eq!(fs::read_to_string(path.join("pids.max")).unwrap(), "50");
    ensure_session_cgroup(base.path(), SESSION_ID, 64, 32, 0).unwrap();
    assert_eq!(fs::read_to_string(path.join("pids.max")).unwrap(), "max");
  }

  #[test]
  fn missing_base_degrades_and_memory_events_classify_cap_kills() {
    let base = tempfile::tempdir().unwrap().path().join("missing");
    assert!(ensure_session_cgroup(&base, SESSION_ID, 64, 32, 4096).is_none());
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("memory.events"), "low 0\nmax 2\noom_kill 1\n").unwrap();
    assert_eq!(read_memory_events(root.path()), Some((2, 1)));
    let message = classify_cgroup_exit(Some(-9), Some((1, 0)), Some((2, 0)), 12288).unwrap();
    assert!(message.contains("session 内存上限触发"));
    assert!(message.contains("12288"));
    assert!(message.contains("gpuq"));
  }

  #[test]
  fn sweep_removes_empty_directories_and_keeps_populated_ones() {
    let base = tempfile::tempdir().unwrap();
    fs::create_dir(base.path().join("charliebot-sess-empty")).unwrap();
    let populated = base.path().join("charliebot-sess-full");
    fs::create_dir(&populated).unwrap();
    fs::write(populated.join("cgroup.procs"), "1").unwrap();
    assert_eq!(sweep_stale_session_cgroups(base.path()), 1);
    assert!(!base.path().join("charliebot-sess-empty").exists());
    assert!(populated.is_dir());
  }

  #[test]
  fn cleanup_removes_only_an_empty_session_group() {
    let base = tempfile::tempdir().unwrap();
    let path = ensure_session_cgroup(base.path(), SESSION_ID, 64, 32, 50).unwrap();
    for name in ["memory.max", "memory.swap.max", "pids.max"] {
      fs::remove_file(path.join(name)).unwrap();
    }
    assert!(cleanup_session_cgroup(base.path(), SESSION_ID));
    assert!(!path.exists());
  }

  #[test]
  fn negative_memory_limit_starts_no_session_cgroup() {
    let base = tempfile::tempdir().unwrap();
    assert!(prepare_session_cgroup(base.path(), Some(SESSION_ID), -1, 32, 50).is_none());
    assert!(!session_cgroup_path(base.path(), SESSION_ID).exists());
  }

  #[test]
  fn negative_swap_limit_is_written_as_a_negative_number() {
    // The kernel refuses this write, so the spawn runs without its cgroup. The temporary directory accepts any text,
    // so the test checks the bytes that the refusal depends on.
    let base = tempfile::tempdir().unwrap();
    let path = ensure_session_cgroup(base.path(), SESSION_ID, 64, -32, 50).unwrap();
    assert_eq!(fs::read_to_string(path.join("memory.swap.max")).unwrap(), "-33554432");
  }

  #[test]
  fn negative_task_limit_writes_no_task_cap() {
    let base = tempfile::tempdir().unwrap();
    let path = ensure_session_cgroup(base.path(), SESSION_ID, 64, 32, -1).unwrap();
    assert_eq!(fs::read_to_string(path.join("pids.max")).unwrap(), "max");
  }
}
