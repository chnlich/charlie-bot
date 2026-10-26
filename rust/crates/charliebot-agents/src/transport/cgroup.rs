//! Session cgroups: one delegated directory per session, under the leaf slice of the session's resource class.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use cgfs::{Cgroup, EntryKind, Hierarchy, LeafSpec};
use charliebot_core::cgroup::{
  ResourceClass, session_cgroup_name, session_cgroup_path, session_slice_dir, session_slice_dirs,
};
use charliebot_core::config::CharlieBotConfig;
use charliebot_registry::StartupStop;

/// The session cgroup placement of one spawn: the delegated cgroup base, the home's sessions directory that the class
/// walk reads, and the caps copied from the server configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionCgroupConfig {
  /// Delegated cgroup directory that holds the session cgroups.
  pub base_dir: PathBuf,
  /// The home's sessions directory.
  pub sessions_dir: PathBuf,
  pub memory_max_mb: i64,
  pub swap_max_mb: i64,
  pub tasks_max: i64,
}

impl SessionCgroupConfig {
  /// The caps and directories of the server configuration `cfg`, under this user's delegated cgroup base.
  pub fn from_config(cfg: &CharlieBotConfig) -> Self {
    Self {
      base_dir: charliebot_core::cgroup::system_cgroup_base_dir(),
      sessions_dir: cfg.sessions_dir(),
      memory_max_mb: cfg.server.session_memory_max_mb,
      swap_max_mb: cfg.server.session_swap_max_mb,
      tasks_max: cfg.server.session_tasks_max,
    }
  }

  /// A placement with cgroup control off: a zero memory cap starts no session cgroup, so no directory is read.
  pub fn disabled() -> Self {
    Self { base_dir: PathBuf::new(), sessions_dir: PathBuf::new(), memory_max_mb: 0, swap_max_mb: 0, tasks_max: 0 }
  }
}

/// The pre-spawn memory counter snapshot for one session cgroup.
#[derive(Clone, Debug)]
pub struct SessionCgroup {
  /// Session cgroup directory.
  pub path: PathBuf,
  /// The class that chose the slice; it also fixes the OOM priority of the spawn's processes.
  pub resource_class: ResourceClass,
  /// Configured memory cap in megabytes.
  pub memory_max_mb: i64,
  /// `memory.events` counters recorded before spawn.
  pub events_before: Option<(u64, u64)>,
}

impl SessionCgroup {
  /// The `oom_score_adj` the spawn's processes run with.
  pub fn oom_score_adj(&self) -> i64 {
    self.resource_class.oom_score_adj()
  }

  /// Classify an exited process against this spawn's memory event counters.
  pub fn classify_exit(&self, returncode: Option<i32>) -> Option<String> {
    classify_cgroup_exit(returncode, self.events_before, read_memory_events(&self.path), self.memory_max_mb)
  }
}

/// Create or refresh the session's cgroup under its class's slice, returning the cgroup directory. A missing or
/// unwritable slice is an error that names the slice: an unbounded spawn is the failure this guard exists to prevent.
pub fn ensure_session_cgroup(
  base_dir: &Path,
  resource_class: ResourceClass,
  session_id: &str,
  memory_max_mb: i64,
  swap_max_mb: i64,
  tasks_max: i64,
) -> io::Result<PathBuf> {
  let slice = session_slice_dir(base_dir, resource_class);
  let slice_name = resource_class.slice_name();
  if !slice.is_dir() || !is_writable(&slice) {
    return Err(io::Error::other(format!(
      "session cgroup slice {slice_name} ({}) is missing or not writable; install and start the slices from \
       scripts/systemd/ with scripts/setup.sh",
      slice.display()
    )));
  }
  let hierarchy = open_hierarchy(base_dir)
    .map_err(|error| io::Error::other(format!("session cgroup slice {slice_name} cannot be opened: {error}")))?;
  let slice_cgroup = hierarchy.at_path(relative(base_dir, &slice)?)?;
  // systemd enables a slice's controllers for the units inside it only, and a session cgroup is no unit: without this
  // its limit files would not exist.
  slice_cgroup.enable_subtree_control(&["memory".to_string(), "pids".to_string()]).map_err(|error| {
    io::Error::other(format!(
      "session cgroup slice {slice_name} cannot enable memory and pids for its children: {error}"
    ))
  })?;
  let path = session_cgroup_path(&slice, session_id);
  let leaf = slice_cgroup.child(OsStr::new(&session_cgroup_name(session_id)))?;
  let created = !path.exists();
  create_leaf(&leaf).map_err(|error| {
    io::Error::other(format!("session cgroup slice {slice_name} cannot hold {}: {error}", path.display()))
  })?;
  let limits = [
    ("memory.max", (memory_max_mb * 1024 * 1024).to_string()),
    ("memory.swap.max", (swap_max_mb * 1024 * 1024).to_string()),
    ("pids.max", if tasks_max > 0 { tasks_max.to_string() } else { "max".to_string() }),
  ];
  for (file, value) in limits {
    leaf.control(file)?.write(value).map_err(|error| {
      io::Error::other(format!(
        "session cgroup slice {slice_name} cannot take the limit files in {}: {error}",
        path.display()
      ))
    })?;
  }
  if created {
    tracing::info!(
      target: "charliebot_agents",
      path = %path.display(),
      resource_class = resource_class.as_str(),
      memory_max_mb,
      swap_max_mb,
      tasks_max,
      "session_cgroup_created"
    );
  }
  Ok(path)
}

