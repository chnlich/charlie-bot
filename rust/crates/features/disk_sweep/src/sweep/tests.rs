use std::collections::BTreeMap;
use std::fs::{self, File, FileTimes, Permissions};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use charliebot_registry::worktree_removal::WorktreeRemoval;
use chrono::{DateTime, Utc};
use futures_util::future::BoxFuture;
use serde_json::json;
use tempfile::tempdir;
use walkdir::WalkDir;

use super::{SweepAction, SweepMode, SweepRoots, run_sweep};

fn fixture(base: &Path) -> SweepRoots {
  let roots = SweepRoots {
    worktree_root: base.join("worktrees"),
    scratch_root: base.join("scripts"),
    sessions_root: base.join("sessions"),
    proc_root: base.join("proc"),
    reference_command: None,
  };
  for path in [&roots.worktree_root, &roots.scratch_root, &roots.sessions_root, &roots.proc_root] {
    fs::create_dir_all(path).unwrap();
  }
  roots
}

fn git(directory: &Path, arguments: &[&str]) -> Vec<u8> {
  let output = Command::new("git").args(arguments).current_dir(directory).output().unwrap();
  assert!(output.status.success(), "git {arguments:?}: {}", String::from_utf8_lossy(&output.stderr));
  output.stdout
}

fn create_repo(path: &Path) -> PathBuf {
  fs::create_dir_all(path).unwrap();
  git(path, &["init", "-b", "main"]);
  git(path, &["config", "user.name", "Disk Sweep Test"]);
  git(path, &["config", "user.email", "disk-sweep@example.test"]);
  // Git detaches a `git maintenance run --auto` after every commit. Its writes into `.git` land after a test has
  // aged the fixture, so the sweep would see a fresh tree and skip the entry. A walked `.git` entry could also
  // vanish mid-walk. The fixture must stay quiet while a test asserts on its age.
  git(path, &["config", "maintenance.auto", "false"]);
  git(path, &["config", "gc.auto", "0"]);
  fs::write(path.join("tracked.txt"), b"original\n").unwrap();
  git(path, &["add", "tracked.txt"]);
  git(path, &["commit", "-m", "initial"]);
  path.to_path_buf()
}

fn create_worktree(roots: &SweepRoots, repo: &Path, name: &str) -> (PathBuf, String) {
  let branch = format!("charliebot/{name}");
  let path = roots.worktree_root.join(branch.replace('/', "-"));
  let path_text = path.to_str().unwrap();
  git(repo, &["worktree", "add", "-b", &branch, path_text, "HEAD"]);
  (path, branch)
}

struct Owner<'a> {
  session_id: &'a str,
  worktree: &'a Path,
  repo: &'a Path,
  branch: &'a str,
  closed_at: Option<DateTime<Utc>>,
  keep_worktree: bool,
  reopened: bool,
}

fn write_owner(roots: &SweepRoots, owner: Owner<'_>) -> PathBuf {
  let run_id = format!("run-{}", owner.session_id);
  let session = roots.sessions_root.join(owner.session_id);
  let run = session.join("data/runs").join(&run_id);
  fs::create_dir_all(&run).unwrap();
  fs::write(
    session.join("metadata.json"),
    serde_json::to_vec(&json!({"task": {"goal": "synthetic task", "keep_worktree": owner.keep_worktree}})).unwrap(),
  )
  .unwrap();
  let mut events = Vec::new();
  if let Some(closed_at) = owner.closed_at {
    events.push(json!({"type": "task_closed", "timestamp": closed_at.to_rfc3339(), "outcome": "archived"}));
  }
  if owner.reopened {
    events.push(json!({
      "type": "task_reopened",
      "timestamp": owner.closed_at.map(|value| value.to_rfc3339()),
    }));
  }
  let event_path = session.join("data/chat_events.jsonl");
  fs::create_dir_all(event_path.parent().unwrap()).unwrap();
  let event_lines = events.into_iter().map(|event| format!("{event}\n")).collect::<String>();
  fs::write(event_path, event_lines).unwrap();
  let worktree = owner.worktree.canonicalize().unwrap();
  let record = json!({
    "id": run_id,
    "session_id": owner.session_id,
    "worktree_path": worktree,
    "repo_path": owner.repo.to_string_lossy(),
    "branch_name": owner.branch,
  });
  fs::write(run.join("metadata.json"), serde_json::to_vec(&record).unwrap()).unwrap();
  run
}

/// The removal the sweep asks for: `git worktree remove --force`, which the task runtime performs in production.
fn remove_worktree(request: WorktreeRemoval<'_>) -> BoxFuture<'_, Option<String>> {
  Box::pin(async move {
    let output = Command::new("git")
      .args(["worktree", "remove", "--force"])
      .arg(request.worktree_path)
      .current_dir(request.repo_path)
      .output()
      .expect("the git worktree remove subprocess runs");
    if output.status.success() {
      None
    } else {
      Some(format!(
        "{} cleanup failed for {}: {}",
        request.label,
        request.worktree_path,
        String::from_utf8_lossy(&output.stderr).trim()
      ))
    }
  })
}

async fn run(roots: &SweepRoots, mode: SweepMode, now: DateTime<Utc>) -> super::SweepReport {
  run_sweep(roots.clone(), mode, now, remove_worktree).await.unwrap()
}

fn utc(value: &str) -> DateTime<Utc> {
  DateTime::parse_from_rfc3339(value).unwrap().to_utc()
}

fn row_by_path<'a>(report: &'a super::SweepReport, path: &Path) -> &'a super::SweepRow {
  let row = report.rows.iter().find(|row| row.path == path);
  if row.is_none() {
    // A missing row for a scratch candidate means the sweep found it younger than the cutoff and skipped it without
    // a row. Name the newest entry so a failure points at whatever kept the tree fresh.
    let detail = if path.exists() {
      let (newest, freshest) = newest_entry(path);
      format!("its newest entry is {} at {newest:.3}", freshest.display())
    } else {
      "it does not exist".to_string()
    };
    panic!("no sweep row for {}: {detail}", path.display());
  }
  row.unwrap()
}

