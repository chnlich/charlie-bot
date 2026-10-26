//! Shared names and cleanup for per-session process cgroups.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const SESSION_CGROUP_PREFIX: &str = "charliebot-sess-";
const AGENTS_SLICE: &str = "charliebot-agents.slice";
const REPAIR_SLICE: &str = "charliebot-repair.slice";
/// The pre-split parent of the session slices. Nothing is created here; a leftover directory is still swept.
pub const RETIRED_APP_SLICE: &str = "app.slice";

/// The memory share a session's agent processes run in (`models.ResourceClass`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResourceClass {
  #[default]
  Agent,
  Repair,
}

impl ResourceClass {
  /// The wire name of this class, as the metadata file and the API spell it.
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Agent => "agent",
      Self::Repair => "repair",
    }
  }

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

/// Return this user's delegated systemd service directory, which holds the session slices.
pub fn system_cgroup_base_dir() -> PathBuf {
  let uid = unsafe { libc::getuid() };
  PathBuf::from(format!("/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service"))
}

/// Return the cgroup directory systemd gives the user slice `unit` under `base_dir`.
///
/// systemd nests a slice inside the slice each dash prefix of its name names (`a-b.slice` lives in
/// `a.slice/a-b.slice`), so both leaf slices sit inside `charliebot.slice`, not beside it.
pub fn user_slice_dir(base_dir: &Path, unit: &str) -> PathBuf {
  let name = unit.strip_suffix(".slice").unwrap_or(unit);
  let parts: Vec<&str> = name.split('-').collect();
  let mut dir = base_dir.to_path_buf();
  for end in 1..=parts.len() {
    dir.push(format!("{}.slice", parts[..end].join("-")));
  }
  dir
}

/// Return the leaf-slice directory of `resource_class` under `base_dir`.
pub fn session_slice_dir(base_dir: &Path, resource_class: ResourceClass) -> PathBuf {
  user_slice_dir(base_dir, resource_class.slice_name())
}

/// Return every directory a session cgroup may live under: both leaf slices, and the retired slice.
pub fn session_slice_dirs(base_dir: &Path) -> [PathBuf; 3] {
  [
    session_slice_dir(base_dir, ResourceClass::Agent),
    session_slice_dir(base_dir, ResourceClass::Repair),
    user_slice_dir(base_dir, RETIRED_APP_SLICE),
  ]
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
    assert_eq!(ResourceClass::Agent.oom_score_adj(), 500);
  }

  #[test]
  fn cleanup_removes_only_an_empty_session_group() {
    let base = tempfile::tempdir().unwrap();
    let slice = session_slice_dir(base.path(), ResourceClass::Repair);
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
