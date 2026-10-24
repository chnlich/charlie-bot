//! Shared names and cleanup for per-session process cgroups.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const SESSION_CGROUP_PREFIX: &str = "charliebot-sess-";
const AGENTS_SLICE: &str = "charliebot-agents.slice";
const REPAIR_SLICE: &str = "charliebot-repair.slice";

/// The memory share a session's agent processes run in (`models.ResourceClass`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResourceClass {
  #[default]
  Agent,
  Repair,
}

impl ResourceClass {
  /// The leaf slice that holds the session cgroups of this class.
  pub fn slice_name(self) -> &'static str {
    match self {
      Self::Agent => AGENTS_SLICE,
      Self::Repair => REPAIR_SLICE,
    }
  }

  /// The `oom_score_adj` written to the processes of this class: a host-wide OOM picks agent processes first, and
  /// never the server, which keeps the default 0.
  pub fn oom_score_adj(self) -> i64 {
    match self {
      Self::Agent => 500,
      Self::Repair => 100,
    }
  }
}

/// Return the delegated systemd `app.slice` directory for this user.
pub fn system_cgroup_base_dir() -> PathBuf {
  let uid = unsafe { libc::getuid() };
  PathBuf::from(format!("/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service/app.slice"))
}

/// Return the leaf-slice directory of `resource_class` under `base_dir`.
pub fn session_slice_dir(base_dir: &Path, resource_class: ResourceClass) -> PathBuf {
  base_dir.join(resource_class.slice_name())
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

/// Remove an empty session cgroup; a populated or unavailable directory stays in place.
pub fn cleanup_session_cgroup(base_dir: &Path, session_id: &str) -> bool {
  let path = session_cgroup_path(base_dir, session_id);
  match fs::remove_dir(&path) {
    Ok(()) => {
      tracing::info!(target: "charliebot_agents", path = %path.display(), "session_cgroup_removed");
      true
    }
    Err(error) if error.kind() == io::ErrorKind::NotFound => false,
    Err(error) => {
      tracing::debug!(target: "charliebot_agents", path = %path.display(), error = %error, "session_cgroup_cleanup_retained");
      false
    }
  }
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
  fn session_cgroup_oom_priorities() {
    assert_eq!(ResourceClass::Agent.oom_score_adj(), 500);
    assert_eq!(ResourceClass::Repair.oom_score_adj(), 100);
  }

  #[test]
  fn cleanup_removes_only_an_empty_session_group() {
    let base = tempfile::tempdir().unwrap();
    let path = session_cgroup_path(base.path(), "synthetic-session");
    fs::create_dir(&path).unwrap();
    assert!(cleanup_session_cgroup(base.path(), "synthetic-session"));
    assert!(!path.exists());

    fs::create_dir(&path).unwrap();
    fs::write(path.join("member"), "occupied").unwrap();
    assert!(!cleanup_session_cgroup(base.path(), "synthetic-session"));
    assert!(path.is_dir());
  }
}