fn set_tree_mtime(path: &Path, timestamp: SystemTime) {
  for entry in fs::read_dir(path).unwrap() {
    let entry = entry.unwrap();
    let entry_path = entry.path();
    if entry.file_type().unwrap().is_dir() {
      set_tree_mtime(&entry_path, timestamp);
    }
    File::open(&entry_path)
      .unwrap_or_else(|error| panic!("opening {} to set its age: {error}", entry_path.display()))
      .set_times(FileTimes::new().set_modified(timestamp))
      .unwrap();
  }
  File::open(path)
    .unwrap_or_else(|error| panic!("opening {} to set its age: {error}", path.display()))
    .set_times(FileTimes::new().set_modified(timestamp))
    .unwrap();
}

/// The newest entry in one fixture tree, with the path that carries it.
fn newest_entry(path: &Path) -> (f64, PathBuf) {
  fn seconds(metadata: &fs::Metadata) -> f64 {
    metadata.mtime() as f64 + metadata.mtime_nsec() as f64 / 1_000_000_000.0
  }
  let mut newest = (seconds(&fs::symlink_metadata(path).unwrap()), path.to_path_buf());
  for entry in WalkDir::new(path).min_depth(1) {
    let entry = entry.unwrap();
    let candidate = (seconds(&entry.metadata().unwrap()), entry.path().to_path_buf());
    if candidate.0 > newest.0 {
      newest = candidate;
    }
  }
  newest
}

/// The assertion that one fixture tree is older than the sweep's scratch cutoff, naming the entry that keeps it fresh.
fn assert_scratch_aged(path: &Path, now: DateTime<Utc>) {
  let cutoff = super::scratch_cutoff(now);
  let (newest, freshest) = newest_entry(path);
  assert!(
    newest < cutoff,
    "{} is not aged: {} is {:.3}s newer than the scratch cutoff",
    path.display(),
    freshest.display(),
    newest - cutoff
  );
}

type TreeEntry = (String, u32, u64, i64, i64, Option<Vec<u8>>);

fn snapshot_tree(path: &Path) -> Vec<TreeEntry> {
  fn visit(base: &Path, path: &Path, output: &mut Vec<TreeEntry>) {
    let mut entries = fs::read_dir(path).unwrap().map(|entry| entry.unwrap().path()).collect::<Vec<_>>();
    entries.sort();
    for entry in entries {
      let metadata = fs::symlink_metadata(&entry).unwrap();
      let kind = metadata.mode();
      let content =
        if metadata.is_file() && !metadata.file_type().is_symlink() { Some(fs::read(&entry).unwrap()) } else { None };
      output.push((
        entry.strip_prefix(base).unwrap().to_string_lossy().into_owned(),
        kind,
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        content,
      ));
      if metadata.is_dir() && !metadata.file_type().is_symlink() {
        visit(base, &entry, output);
      }
    }
  }
  let mut result = Vec::new();
  visit(path, path, &mut result);
  result
}

struct FakeProcess<'a> {
  pid: &'a str,
  cwd: &'a Path,
  cmdline: &'a [u8],
  exe: &'a Path,
  fd_target: &'a Path,
  root: &'a Path,
  maps: &'a str,
}

fn fake_process(proc_root: &Path, fixture: FakeProcess<'_>) {
  let process = proc_root.join(fixture.pid);
  fs::create_dir_all(process.join("fd")).unwrap();
  std::os::unix::fs::symlink(fixture.cwd, process.join("cwd")).unwrap();
  std::os::unix::fs::symlink(fixture.root, process.join("root")).unwrap();
  std::os::unix::fs::symlink(fixture.exe, process.join("exe")).unwrap();
  std::os::unix::fs::symlink(fixture.fd_target, process.join("fd/3")).unwrap();
  fs::write(process.join("maps"), fixture.maps).unwrap();
  fs::write(process.join("cmdline"), fixture.cmdline).unwrap();
}

struct RestorePermissions(PathBuf);

impl Drop for RestorePermissions {
  fn drop(&mut self) {
    if self.0.exists() {
      fs::set_permissions(&self.0, Permissions::from_mode(0o700)).unwrap();
    }
  }
}

fn tar_files(path: &Path) -> BTreeMap<String, Vec<u8>> {
  let file = File::open(path).unwrap();
  tar::Archive::new(file)
    .entries()
    .unwrap()
    .map(|entry| {
      let mut entry = entry.unwrap();
      let name = entry.path().unwrap().to_string_lossy().into_owned();
      let mut bytes = Vec::new();
      std::io::Read::read_to_end(&mut entry, &mut bytes).unwrap();
      (name, bytes)
    })
    .collect()
}

#[tokio::test]
#[ignore = "integration"]
async fn test_worktree_age_reopen_keep_and_unknown_owner_rules() {
  let root = tempdir().unwrap();
  let roots = fixture(root.path());
  let repo = create_repo(&root.path().join("repo"));
  let now = utc("2026-05-20T00:00:00Z");

  let (due, due_branch) = create_worktree(&roots, &repo, "due");
  write_owner(
    &roots,
    Owner {
      session_id: "due",
      worktree: &due,
      repo: &repo,
      branch: &due_branch,
      closed_at: Some(now - chrono::Duration::days(3)),
      keep_worktree: false,
      reopened: false,
    },
  );
  let (young, young_branch) = create_worktree(&roots, &repo, "young");
  write_owner(
    &roots,
    Owner {
      session_id: "young",
      worktree: &young,
      repo: &repo,
      branch: &young_branch,
      closed_at: Some(now - chrono::Duration::days(2) - chrono::Duration::hours(23)),
      keep_worktree: false,
      reopened: false,
    },
  );
  let (kept, kept_branch) = create_worktree(&roots, &repo, "kept");
  write_owner(
    &roots,
    Owner {
      session_id: "kept",
      worktree: &kept,
      repo: &repo,
      branch: &kept_branch,
      closed_at: Some(now - chrono::Duration::days(14)),
      keep_worktree: true,
      reopened: false,
    },
  );
  let (kept_young, kept_young_branch) = create_worktree(&roots, &repo, "kept-young");
  write_owner(
    &roots,
    Owner {
      session_id: "kept-young",
      worktree: &kept_young,
      repo: &repo,
      branch: &kept_young_branch,
      closed_at: Some(now - chrono::Duration::days(13)),
      keep_worktree: true,
      reopened: false,
    },
  );
  let (reopened, reopened_branch) = create_worktree(&roots, &repo, "reopened");
  write_owner(
    &roots,
    Owner {
      session_id: "reopened",
      worktree: &reopened,
      repo: &repo,
      branch: &reopened_branch,
      closed_at: Some(now - chrono::Duration::days(30)),
      keep_worktree: false,
      reopened: true,
    },
  );
  let (unknown, unknown_branch) = create_worktree(&roots, &repo, "unknown");
  let unknown_run = roots.sessions_root.join("missing-owner/data/runs/run-missing-owner");
  fs::create_dir_all(&unknown_run).unwrap();
  fs::write(
    unknown_run.join("metadata.json"),
    serde_json::to_vec(&json!({
      "id": "run-missing-owner",
      "session_id": "missing-owner",
      "repo_path": repo.to_string_lossy(),
      "branch_name": unknown_branch,
      "worktree_path": unknown.canonicalize().unwrap(),
    }))
    .unwrap(),
  )
  .unwrap();

  let report = run(&roots, SweepMode::List, now).await;
  assert_eq!(row_by_path(&report, &due).action, SweepAction::Delete);
  assert_eq!(row_by_path(&report, &kept).action, SweepAction::Delete);
  assert_eq!(row_by_path(&report, &reopened).reason, "task-open");
  assert_eq!(row_by_path(&report, &unknown).reason, "unknown-owner");
  assert!(report.rows.iter().all(|row| row.path != young));
  assert!(report.rows.iter().all(|row| row.path != kept_young));
}

