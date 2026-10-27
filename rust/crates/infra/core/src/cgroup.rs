//! Shared names and cleanup for per-session process cgroups.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const SESSION_CGROUP_PREFIX: &str = "charliebot-sess-";
/// The one leaf slice that holds every session cgroup. It carries the host's agent memory limit.
pub const AGENTS_SLICE: &str = "charliebot-agents.slice";
/// The pre-split parent of the session slices. Nothing is created here; a leftover directory is still swept.
pub const RETIRED_APP_SLICE: &str = "app.slice";
/// The `oom_score_adj` of every agent process: a host-wide OOM picks an agent process before the server, which keeps
/// the default 0. The value is positive, which an unprivileged writer may set.
pub const SESSION_CGROUP_OOM_SCORE_ADJ: i64 = 500;

/// Return this user's delegated systemd service directory, which holds the session slices.
pub fn system_cgroup_base_dir() -> PathBuf {
  let uid = unsafe { libc::getuid() };
  PathBuf::from(format!("/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service"))
}

/// Return the cgroup directory systemd gives the user slice `unit` under `base_dir`.
///
/// systemd nests a slice inside the slice each dash prefix of its name names (`a-b.slice` lives in
/// `a.slice/a-b.slice`), so the agents' leaf slice sits inside `charliebot.slice`, not beside it.
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

  use super::*;

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
