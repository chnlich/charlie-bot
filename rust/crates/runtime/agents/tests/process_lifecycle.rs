use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use charliebot_agents::transport::cgroup::SessionCgroupConfig;
use charliebot_agents::transport::identity::{is_run_alive, pin_process_identity, read_host_boot_time};
use charliebot_agents::transport::kill::{detach, kill_process_group};
use charliebot_agents::transport::spawn::{AgentChild, RawStdin, SpawnSpec, spawn_one_shot, spawn_raw_log};
use nix::sys::signal::Signal;
use time::OffsetDateTime;
use tokio::runtime::{Builder, Runtime};

struct ChildGroupGuard<'a> {
  child: AgentChild,
  pid: u32,
  runtime: &'a Runtime,
}

impl Drop for ChildGroupGuard<'_> {
  fn drop(&mut self) {
    kill_process_group(self.pid, Signal::SIGKILL);
    if let Err(error) = self.runtime.block_on(self.child.wait()) {
      eprintln!("could not reap test process {}: {error}", self.pid);
    }
  }
}

fn assert_pdeath_child_survives_blocking_keep_alive() {
  let root = tempfile::tempdir().unwrap();
  let base = root.path().join("user@1001.service");
  fs::create_dir(&base).unwrap();
  let spec = SpawnSpec::new(
    "/bin/sleep",
    vec!["30".into()],
    None,
    HashMap::new(),
    None,
    SessionCgroupConfig { base_dir: base, ..SessionCgroupConfig::disabled() },
  );
  let runtime =
    Builder::new_current_thread().enable_all().thread_keep_alive(Duration::from_millis(100)).build().unwrap();
  let boot_time = read_host_boot_time().unwrap();
  let started_at = OffsetDateTime::now_utc();
  let spawned = runtime.block_on(async { spawn_one_shot(&spec, Some(Signal::SIGKILL)).await }).unwrap();
  let child = spawned.child;
  let pid = child.id().unwrap();
  let _guard = ChildGroupGuard { child, pid, runtime: &runtime };
  let identity = pin_process_identity(pid);
  let pid_start = identity.pid_start.as_deref().unwrap();

  std::thread::sleep(Duration::from_secs(1));

  assert!(
    is_run_alive(Some(identity.pid), Some(pid_start), Some(started_at), boot_time),
    "child {pid} did not survive the blocking thread keep-alive"
  );
}

#[test]
fn pdeath_child_survives_blocking_thread_keep_alive() {
  assert_pdeath_child_survives_blocking_keep_alive();
}

#[tokio::test]
async fn child_survival_matches_parent_death_policy() {
  let root = tempfile::tempdir().unwrap();
  let base = root.path().join("user@1001.service");
  fs::create_dir(&base).unwrap();
  for mode in ["raw", "piped"] {
    let raw_path = root.path().join(format!("{mode}.raw"));
    let stderr_path = root.path().join(format!("{mode}.stderr"));
    let identity_path = root.path().join(format!("{mode}.identity"));
    let output = Command::new(std::env::current_exe().unwrap())
      .args(["--exact", "spawn_and_exit_helper", "--nocapture"])
      .env("CHARLIEBOT_SPAWN_HELPER_MODE", mode)
      .env("CHARLIEBOT_SPAWN_HELPER_BASE", &base)
      .env("CHARLIEBOT_SPAWN_HELPER_RAW", &raw_path)
      .env("CHARLIEBOT_SPAWN_HELPER_STDERR", &stderr_path)
      .env("CHARLIEBOT_SPAWN_HELPER_IDENTITY", &identity_path)
      .output()
      .unwrap();
    assert!(output.status.success(), "spawn helper failed:\n{}", String::from_utf8_lossy(&output.stderr));
    let identity = fs::read_to_string(identity_path).unwrap();
    let mut fields = identity.split_whitespace();
    let pid = fields.next().unwrap().parse::<u32>().unwrap();
    let pid_start = fields.next().unwrap().to_string();
    let boot = read_host_boot_time().unwrap();
    let started_at = OffsetDateTime::now_utc();
    let expected_alive = mode == "raw";
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while is_run_alive(Some(pid), Some(&pid_start), Some(started_at), boot) != expected_alive
      && tokio::time::Instant::now() < deadline
    {
      tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let actually_alive = is_run_alive(Some(pid), Some(&pid_start), Some(started_at), boot);
    if actually_alive != expected_alive {
      kill_process_group(pid, Signal::SIGKILL);
    }
    assert_eq!(actually_alive, expected_alive, "{mode}");
    if expected_alive {
      kill_process_group(pid, Signal::SIGTERM);
      let cleanup_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
      while is_run_alive(Some(pid), Some(&pid_start), Some(started_at), boot)
        && tokio::time::Instant::now() < cleanup_deadline
      {
        tokio::time::sleep(Duration::from_millis(20)).await;
      }
      assert!(!is_run_alive(Some(pid), Some(&pid_start), Some(started_at), boot));
    }
  }
}

#[tokio::test]
async fn spawn_and_exit_helper() {
  let Ok(mode) = std::env::var("CHARLIEBOT_SPAWN_HELPER_MODE") else {
    return;
  };
  let base = PathBuf::from(std::env::var_os("CHARLIEBOT_SPAWN_HELPER_BASE").unwrap());
  let raw_path = PathBuf::from(std::env::var_os("CHARLIEBOT_SPAWN_HELPER_RAW").unwrap());
  let stderr_path = PathBuf::from(std::env::var_os("CHARLIEBOT_SPAWN_HELPER_STDERR").unwrap());
  let identity_path = PathBuf::from(std::env::var_os("CHARLIEBOT_SPAWN_HELPER_IDENTITY").unwrap());
  let program = if mode == "raw" { "/bin/sh" } else { "/bin/sleep" };
  let args = if mode == "raw" {
    vec!["-c".into(), "printf '%s\\n' \"$$\"; exec /bin/sleep 30".into()]
  } else {
    vec!["30".into()]
  };
  let spec = SpawnSpec::new(
    program,
    args,
    None,
    HashMap::new(),
    None,
    SessionCgroupConfig { base_dir: base, ..SessionCgroupConfig::disabled() },
  );
  let pid = if mode == "raw" {
    let child = spawn_raw_log(&spec, &raw_path, &stderr_path, RawStdin::Null, |_| ()).await.unwrap().0.child;
    let pid = child.id().unwrap();
    detach(child);
    pid
  } else {
    let child = spawn_one_shot(&spec, Some(Signal::SIGKILL)).await.unwrap().child;
    let pid = child.id().unwrap();
    detach(child);
    pid
  };
  let identity = charliebot_agents::transport::identity::pin_process_identity(pid);
  fs::write(identity_path, format!("{} {}", identity.pid, identity.pid_start.unwrap())).unwrap();
}