#[tokio::test]
#[ignore = "integration"]
async fn test_shared_worktree_uses_newest_owner_close_and_any_keep_flag() {
  let root = tempdir().unwrap();
  let roots = fixture(root.path());
  let repo = create_repo(&root.path().join("repo"));
  let now = utc("2026-05-20T00:00:00Z");

  let (recent, recent_branch) = create_worktree(&roots, &repo, "recent-owner");
  write_owner(
    &roots,
    Owner {
      session_id: "old-owner",
      worktree: &recent,
      repo: &repo,
      branch: &recent_branch,
      closed_at: Some(now - chrono::Duration::days(30)),
      keep_worktree: false,
      reopened: false,
    },
  );
  write_owner(
    &roots,
    Owner {
      session_id: "recent-owner",
      worktree: &recent,
      repo: &repo,
      branch: &recent_branch,
      closed_at: Some(now - chrono::Duration::days(2)),
      keep_worktree: false,
      reopened: false,
    },
  );
  let (kept, kept_branch) = create_worktree(&roots, &repo, "shared-kept");
  write_owner(
    &roots,
    Owner {
      session_id: "shared-ordinary-owner",
      worktree: &kept,
      repo: &repo,
      branch: &kept_branch,
      closed_at: Some(now - chrono::Duration::days(15)),
      keep_worktree: false,
      reopened: false,
    },
  );
  write_owner(
    &roots,
    Owner {
      session_id: "shared-kept-owner",
      worktree: &kept,
      repo: &repo,
      branch: &kept_branch,
      closed_at: Some(now - chrono::Duration::days(15)),
      keep_worktree: true,
      reopened: false,
    },
  );

  let report = run(&roots, SweepMode::List, now).await;
  assert!(report.rows.iter().all(|row| row.path != recent));
  assert_eq!(row_by_path(&report, &kept).action, SweepAction::Delete);
}

#[tokio::test]
#[ignore = "integration"]
async fn test_apply_archives_worktree_changes_before_removing_it() {
  let root = tempdir().unwrap();
  for change in ["tracked", "untracked"] {
    let leg = root.path().join(change);
    let roots = fixture(&leg);
    let repo = create_repo(&leg.join("repo"));
    let (worktree, branch) = create_worktree(&roots, &repo, change);
    let run_dir = write_owner(
      &roots,
      Owner {
        session_id: change,
        worktree: &worktree,
        repo: &repo,
        branch: &branch,
        closed_at: Some(utc("2026-01-01T00:00:00Z")),
        keep_worktree: false,
        reopened: false,
      },
    );
    match change {
      "tracked" => fs::write(worktree.join("tracked.txt"), b"changed\n").unwrap(),
      "untracked" => fs::write(worktree.join("new.txt"), b"untracked contents\n").unwrap(),
      other => panic!("unknown change {other}"),
    }
    let report = run(&roots, SweepMode::Apply, utc("2026-01-20T00:00:00Z")).await;
    let row = row_by_path(&report, &worktree);
    let archive = run_dir.join("worktree-changes.tar");
    assert_eq!(row.action, SweepAction::ArchiveDelete);
    assert!(!worktree.exists());
    assert!(archive.is_file());
    let files = tar_files(&archive);
    assert!(files.contains_key("tracked.patch"));
    match change {
      "tracked" => {
        assert!(files["tracked.patch"].windows(b"changed".len()).any(|bytes| bytes == b"changed"));
        assert!(!files.contains_key("untracked/new.txt"));
      }
      "untracked" => {
        assert!(files["tracked.patch"].is_empty());
        assert_eq!(files["untracked/new.txt"], b"untracked contents\n");
      }
      other => panic!("unknown change {other}"),
    }
  }
}

#[tokio::test]
#[ignore = "integration"]
async fn test_large_untracked_and_dirty_submodule_worktrees_are_skipped() {
  let root = tempdir().unwrap();
  let roots = fixture(root.path());
  let repo = create_repo(&root.path().join("repo"));
  let submodule_source = create_repo(&root.path().join("submodule-source"));
  let submodule_text = submodule_source.to_str().unwrap();
  git(&repo, &["-c", "protocol.file.allow=always", "submodule", "add", submodule_text, "deps/lib"]);
  git(&repo, &["commit", "-m", "add submodule"]);
  let (large, large_branch) = create_worktree(&roots, &repo, "large");
  let (dirty, dirty_branch) = create_worktree(&roots, &repo, "dirty-submodule");
  git(&dirty, &["-c", "protocol.file.allow=always", "submodule", "update", "--init"]);
  write_owner(
    &roots,
    Owner {
      session_id: "large",
      worktree: &large,
      repo: &repo,
      branch: &large_branch,
      closed_at: Some(utc("2026-01-01T00:00:00Z")),
      keep_worktree: false,
      reopened: false,
    },
  );
  write_owner(
    &roots,
    Owner {
      session_id: "dirty-submodule",
      worktree: &dirty,
      repo: &repo,
      branch: &dirty_branch,
      closed_at: Some(utc("2026-01-01T00:00:00Z")),
      keep_worktree: false,
      reopened: false,
    },
  );
  fs::write(large.join("too-large.bin"), vec![b'x'; 10 * 1024 * 1024 + 1]).unwrap();
  fs::write(dirty.join("deps/lib/tracked.txt"), b"submodule edit\n").unwrap();

  let report = run(&roots, SweepMode::List, utc("2026-01-20T00:00:00Z")).await;
  assert_eq!(row_by_path(&report, &large).reason, "large-untracked");
  assert_eq!(row_by_path(&report, &dirty).reason, "dirty-submodule");
  assert!(large.is_dir());
  assert!(dirty.is_dir());
}