/// Open `path` as a cgroup hierarchy root, which a delegated subtree is.
fn open_hierarchy(path: &Path) -> io::Result<Hierarchy> {
  if cfg!(test) || FAKE_HIERARCHY.load(Ordering::Relaxed) {
    // SAFETY: unit tests and the tests that call `use_fake_cgroup_hierarchy` pass a private temporary directory that
    // stands in for cgroupfs; the server never turns the flag on.
    unsafe { Hierarchy::open_for_test(path) }
  } else {
    Hierarchy::open(path)
  }
}

/// Whether the cgroup handles name a test tree. The tests' temporary directories hold the control files a cgroupfs
/// would publish, but they are not cgroup2.
static FAKE_HIERARCHY: AtomicBool = AtomicBool::new(false);

/// Mark this process's cgroup bases as fake test trees. Integration tests call this before their first cgroup call.
#[doc(hidden)]
pub fn use_fake_cgroup_hierarchy() {
  FAKE_HIERARCHY.store(true, Ordering::Relaxed);
}

/// The path of `path` relative to the hierarchy root `base_dir`, which is what a cgroup handle names.
fn relative(base_dir: &Path, path: &Path) -> io::Result<PathBuf> {
  path.strip_prefix(base_dir).map(Path::to_path_buf).map_err(|_| {
    io::Error::other(format!("{} is outside the cgroup base {}", path.display(), base_dir.display()))
  })
}

/// Create the leaf cgroup, keeping an existing one, as `mkdir` with `exist_ok` does.
fn create_leaf(leaf: &Cgroup) -> io::Result<()> {
  leaf.apply(&LeafSpec::new(), None)
}

/// Whether this user may write to `path`, by the `access(2)` check that the startup log also makes.
fn is_writable(path: &Path) -> bool {
  let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).expect("path has no NUL byte");
  unsafe { libc::access(path.as_ptr(), libc::W_OK) == 0 }
}

