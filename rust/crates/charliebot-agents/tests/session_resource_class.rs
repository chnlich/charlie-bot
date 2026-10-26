//! The repair mark at spawn time: a session's cgroup lands under its class's slice, with that slice's OOM priority.
//! Each test places cgroups under a temporary delegated base, so no test moves a process into a real cgroup.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use charliebot_agents::transport::cgroup::{SessionCgroupConfig, use_fake_cgroup_hierarchy};
use charliebot_agents::transport::spawn::{ChildStdin, SpawnSetup, SpawnSpec, spawn_one_shot, spawn_with_setup};
use charliebot_core::cgroup::{ResourceClass, session_slice_dir};

const PARENT_ID: &str = "11112222-3333-4444-5555-666677778888";
const CHILD_ID: &str = "99998888-7777-6666-5555-444433332222";
const AGENTS_SLICE: &str = "charliebot-agents.slice";
const CHILD_CGROUP: &str = "charliebot-sess-99998888";

/// Write the one file the class walk reads: the task parent and the optional repair mark.
fn write_metadata(sessions: &Path, session_id: &str, parent: Option<&str>, mark: Option<&str>) {
  let directory = sessions.join(session_id);
  fs::create_dir_all(&directory).unwrap();
  let mut payload = serde_json::json!({"name": session_id, "profile": "manager", "task_parent_id": parent});
  if let Some(mark) = mark {
    payload["resource_class"] = serde_json::json!(mark);
  }
  fs::write(directory.join("metadata.json"), payload.to_string()).unwrap();
}

/// A delegated base with both leaf slices, and the sessions directory of the same home.
fn fixture(root: &Path) -> (PathBuf, PathBuf) {
  let base = root.join("user@1001.service");
  for class in [ResourceClass::Agent, ResourceClass::Repair] {
    let slice = session_slice_dir(&base, class);
    fs::create_dir_all(&slice).unwrap();
    fs::write(slice.join("cgroup.subtree_control"), "").unwrap();
  }
  (base, root.join("sessions"))
}

fn cgroup_config(base: &Path, sessions: &Path) -> SessionCgroupConfig {
  SessionCgroupConfig {
    base_dir: base.to_path_buf(),
    sessions_dir: sessions.to_path_buf(),
    memory_max_mb: 64,
    swap_max_mb: 0,
    tasks_max: 64,
  }
}

/// Empty each place a spawn may put the child session's cgroup, with the `cgroup.procs` the kernel supplies to a real
/// cgroup. The spawn's pid must land in the one its class selects, and the others stay empty.
fn reset_session_cgroup_stand_ins(base: &Path) {
  let directories = [
    base.join(CHILD_CGROUP),
    session_slice_dir(base, ResourceClass::Agent).join(CHILD_CGROUP),
    session_slice_dir(base, ResourceClass::Repair).join(CHILD_CGROUP),
  ];
  for directory in directories {
    fs::create_dir_all(&directory).unwrap();
    // The control files the kernel publishes for a cgroup, which the fake tree must carry.
    for name in ["cgroup.procs", "cgroup.threads", "cgroup.subtree_control", "memory.max", "memory.swap.max", "pids.max"]
    {
      fs::write(directory.join(name), "").unwrap();
    }
  }
}

/// Spawn one child of the child session and return its pid and the `oom_score_adj` it runs with.
async fn spawn_child_session(config: &SessionCgroupConfig) -> (u32, String) {
  reset_session_cgroup_stand_ins(&config.base_dir);
  let spec =
    SpawnSpec::new("/bin/sleep", vec!["5".into()], None, HashMap::new(), Some(CHILD_ID.to_string()), config.clone());
  let mut spawned = spawn_with_setup(&spec, None, ChildStdin::Null, SpawnSetup::ChildSideCgroup).await.unwrap();
  let pid = spawned.child.id().unwrap();
  let oom = fs::read_to_string(format!("/proc/{pid}/oom_score_adj")).unwrap().trim().to_string();
  spawned.child.kill().await.unwrap();
  spawned.child.wait().await.unwrap();
  (pid, oom)
}

fn assert_procs_hold(procs: &Path, pid: u32) {
  assert!(procs.is_file(), "{} is missing", procs.display());
  let content = fs::read_to_string(procs).unwrap();
  assert!(content.contains(&pid.to_string()), "{} must hold pid {pid}, got {content:?}", procs.display());
}

#[tokio::test]
async fn spawn_follows_the_nearest_repair_mark() {
  use_fake_cgroup_hierarchy();
  let root = tempfile::tempdir().unwrap();
  let (base, sessions) = fixture(root.path());
  write_metadata(&sessions, PARENT_ID, None, None);
  write_metadata(&sessions, CHILD_ID, Some(PARENT_ID), None);
  let config = cgroup_config(&base, &sessions);
  let agents_procs = session_slice_dir(&base, ResourceClass::Agent).join(CHILD_CGROUP).join("cgroup.procs");
  let repair_procs = session_slice_dir(&base, ResourceClass::Repair).join(CHILD_CGROUP).join("cgroup.procs");

  let (agent_pid, oom) = spawn_child_session(&config).await;
  assert_procs_hold(&agents_procs, agent_pid);
  assert_eq!(oom, "500");

  write_metadata(&sessions, PARENT_ID, None, Some("repair"));
  let (repair_pid, oom) = spawn_child_session(&config).await;
  assert_procs_hold(&repair_procs, repair_pid);
  assert!(!fs::read_to_string(&agents_procs).unwrap().contains(&repair_pid.to_string()));
  assert_eq!(oom, "100");

  write_metadata(&sessions, PARENT_ID, None, None);
  let (back_pid, oom) = spawn_child_session(&config).await;
  assert_procs_hold(&agents_procs, back_pid);
  assert_eq!(oom, "500");
}

#[tokio::test]
async fn spawn_fails_with_the_slice_name_when_the_selected_slice_is_missing() {
  let root = tempfile::tempdir().unwrap();
  let base = root.path().join("user@1001.service");
  fs::create_dir_all(session_slice_dir(&base, ResourceClass::Repair)).unwrap();
  let sessions = root.path().join("sessions");
  write_metadata(&sessions, PARENT_ID, None, None);
  let spec = SpawnSpec::new(
    "/bin/true",
    Vec::new(),
    None,
    HashMap::new(),
    Some(PARENT_ID.to_string()),
    cgroup_config(&base, &sessions),
  );

  let result = spawn_one_shot(&spec, None).await;
  assert!(result.is_err(), "a spawn whose agents slice is missing must not start without its memory guard");
  let error = result.err().unwrap().to_string();
  assert!(error.contains(AGENTS_SLICE), "the error must name the missing slice, got {error}");
}