#[tokio::test]
#[ignore = "integration"]
async fn test_process_cwd_and_cmdline_paths_protect_a_worktree() {
  let root = tempdir().unwrap();
  for source in ["cwd", "cmdline"] {
    let leg = root.path().join(source);
    let roots = fixture(&leg);
    let repo = create_repo(&leg.join("repo"));
    let (worktree, branch) = create_worktree(&roots, &repo, source);
    write_owner(
      &roots,
      Owner {
        session_id: source,
        worktree: &worktree,
        repo: &repo,
        branch: &branch,
        closed_at: Some(utc("2026-01-01T00:00:00Z")),
        keep_worktree: false,
        reopened: false,
      },
    );
    let outside = leg.join("outside");
    fs::write(&outside, b"open file\n").unwrap();
    let (cwd, target) = match source {
      "cwd" => (worktree.as_path(), outside.clone()),
      "cmdline" => (leg.as_path(), worktree.join("tracked.txt")),
      other => panic!("unknown source {other}"),
    };
    let mut cmdline = b"python\0".to_vec();
    cmdline.extend_from_slice(target.to_str().unwrap().as_bytes());
    cmdline.push(0);
    let exe = std::env::current_exe().unwrap();
    fake_process(
      &roots.proc_root,
      FakeProcess {
        pid: "12345",
        cwd,
        cmdline: &cmdline,
        exe: &exe,
        fd_target: &outside,
        root: Path::new("/"),
        maps: "",
      },
    );

    let report = run(&roots, SweepMode::List, utc("2026-01-20T00:00:00Z")).await;
    let row = row_by_path(&report, &worktree);
    assert_eq!(row.reason, "in-use");
    assert_eq!(row.action, SweepAction::Skip);
    assert!(worktree.is_dir());
  }
}

#[tokio::test]
#[ignore = "integration"]
async fn test_process_root_exe_fd_and_maps_paths_protect_a_worktree() {
  let root = tempdir().unwrap();
  for source in ["root", "exe", "fd", "maps"] {
    let leg = root.path().join(source);
    let roots = fixture(&leg);
    let repo = create_repo(&leg.join("repo"));
    let (worktree, branch) = create_worktree(&roots, &repo, source);
    write_owner(
      &roots,
      Owner {
        session_id: source,
        worktree: &worktree,
        repo: &repo,
        branch: &branch,
        closed_at: Some(utc("2026-01-01T00:00:00Z")),
        keep_worktree: false,
        reopened: false,
      },
    );
    let outside = leg.join("outside");
    fs::write(&outside, b"open file\n").unwrap();
    let target = worktree.join("tracked.txt");
    let (exe, fd, process_root, maps) = match source {
      "exe" => (target.as_path(), outside.as_path(), Path::new("/"), String::new()),
      "fd" => (Path::new("/bin/sh"), target.as_path(), Path::new("/"), String::new()),
      "root" => (Path::new("/bin/sh"), outside.as_path(), worktree.as_path(), String::new()),
      "maps" => (
        Path::new("/bin/sh"),
        outside.as_path(),
        Path::new("/"),
        format!("1000-2000 r-xp 00000000 08:01 123 {}\n", target.display()),
      ),
      other => panic!("unknown source {other}"),
    };
    fake_process(
      &roots.proc_root,
      FakeProcess {
        pid: "12346",
        cwd: &leg,
        cmdline: b"python\0",
        exe,
        fd_target: fd,
        root: process_root,
        maps: &maps,
      },
    );

    let report = run(&roots, SweepMode::List, utc("2026-01-20T00:00:00Z")).await;
    let row = row_by_path(&report, &worktree);
    assert_eq!(row.reason, "in-use");
    assert_eq!(row.action, SweepAction::Skip);
    assert!(worktree.is_dir());
  }
}

#[tokio::test]
#[ignore = "integration"]
async fn test_a_deleted_marker_on_a_process_target_still_protects_a_worktree() {
  let root = tempdir().unwrap();
  let roots = fixture(root.path());
  let repo = create_repo(&root.path().join("repo"));
  let (worktree, branch) = create_worktree(&roots, &repo, "deleted");
  write_owner(
    &roots,
    Owner {
      session_id: "deleted",
      worktree: &worktree,
      repo: &repo,
      branch: &branch,
      closed_at: Some(utc("2026-01-01T00:00:00Z")),
      keep_worktree: false,
      reopened: false,
    },
  );
  let marked = PathBuf::from(format!("{} (deleted)", worktree.join("tracked.txt").display()));
  fake_process(
    &roots.proc_root,
    FakeProcess {
      pid: "12347",
      cwd: root.path(),
      cmdline: b"python\0",
      exe: Path::new("/bin/sh"),
      fd_target: &marked,
      root: Path::new("/"),
      maps: "",
    },
  );

  let report = run(&roots, SweepMode::List, utc("2026-01-20T00:00:00Z")).await;
  let row = row_by_path(&report, &worktree);
  assert_eq!(row.reason, "in-use");
  assert_eq!(row.action, SweepAction::Skip);
  assert!(worktree.is_dir());
}