/// Read `(max, oom_kill)` from `memory.events`, or `None` when either counter is unavailable.
pub fn read_memory_events(cgroup_dir: &Path) -> Option<(u64, u64)> {
  let hierarchy = open_hierarchy(cgroup_dir).ok()?;
  let cgroup = hierarchy.root();
  let events = cgroup.control("memory.events").ok()?.read_kv().ok()?;
  let mut max = None;
  let mut oom_kill = None;
  for (key, value) in events {
    if key == "max" {
      max = Some(value.parse().ok()?);
    } else if key == "oom_kill" {
      oom_kill = Some(value.parse().ok()?);
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

/// The session cgroup of one spawn, ensured under its class's slice with its counters snapshot. `None` when cgroup
/// control is off for the spawn: no session, or a memory limit of zero or below. The class is the nearest repair mark
/// up the session's task-parent chain, so a failed walk or an unavailable slice fails the spawn.
pub async fn prepare_session_cgroup(
  config: &SessionCgroupConfig,
  session_id: Option<&str>,
) -> io::Result<Option<SessionCgroup>> {
  let session_id = match session_id {
    Some(session_id) if !session_id.is_empty() && config.memory_max_mb > 0 => session_id,
    _ => return Ok(None),
  };
  let resource_class = resolve_resource_class(&config.sessions_dir, session_id).await?;
  let path = ensure_session_cgroup(
    &config.base_dir,
    resource_class,
    session_id,
    config.memory_max_mb,
    config.swap_max_mb,
    config.tasks_max,
  )?;
  Ok(Some(SessionCgroup {
    events_before: read_memory_events(&path),
    path,
    resource_class,
    memory_max_mb: config.memory_max_mb,
  }))
}

/// The spawn-time class of `session_id`, resolved by the store's task-parent walk on a blocking thread.
async fn resolve_resource_class(sessions_dir: &Path, session_id: &str) -> io::Result<ResourceClass> {
  let sessions_dir = sessions_dir.to_path_buf();
  let session_id = session_id.to_string();
  tokio::task::spawn_blocking(move || charliebot_store::resource_class::resolve(&sessions_dir, &session_id))
    .await
    .map_err(io::Error::other)?
    .map_err(|error| io::Error::other(error.to_string()))
}

/// Move a spawned process into its cgroup and set its OOM priority; failures match the Python warnings and do not fail
/// the spawn.
pub fn move_process_to_cgroup(pid: u32, cgroup: &SessionCgroup) {
  let path = cgroup.path.join("cgroup.procs");
  if let Err(error) = attach_pid(&cgroup.path, pid) {
    tracing::warn!(target: "charliebot_agents", pid, path = %path.display(), error = %error, "session_cgroup_move_failed");
  }
  let oom_path = PathBuf::from(format!("/proc/{pid}/oom_score_adj"));
  if let Err(error) = fs::write(&oom_path, cgroup.oom_score_adj().to_string()) {
    tracing::warn!(
      target: "charliebot_agents",
      pid,
      path = %oom_path.display(),
      error = %error,
      "session_cgroup_oom_adjust_failed"
    );
  }
}

/// Attach `pid` by writing the leaf's `cgroup.procs` control file.
fn attach_pid(cgroup_dir: &Path, pid: u32) -> io::Result<()> {
  open_hierarchy(cgroup_dir)?.root().attach(pid)
}

/// Remove an empty session cgroup; a populated or unavailable directory is retained.
pub fn cleanup_session_cgroup(base_dir: &Path, session_id: &str) -> bool {
  charliebot_core::cgroup::cleanup_session_cgroup(base_dir, session_id)
}

/// Remove each empty cgroup named for a session of `sessions_dir` and retain a populated one with a warning. Other
/// entries under `base_dir`, and the cgroups of other homes, stay.
pub fn sweep_stale_session_cgroups(base_dir: &Path, sessions_dir: &Path) -> io::Result<usize> {
  sweep_stale_session_cgroups_until_stopped(base_dir, sessions_dir, &StartupStop::default())
}

/// Remove stale cgroups until the startup stop request reaches a safe boundary.
pub fn sweep_stale_session_cgroups_until_stopped(
  base_dir: &Path,
  sessions_dir: &Path,
  stop: &StartupStop,
) -> io::Result<usize> {
  let is_stopped = || stop.is_cancelled();
  let Some(owned) = home_session_cgroup_names(sessions_dir, &is_stopped)? else {
    return Ok(0);
  };
  let hierarchy = match open_hierarchy(base_dir) {
    Ok(hierarchy) => hierarchy,
    Err(error) => {
      tracing::info!(target: "charliebot_agents", base = %base_dir.display(), error = %error, "session_cgroup_sweep_unavailable");
      return Ok(0);
    }
  };
  let mut removed = Vec::new();
  'slices: for slice in session_slice_dirs(base_dir) {
    let slice_cgroup = match relative(base_dir, &slice).and_then(|path| hierarchy.at_path(path)) {
      Ok(cgroup) => cgroup,
      Err(error) => {
        tracing::info!(target: "charliebot_agents", base = %slice.display(), error = %error, "session_cgroup_sweep_unavailable");
        continue;
      }
    };
    let entries = match slice_cgroup.entries() {
      Ok(entries) => entries,
      Err(error) => {
        tracing::info!(target: "charliebot_agents", base = %slice.display(), error = %error, "session_cgroup_sweep_unavailable");
        continue;
      }
    };
    for entry in entries {
      if is_stopped() {
        break 'slices;
      }
      if entry.kind != EntryKind::Directory || !owned.contains(&entry.name) {
        continue;
      }
      let leaf = match slice_cgroup.child(&entry.name) {
        Ok(leaf) => leaf,
        Err(error) => {
          tracing::warn!(target: "charliebot_agents", path = %slice.join(&entry.name).display(), error = %error, "session_cgroup_sweep_retained");
          continue;
        }
      };
      match leaf.delete_leaf() {
        Ok(()) => removed.push(entry.name.to_string_lossy().into_owned()),
        Err(error) => {
          tracing::warn!(target: "charliebot_agents", path = %slice.join(&entry.name).display(), error = %error, "session_cgroup_sweep_retained")
        }
      }
    }
  }
  removed.sort();
  if !removed.is_empty() {
    tracing::info!(target: "charliebot_agents", count = removed.len(), names = ?removed, "session_cgroup_sweep_removed");
  }
  Ok(removed.len())
}

/// The cgroup directory name of each session directory under `sessions_dir`.
fn home_session_cgroup_names(
  sessions_dir: &Path,
  is_stopped: &impl Fn() -> bool,
) -> io::Result<Option<HashSet<OsString>>> {
  let mut names = HashSet::new();
  for entry in fs::read_dir(sessions_dir)? {
    if is_stopped() {
      return Ok(None);
    }
    let entry = entry?;
    if entry.path().is_dir() {
      names.insert(OsString::from(session_cgroup_name(&entry.file_name().to_string_lossy())));
    }
  }
  Ok(Some(names))
}

/// The one startup line that states whether session cgroup control is on. A memory limit of zero or below turns it off.
/// A leaf slice that is missing or unwritable is a warning, because every spawn with a session then fails.
pub fn log_session_cgroup_startup(base_dir: &Path, memory_max_mb: i64, swap_max_mb: i64, tasks_max: i64) {
  if memory_max_mb <= 0 {
    tracing::info!(target: "charliebot_agents", reason = "server.session_memory_max_mb is 0", "session_cgroup_disabled");
    return;
  }
  for resource_class in [ResourceClass::Agent, ResourceClass::Repair] {
    let slice = session_slice_dir(base_dir, resource_class);
    if !slice.is_dir() {
      tracing::warn!(target: "charliebot_agents", reason = %format!("{} not present on this host; spawns with a session fail", slice.display()), "session_cgroup_unavailable");
      return;
    }
    if !is_writable(&slice) {
      tracing::warn!(target: "charliebot_agents", reason = %format!("{} not writable by this user; spawns with a session fail", slice.display()), "session_cgroup_unavailable");
      return;
    }
  }
  tracing::info!(target: "charliebot_agents", memory_max_mb, swap_max_mb, tasks_max, "session_cgroup_enabled");
}

#[cfg(test)]
mod tests {
  use std::fs::Permissions;
  use std::os::unix::fs::PermissionsExt;

  use super::*;

  const SESSION_ID: &str = "abcd1234-ef56-7890-abcd-ef1234567890";
  const CHARLIEBOT_SLICE: &str = "charliebot.slice";
  const AGENTS_SLICE: &str = "charliebot-agents.slice";
  const REPAIR_SLICE: &str = "charliebot-repair.slice";

  /// The user service directory with its two leaf slices, standing in for the delegated cgroup base.
  fn fake_slices() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let parent = root.path().join(CHARLIEBOT_SLICE);
    fs::create_dir(&parent).unwrap();
    for name in [AGENTS_SLICE, REPAIR_SLICE] {
      let slice = parent.join(name);
      fs::create_dir(&slice).unwrap();
      fs::write(slice.join("cgroup.subtree_control"), "").unwrap();
    }
    root
  }

  fn leaf_slice(root: &Path, name: &str) -> PathBuf {
    root.join(CHARLIEBOT_SLICE).join(name)
  }

  fn leaf_path(root: &Path, slice: &str) -> PathBuf {
    leaf_slice(root, slice).join(session_cgroup_name(SESSION_ID))
  }

  /// A leaf as the kernel publishes one: the core control files and the limit files the spawn writes.
  fn fake_leaf(root: &Path, slice: &str) -> PathBuf {
    let leaf = leaf_path(root, slice);
    fs::create_dir_all(&leaf).unwrap();
    for name in ["cgroup.procs", "cgroup.threads", "cgroup.subtree_control", "memory.max", "memory.swap.max", "pids.max"]
    {
      fs::write(leaf.join(name), "").unwrap();
    }
    leaf
  }

  fn cgroup_config(base_dir: &Path, memory_max_mb: i64) -> SessionCgroupConfig {
    SessionCgroupConfig {
      base_dir: base_dir.to_path_buf(),
      sessions_dir: PathBuf::new(),
      memory_max_mb,
      swap_max_mb: 32,
      tasks_max: 4096,
    }
  }

  #[test]
  fn the_leaf_is_created_under_the_chosen_slice() {
    let root = fake_slices();
    let hierarchy = open_hierarchy(root.path()).unwrap();
    let slice = hierarchy.at_path(format!("{CHARLIEBOT_SLICE}/{AGENTS_SLICE}")).unwrap();
    let leaf = slice.child(OsStr::new(&session_cgroup_name(SESSION_ID))).unwrap();

    create_leaf(&leaf).unwrap();

    let path = leaf_path(root.path(), AGENTS_SLICE);
    assert!(path.is_dir());
    assert!(path.join("cgroup.procs").is_file());
  }

  #[test]
  fn ensure_session_cgroup_writes_the_limits_under_the_chosen_slice() {
    let root = fake_slices();
    fake_leaf(root.path(), AGENTS_SLICE);
    let path = ensure_session_cgroup(root.path(), ResourceClass::Agent, SESSION_ID, 64, 32, 4096).unwrap();
    assert_eq!(path, leaf_path(root.path(), AGENTS_SLICE));
    assert_eq!(
      fs::read_to_string(leaf_slice(root.path(), AGENTS_SLICE).join("cgroup.subtree_control")).unwrap(),
      "+memory +pids\n"
    );
    assert_eq!(fs::read_to_string(path.join("memory.max")).unwrap(), format!("{}\n", 64 * 1024 * 1024));
    assert_eq!(fs::read_to_string(path.join("memory.swap.max")).unwrap(), format!("{}\n", 32 * 1024 * 1024));
    assert_eq!(fs::read_to_string(path.join("pids.max")).unwrap(), "4096\n");
  }

  #[test]
  fn ensure_session_cgroup_puts_a_repair_session_under_the_repair_slice() {
    let root = fake_slices();
    fake_leaf(root.path(), REPAIR_SLICE);
    let path = ensure_session_cgroup(root.path(), ResourceClass::Repair, SESSION_ID, 64, 32, 4096).unwrap();
    assert_eq!(path, leaf_path(root.path(), REPAIR_SLICE));
  }

  #[test]
  fn ensure_session_cgroup_names_the_failed_subtree_control_write() {
    // A directory where the control file belongs makes the write fail, as a read-only cgroup file system does.
    let root = fake_slices();
    let control = leaf_slice(root.path(), AGENTS_SLICE).join("cgroup.subtree_control");
    fs::remove_file(&control).unwrap();
    fs::create_dir(&control).unwrap();
    let result = ensure_session_cgroup(root.path(), ResourceClass::Agent, SESSION_ID, 64, 32, 4096);
    assert!(
      matches!(
        &result,
        Err(error) if error.to_string().contains(AGENTS_SLICE)
          && error.to_string().contains("cannot enable memory and pids for its children")
      ),
      "a refused controller enablement must fail the spawn and name the slice, got {result:?}"
    );
  }

  #[test]
  fn ensure_session_cgroup_rewrites_task_cap_and_zero_lifts_it() {
    let root = fake_slices();
    fake_leaf(root.path(), AGENTS_SLICE);
    let path = ensure_session_cgroup(root.path(), ResourceClass::Agent, SESSION_ID, 64, 32, 50).unwrap();
    assert_eq!(fs::read_to_string(path.join("pids.max")).unwrap(), "50\n");
    assert_eq!(ensure_session_cgroup(root.path(), ResourceClass::Agent, SESSION_ID, 64, 32, 0).unwrap(), path);
    assert_eq!(fs::read_to_string(path.join("pids.max")).unwrap(), "max\n");
  }

  #[test]
  fn ensure_session_cgroup_names_the_missing_slice() {
    let root = tempfile::tempdir().unwrap();
    let result = ensure_session_cgroup(root.path(), ResourceClass::Agent, SESSION_ID, 64, 32, 4096);
    assert!(
      matches!(&result, Err(error) if error.to_string().contains(AGENTS_SLICE)),
      "a missing slice must fail the spawn and name the slice, got {result:?}"
    );
  }

  #[test]
  fn ensure_session_cgroup_names_the_unwritable_slice() {
    let root = fake_slices();
    let repair = leaf_slice(root.path(), REPAIR_SLICE);
    fs::set_permissions(&repair, Permissions::from_mode(0o500)).unwrap();
    let result = ensure_session_cgroup(root.path(), ResourceClass::Repair, SESSION_ID, 64, 32, 4096);
    fs::set_permissions(&repair, Permissions::from_mode(0o755)).unwrap();
    assert!(
      matches!(&result, Err(error) if error.to_string().contains(REPAIR_SLICE)),
      "an unwritable slice must fail the spawn and name the slice, got {result:?}"
    );
  }

  #[test]
  fn memory_events_classify_cap_kills() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("memory.events"), "low 0\nmax 2\noom_kill 1\n").unwrap();
    assert_eq!(read_memory_events(root.path()), Some((2, 1)));
    let message = classify_cgroup_exit(Some(-9), Some((1, 0)), Some((2, 0)), 12288).unwrap();
    assert!(message.contains("session 内存上限触发"));
    assert!(message.contains("12288"));
    assert!(message.contains("gpuq"));
  }

  #[test]
  fn cleanup_removes_only_an_empty_session_group() {
    let root = fake_slices();
    let path = fake_leaf(root.path(), AGENTS_SLICE);
    ensure_session_cgroup(root.path(), ResourceClass::Agent, SESSION_ID, 64, 32, 50).unwrap();
    // The kernel drops a cgroup's control files with the directory; the fake tree must lose them first.
    for name in
      ["cgroup.procs", "cgroup.threads", "cgroup.subtree_control", "memory.max", "memory.swap.max", "pids.max"]
    {
      fs::remove_file(path.join(name)).unwrap();
    }
    assert!(cleanup_session_cgroup(root.path(), SESSION_ID));
    assert!(!path.exists());
  }

  #[tokio::test]
  async fn negative_memory_limit_starts_no_session_cgroup() {
    let root = fake_slices();
    let config = cgroup_config(root.path(), -1);
    assert!(prepare_session_cgroup(&config, Some(SESSION_ID)).await.unwrap().is_none());
    assert!(!session_cgroup_path(&leaf_slice(root.path(), AGENTS_SLICE), SESSION_ID).exists());
  }

  #[test]
  fn negative_swap_limit_is_written_as_a_negative_number() {
    // The kernel refuses this write, so the spawn runs without its cgroup. The temporary directory accepts any text,
    // so the test checks the bytes that the refusal depends on.
    let root = fake_slices();
    fake_leaf(root.path(), AGENTS_SLICE);
    let path = ensure_session_cgroup(root.path(), ResourceClass::Agent, SESSION_ID, 64, -32, 50).unwrap();
    assert_eq!(fs::read_to_string(path.join("memory.swap.max")).unwrap(), "-33554432\n");
  }

  #[test]
  fn negative_task_limit_writes_no_task_cap() {
    let root = fake_slices();
    fake_leaf(root.path(), AGENTS_SLICE);
    let path = ensure_session_cgroup(root.path(), ResourceClass::Agent, SESSION_ID, 64, 32, -1).unwrap();
    assert_eq!(fs::read_to_string(path.join("pids.max")).unwrap(), "max\n");
  }
}