#[tokio::test]
#[ignore = "integration"]
async fn test_scratch_age_and_nested_repository_safety() {
  let root = tempdir().unwrap();
  let roots = fixture(root.path());
  let now = utc("2026-05-20T00:00:00Z");
  let old_time = SystemTime::UNIX_EPOCH + Duration::from_secs((now - chrono::Duration::days(15)).timestamp() as u64);

  let old_clean = roots.scratch_root.join("old-clean");
  fs::create_dir(&old_clean).unwrap();
  fs::write(old_clean.join("file.txt"), b"data\n").unwrap();
  set_tree_mtime(&old_clean, old_time);
  let new = roots.scratch_root.join("new");
  fs::create_dir(&new).unwrap();
  fs::write(new.join("file.txt"), b"new\n").unwrap();

  let dirty = roots.scratch_root.join("dirty-repo");
  let dirty_repo = create_repo(&dirty.join("nested"));
  fs::write(dirty_repo.join("tracked.txt"), b"uncommitted\n").unwrap();
  set_tree_mtime(&dirty, old_time);

  let unpushed = roots.scratch_root.join("unpushed-repo");
  create_repo(&unpushed.join("nested"));
  set_tree_mtime(&unpushed, old_time);

  let detached = roots.scratch_root.join("detached-repo");
  let detached_repo = create_repo(&detached.join("nested"));
  let remote = root.path().join("remote.git");
  git(root.path(), &["init", "--bare", remote.to_str().unwrap()]);
  git(&detached_repo, &["remote", "add", "origin", remote.to_str().unwrap()]);
  git(&detached_repo, &["push", "-u", "origin", "main"]);
  git(&detached_repo, &["checkout", "--detach"]);
  fs::write(detached_repo.join("tracked.txt"), b"detached commit\n").unwrap();
  git(&detached_repo, &["commit", "-am", "detached head commit"]);
  set_tree_mtime(&detached, old_time);

  // The sweep skips a candidate whose newest entry is younger than the cutoff, so the fixture must be quiet while
  // the sweep reads it. A writer inside a tree would make that entry look fresh.
  for path in [&old_clean, &dirty, &unpushed, &detached] {
    assert_scratch_aged(path, now);
  }

  let report = run(&roots, SweepMode::List, now).await;
  assert_eq!(row_by_path(&report, &old_clean).action, SweepAction::Delete);
  assert_eq!(row_by_path(&report, &dirty).reason, "has-unsaved-repo");
  assert_eq!(row_by_path(&report, &unpushed).reason, "has-unsaved-repo");
  assert_eq!(row_by_path(&report, &detached).reason, "has-unsaved-repo");
  assert!(report.rows.iter().all(|row| row.path != new));
}

#[tokio::test]
async fn test_scratch_tree_with_unreadable_entries_is_reported_as_skipped() {
  let root = tempdir().unwrap();
  let roots = fixture(root.path());
  let unreadable = roots.scratch_root.join("unreadable");
  fs::create_dir(&unreadable).unwrap();
  fs::set_permissions(&unreadable, Permissions::from_mode(0o000)).unwrap();
  let _restore_permissions = RestorePermissions(unreadable.clone());

  let report = run(&roots, SweepMode::List, utc("2026-05-20T00:00:00Z")).await;
  let row = row_by_path(&report, &unreadable);
  assert_eq!(row.action, SweepAction::Skip);
  assert!(row.reason.starts_with("unreadable-path:"));
  assert!(unreadable.is_dir());
  fs::set_permissions(&unreadable, Permissions::from_mode(0o700)).unwrap();
}

#[tokio::test]
async fn test_scratch_child_that_cannot_be_statted_is_reported_as_unreadable() {
  let root = tempdir().unwrap();
  let roots = fixture(root.path());
  let partly_unreadable = roots.scratch_root.join("partly-unreadable");
  let listable = partly_unreadable.join("listable");
  fs::create_dir_all(&listable).unwrap();
  fs::write(listable.join("child"), b"content\n").unwrap();
  // Readable but not searchable: the names list, and the lstat of each name inside it fails.
  fs::set_permissions(&listable, Permissions::from_mode(0o600)).unwrap();
  let _restore_permissions = RestorePermissions(listable.clone());

  let report = run(&roots, SweepMode::List, utc("2026-05-20T00:00:00Z")).await;
  let row = row_by_path(&report, &partly_unreadable);
  assert_eq!(row.action, SweepAction::Skip);
  assert!(row.reason.starts_with("unreadable-path:"), "reason {}", row.reason);
}

#[tokio::test]
#[ignore = "integration"]
async fn test_cli_without_yes_is_read_only_and_reports_a_total() {
  let root = tempdir().unwrap();
  let roots = fixture(root.path());
  let repo = create_repo(&root.path().join("repo"));
  let (worktree, branch) = create_worktree(&roots, &repo, "dry-run");
  let run_dir = write_owner(
    &roots,
    Owner {
      session_id: "dry-run",
      worktree: &worktree,
      repo: &repo,
      branch: &branch,
      closed_at: Some(utc("2026-01-01T00:00:00Z")),
      keep_worktree: false,
      reopened: false,
    },
  );
  fs::write(worktree.join("tracked.txt"), b"dry-run edit\n").unwrap();
  let before = snapshot_tree(root.path());

  let report = run(&roots, SweepMode::List, utc("2026-01-20T00:00:00Z")).await;
  let row = row_by_path(&report, &worktree);
  let fields = row.to_tsv().split('\t').map(str::to_string).collect::<Vec<_>>();
  assert_eq!(fields.len(), 5);
  assert_eq!(&fields[0], "worktree");
  assert_eq!(&fields[1], worktree.to_str().unwrap());
  assert_eq!(&fields[3..], ["archive+delete", "uncommitted-changes"]);
  assert_eq!(report.total_line(), format!("total\t1\t{}\t0\t0", row.freed_bytes));
  assert!(worktree.is_dir());
  assert!(!run_dir.join("worktree-changes.tar").exists());
  assert_eq!(snapshot_tree(root.path()), before);
}

#[test]
fn test_bare_chat_events_archive_name_does_not_change_close_time() {
  let root = tempdir().unwrap();
  let session = root.path().join("sessions").join("sess");
  let archives = session.join("data").join("archives");
  fs::create_dir_all(&archives).unwrap();
  fs::write(
    archives.join("chat_events.2026-W05.jsonl"),
    "{\"type\": \"task_closed\", \"timestamp\": \"2026-02-02T00:00:00Z\"}\n",
  )
  .unwrap();
  fs::write(
    archives.join("chat_events.jsonl"),
    "{\"type\": \"task_closed\", \"timestamp\": \"2026-02-09T00:00:00Z\"}\n",
  )
  .unwrap();
  assert_eq!(super::latest_closed_at(&session).unwrap(), Some(utc("2026-02-02T00:00:00Z")));
}

/// A repository whose `main` is pushed to a bare remote, so every commit is saved and a scratch copy of it may go.
fn create_pushed_repo(root: &Path, name: &str) -> PathBuf {
  let repo = create_repo(&root.join(name));
  let remote = root.join(format!("{name}-remote.git"));
  git(root, &["init", "--bare", remote.to_str().unwrap()]);
  git(&repo, &["remote", "add", "origin", remote.to_str().unwrap()]);
  git(&repo, &["push", "-u", "origin", "main"]);
  repo
}

/// A worktree named `name` on its own branch, owned by a session closed on 2026-01-01.
fn closed_worktree(roots: &SweepRoots, repo: &Path, name: &str) -> PathBuf {
  let (worktree, branch) = create_worktree(roots, repo, name);
  write_owner(
    roots,
    Owner {
      session_id: name,
      worktree: &worktree,
      repo,
      branch: &branch,
      closed_at: Some(utc("2026-01-01T00:00:00Z")),
      keep_worktree: false,
      reopened: false,
    },
  );
  worktree
}

fn row_reason(report: &super::SweepReport, path: &Path) -> (SweepAction, String) {
  let row = row_by_path(report, path);
  (row.action, row.reason.clone())
}

#[tokio::test]
#[ignore = "integration"]
async fn test_ignored_entries_outside_the_rebuildable_whitelist_keep_a_worktree() {
  let root = tempdir().unwrap();
  let roots = fixture(root.path());
  let repo = create_repo(&root.path().join("repo"));
  fs::write(repo.join(".gitignore"), "*.bin\n__pycache__/\n.venv\n.torch-runtime.lock\n").unwrap();
  git(&repo, &["add", ".gitignore"]);
  git(&repo, &["commit", "-m", "ignore rules"]);
  // The main checkout's env, which a worktree's `.venv` symlink may point at.
  fs::create_dir_all(repo.join(".venv/bin")).unwrap();

  let cache_only = closed_worktree(&roots, &repo, "cache-only");
  fs::create_dir_all(cache_only.join("pkg/__pycache__")).unwrap();
  fs::write(cache_only.join("pkg/__pycache__/module.pyc"), b"bytecode").unwrap();
  fs::write(cache_only.join(".torch-runtime.lock"), b"").unwrap();

  let linked_env = closed_worktree(&roots, &repo, "linked-env");
  std::os::unix::fs::symlink(repo.join(".venv"), linked_env.join(".venv")).unwrap();

  let binary = closed_worktree(&roots, &repo, "binary");
  fs::write(binary.join("weights.bin"), b"checkpoint").unwrap();

  let real_env = closed_worktree(&roots, &repo, "real-env");
  fs::create_dir_all(real_env.join(".venv/bin")).unwrap();
  fs::write(real_env.join(".venv/bin/python"), b"interpreter").unwrap();

  let report = run(&roots, SweepMode::List, utc("2026-01-20T00:00:00Z")).await;
  assert_eq!(row_reason(&report, &cache_only), (SweepAction::Delete, "closed-task".to_string()));
  assert_eq!(row_reason(&report, &linked_env), (SweepAction::Delete, "closed-task".to_string()));
  assert_eq!(row_reason(&report, &binary), (SweepAction::Skip, "non-rebuildable-ignored".to_string()));
  assert_eq!(row_reason(&report, &real_env), (SweepAction::Skip, "non-rebuildable-ignored".to_string()));
}

#[tokio::test]
#[ignore = "integration"]
async fn test_head_reachable_only_from_no_other_ref_keeps_a_worktree_and_unborn_repo_is_skipped() {
  let root = tempdir().unwrap();
  let roots = fixture(root.path());
  let repo = create_repo(&root.path().join("repo"));
  let detached = closed_worktree(&roots, &repo, "detached");
  git(&detached, &["checkout", "--detach"]);
  fs::write(detached.join("tracked.txt"), b"detached commit\n").unwrap();
  git(&detached, &["commit", "-am", "commit no ref holds"]);

  let unborn = roots.worktree_root.join("unborn");
  fs::create_dir_all(&unborn).unwrap();
  git(&unborn, &["init", "-b", "main"]);
  write_owner(
    &roots,
    Owner {
      session_id: "unborn",
      worktree: &unborn,
      repo: &unborn,
      branch: "charliebot/unborn",
      closed_at: Some(utc("2026-01-01T00:00:00Z")),
      keep_worktree: false,
      reopened: false,
    },
  );

  let report = run(&roots, SweepMode::List, utc("2026-01-20T00:00:00Z")).await;
  assert_eq!(row_reason(&report, &detached), (SweepAction::Skip, "unreachable-head".to_string()));
  assert_eq!(row_reason(&report, &unborn), (SweepAction::Skip, "unborn-head".to_string()));
}

#[tokio::test]
#[ignore = "integration"]
async fn test_scratch_linked_worktrees_are_removed_through_their_main_repository() {
  let root = tempdir().unwrap();
  let roots = fixture(root.path());
  let now = utc("2026-05-20T00:00:00Z");
  let old_time = SystemTime::UNIX_EPOCH + Duration::from_secs((now - chrono::Duration::days(15)).timestamp() as u64);
  let repo = create_pushed_repo(root.path(), "repo");

  // A linked worktree inside an old scratch directory, and a scratch directory that is itself the linked worktree.
  let holder = roots.scratch_root.join("old-task");
  fs::create_dir_all(&holder).unwrap();
  let nested = holder.join("repo-submit");
  git(&repo, &["worktree", "add", "-b", "charliebot/submit", nested.to_str().unwrap(), "HEAD"]);
  set_tree_mtime(&holder, old_time);
  let itself = roots.scratch_root.join("self-linked");
  git(&repo, &["worktree", "add", "-b", "charliebot/self", itself.to_str().unwrap(), "HEAD"]);
  set_tree_mtime(&itself, old_time);
  for path in [&holder, &itself] {
    assert_scratch_aged(path, now);
  }

  let report = run(&roots, SweepMode::Apply, now).await;
  assert!(report.failures.is_empty(), "{:?}", report.failures);
  assert_eq!(row_reason(&report, &holder), (SweepAction::Delete, "old-scratch".to_string()));
  assert_eq!(row_reason(&report, &itself), (SweepAction::Delete, "old-scratch".to_string()));
  assert!(!holder.exists());
  assert!(!itself.exists());
  let listed = String::from_utf8(git(&repo, &["worktree", "list", "--porcelain"])).unwrap();
  assert!(!listed.contains("repo-submit"), "{listed}");
  assert!(!listed.contains("self-linked"), "{listed}");
}

#[tokio::test]
#[ignore = "integration"]
async fn test_scratch_git_file_pointing_outside_a_worktree_list_is_kept_as_foreign_gitdir() {
  let root = tempdir().unwrap();
  let roots = fixture(root.path());
  let now = utc("2026-05-20T00:00:00Z");
  let old_time = SystemTime::UNIX_EPOCH + Duration::from_secs((now - chrono::Duration::days(15)).timestamp() as u64);
  let candidate = roots.scratch_root.join("foreign");
  let submodule = candidate.join("sub");
  fs::create_dir_all(&submodule).unwrap();
  let elsewhere = root.path().join("elsewhere/.git/modules/sub");
  fs::write(submodule.join(".git"), format!("gitdir: {}\n", elsewhere.display())).unwrap();
  set_tree_mtime(&candidate, old_time);
  assert_scratch_aged(&candidate, now);

  let report = run(&roots, SweepMode::List, now).await;
  assert_eq!(row_reason(&report, &candidate), (SweepAction::Skip, "foreign-gitdir".to_string()));
}

#[tokio::test]
#[ignore = "integration"]
async fn test_scratch_candidate_holding_an_open_sessions_worktree_is_task_open() {
  let root = tempdir().unwrap();
  let roots = fixture(root.path());
  let now = utc("2026-05-20T00:00:00Z");
  let old_time = SystemTime::UNIX_EPOCH + Duration::from_secs((now - chrono::Duration::days(15)).timestamp() as u64);
  let candidate = roots.scratch_root.join("busy");
  let worktree = candidate.join("wt");
  fs::create_dir_all(&worktree).unwrap();
  let repo = create_repo(&root.path().join("repo"));
  write_owner(
    &roots,
    Owner {
      session_id: "busy",
      worktree: &worktree,
      repo: &repo,
      branch: "charliebot/busy",
      closed_at: None,
      keep_worktree: false,
      reopened: false,
    },
  );
  set_tree_mtime(&candidate, old_time);
  assert_scratch_aged(&candidate, now);

  let report = run(&roots, SweepMode::List, now).await;
  assert_eq!(row_reason(&report, &candidate), (SweepAction::Skip, "task-open".to_string()));
}

#[tokio::test]
#[ignore = "integration"]
async fn test_reference_command_keeps_the_named_worktree_and_not_one_whose_name_it_extends() {
  let root = tempdir().unwrap();
  let mut roots = fixture(root.path());
  let repo = create_repo(&root.path().join("repo"));
  let one = closed_worktree(&roots, &repo, "wt-1");
  let ten = closed_worktree(&roots, &repo, "wt-10");
  let records = root.path().join("records.json");
  fs::write(&records, json!([{"submit_cwd": ten.to_str().unwrap(), "state": "RUNNING"}]).to_string()).unwrap();
  roots.reference_command = Some(format!("cat {}", records.display()));

  let report = run(&roots, SweepMode::List, utc("2026-01-20T00:00:00Z")).await;
  assert_eq!(row_reason(&report, &ten), (SweepAction::Skip, "external-reference".to_string()));
  assert_eq!(row_reason(&report, &one), (SweepAction::Delete, "closed-task".to_string()));
}

#[tokio::test]
#[ignore = "integration"]
async fn test_failing_or_non_array_reference_command_fails_the_sweep_and_removes_nothing() {
  let root = tempdir().unwrap();
  let mut roots = fixture(root.path());
  let repo = create_repo(&root.path().join("repo"));
  let worktree = closed_worktree(&roots, &repo, "guarded");
  let now = utc("2026-01-20T00:00:00Z");

  for command in ["exit 1", "echo '{}'"] {
    roots.reference_command = Some(command.to_string());
    let result = run_sweep(roots.clone(), SweepMode::Apply, now, remove_worktree).await;
    assert!(result.is_err(), "reference command {command:?} must fail the sweep");
    assert!(worktree.is_dir(), "reference command {command:?} must not remove anything");
  }
}

#[tokio::test]
#[ignore = "integration"]
async fn test_open_worktree_is_held_when_the_scratch_root_is_reached_through_a_symlink() {
  let root = tempdir().unwrap();
  let mut roots = fixture(root.path());
  let now = utc("2026-05-20T00:00:00Z");
  let old_time = SystemTime::UNIX_EPOCH + Duration::from_secs((now - chrono::Duration::days(15)).timestamp() as u64);
  // The run record stores the real path (`write_owner` canonicalizes it), while the sweep walks the symlinked root.
  let link = root.path().join("scripts-link");
  std::os::unix::fs::symlink(&roots.scratch_root, &link).unwrap();
  roots.scratch_root = link;
  let candidate = roots.scratch_root.join("busy");
  let worktree = root.path().join("scripts/busy/wt");
  fs::create_dir_all(&worktree).unwrap();
  let repo = create_repo(&root.path().join("repo"));
  write_owner(
    &roots,
    Owner {
      session_id: "busy",
      worktree: &worktree,
      repo: &repo,
      branch: "charliebot/busy",
      closed_at: None,
      keep_worktree: false,
      reopened: false,
    },
  );
  set_tree_mtime(&root.path().join("scripts/busy"), old_time);
  assert_scratch_aged(&root.path().join("scripts/busy"), now);

  let report = run(&roots, SweepMode::List, now).await;
  assert_eq!(row_reason(&report, &candidate), (SweepAction::Skip, "task-open".to_string()));
}

#[tokio::test]
#[ignore = "integration"]
async fn test_a_worktree_whose_owner_record_names_a_missing_repo_is_removed() {
  let root = tempdir().unwrap();
  let roots = fixture(root.path());
  let repo = create_repo(&root.path().join("repo"));
  let (worktree, branch) = create_worktree(&roots, &repo, "missing-repo");
  let recorded = worktree.canonicalize().unwrap();
  let gone = root.path().join("gone");
  write_owner(
    &roots,
    Owner {
      session_id: "missing-repo",
      worktree: &worktree,
      repo: &gone,
      branch: &branch,
      closed_at: Some(utc("2026-01-01T00:00:00Z")),
      keep_worktree: false,
      reopened: false,
    },
  );

  let report = run(&roots, SweepMode::Apply, utc("2026-01-20T00:00:00Z")).await;
  assert!(report.failures.is_empty(), "{:?}", report.failures);
  assert_eq!(row_reason(&report, &recorded), (SweepAction::Delete, "closed-task".to_string()));
  assert!(!worktree.exists());
  let listed = String::from_utf8(git(&repo, &["worktree", "list", "--porcelain"])).unwrap();
  assert!(!listed.contains("missing-repo"), "{listed}");
}

#[tokio::test]
#[ignore = "integration"]
async fn test_the_archive_holds_a_valid_links_target_and_a_dangling_link_as_a_link() {
  let root = tempdir().unwrap();
  let roots = fixture(root.path());
  let repo = create_repo(&root.path().join("repo"));
  let (worktree, branch) = create_worktree(&roots, &repo, "links");
  let run_dir = write_owner(
    &roots,
    Owner {
      session_id: "links",
      worktree: &worktree,
      repo: &repo,
      branch: &branch,
      closed_at: Some(utc("2026-01-01T00:00:00Z")),
      keep_worktree: false,
      reopened: false,
    },
  );
  fs::write(worktree.join("target.txt"), b"link target\n").unwrap();
  std::os::unix::fs::symlink("target.txt", worktree.join("valid-link")).unwrap();
  let missing = root.path().join("missing");
  std::os::unix::fs::symlink(&missing, worktree.join("dangling-link")).unwrap();

  let report = run(&roots, SweepMode::Apply, utc("2026-01-20T00:00:00Z")).await;
  assert!(report.failures.is_empty(), "{:?}", report.failures);
  assert!(!worktree.exists());
  let mut entries = BTreeMap::new();
  for entry in tar::Archive::new(File::open(run_dir.join("worktree-changes.tar")).unwrap()).entries().unwrap() {
    let mut entry = entry.unwrap();
    let name = entry.path().unwrap().to_string_lossy().into_owned();
    let link = entry.link_name().unwrap().map(|target| target.to_string_lossy().into_owned());
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut entry, &mut bytes).unwrap();
    entries.insert(name, (entry.header().entry_type(), link, bytes));
  }
  assert_eq!(entries["untracked/target.txt"].2, b"link target\n");
  assert_eq!(entries["untracked/valid-link"].0, tar::EntryType::Regular);
  assert_eq!(entries["untracked/valid-link"].2, b"link target\n");
  assert_eq!(entries["untracked/dangling-link"].0, tar::EntryType::Symlink);
  assert_eq!(entries["untracked/dangling-link"].1.as_deref(), Some(missing.to_str().unwrap()));
}

#[tokio::test]
#[ignore = "integration"]
async fn test_a_read_only_scratch_tree_is_removed_and_the_outside_link_target_keeps_its_mode() {
  let root = tempdir().unwrap();
  let roots = fixture(root.path());
  let now = utc("2026-05-20T00:00:00Z");
  let old_time = SystemTime::UNIX_EPOCH + Duration::from_secs((now - chrono::Duration::days(15)).timestamp() as u64);

  let candidate = roots.scratch_root.join("go-cache");
  let locked = candidate.join("pkg/mod");
  let deep = candidate.join("pkg/mod/cache/download");
  fs::create_dir_all(&deep).unwrap();
  fs::write(deep.join("mod.zip"), b"zip\n").unwrap();
  let outside = root.path().join("outside-target");
  fs::create_dir(&outside).unwrap();
  fs::write(outside.join("kept.txt"), b"kept\n").unwrap();
  std::os::unix::fs::symlink(&outside, candidate.join("outside-link")).unwrap();
  set_tree_mtime(&candidate, old_time);
  // The link's own mtime is fresh at creation; the age gate reads it, so it must be aged like the rest.
  let aged =
    Command::new("touch").arg("-h").arg("-r").arg(&candidate).arg(candidate.join("outside-link")).status().unwrap();
  assert!(aged.success());
  fs::set_permissions(&locked, Permissions::from_mode(0o555)).unwrap();
  fs::set_permissions(&deep, Permissions::from_mode(0o555)).unwrap();
  fs::set_permissions(&outside, Permissions::from_mode(0o555)).unwrap();
  let _restore_locked = RestorePermissions(locked.clone());
  let _restore_deep = RestorePermissions(deep.clone());
  let _restore_outside = RestorePermissions(outside.clone());
  assert_scratch_aged(&candidate, now);

  let report = run(&roots, SweepMode::Apply, now).await;
  assert!(report.failures.is_empty(), "{:?}", report.failures);
  assert_eq!(row_reason(&report, &candidate), (SweepAction::Delete, "old-scratch".to_string()));
  assert!(!candidate.exists());
  assert_eq!(fs::read(outside.join("kept.txt")).unwrap(), b"kept\n");
  assert_eq!(fs::metadata(&outside).unwrap().permissions().mode() & 0o777, 0o555);
}

/// A worktree removal that always fails, reporting like the real remover does.
fn fail_removal(request: WorktreeRemoval<'_>) -> BoxFuture<'_, Option<String>> {
  Box::pin(
    async move { Some(format!("{} cleanup failed for {}: synthetic refusal", request.label, request.worktree_path)) },
  )
}

#[tokio::test]
#[ignore = "integration"]
async fn test_a_failed_row_reports_the_failure_line_and_the_full_summary() {
  let root = tempdir().unwrap();
  let roots = fixture(root.path());
  let repo = create_repo(&root.path().join("repo"));
  let (worktree, branch) = create_worktree(&roots, &repo, "bad");
  write_owner(
    &roots,
    Owner {
      session_id: "bad",
      worktree: &worktree,
      repo: &repo,
      branch: &branch,
      closed_at: Some(utc("2026-01-01T00:00:00Z")),
      keep_worktree: false,
      reopened: false,
    },
  );
  let good = roots.scratch_root.join("old-scratch");
  fs::create_dir(&good).unwrap();
  fs::write(good.join("file.txt"), b"data\n").unwrap();
  let now = utc("2026-05-20T00:00:00Z");
  let old_time = SystemTime::UNIX_EPOCH + Duration::from_secs((now - chrono::Duration::days(15)).timestamp() as u64);
  set_tree_mtime(&good, old_time);
  assert_scratch_aged(&good, now);
  let worktree_path = worktree.canonicalize().unwrap();

  let report = run_sweep(roots.clone(), SweepMode::Apply, now, fail_removal).await.unwrap();
  let error = crate::handler_result(&report).expect_err("a failed row fails the handler");
  let message = error.to_string();
  let (first, rest) = message.split_once('\n').expect("the summary follows the failure line");
  assert_eq!(
    first,
    format!(
      "disk_sweep failed: {}: Disk sweep worktree cleanup failed for {}: synthetic refusal",
      worktree_path.display(),
      worktree_path.display()
    )
  );
  assert_eq!(rest, report.apply_summary());
  assert!(rest.contains(&format!("scratch\t{}", good.display())), "{rest}");
  assert!(rest.contains("skipped\tapply-failed\t1"), "{rest}");
  assert!(rest.contains("total\t"), "{rest}");
  assert!(!good.exists());
  assert!(worktree.is_dir());
}
