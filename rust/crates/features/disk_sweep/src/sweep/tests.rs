use std::collections::BTreeMap;
use std::fs::{self, File, FileTimes, Permissions};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use serde_json::json;
use tempfile::tempdir;

use super::{SweepAction, SweepRoots, run_sweep};

fn fixture(base: &Path) -> SweepRoots {
  let roots = SweepRoots {
    worktree_root: base.join("worktrees"),
    scratch_root: base.join("scripts"),
    sessions_root: base.join("sessions"),
    proc_root: base.join("proc"),
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

async fn run(roots: &SweepRoots, apply: bool, now: DateTime<Utc>) -> super::SweepReport {
  run_sweep(roots.clone(), apply, now).await.unwrap()
}

fn utc(value: &str) -> DateTime<Utc> {
  DateTime::parse_from_rfc3339(value).unwrap().to_utc()
}

fn row_by_path<'a>(report: &'a super::SweepReport, path: &Path) -> &'a super::SweepRow {
  let row = report.rows.iter().find(|row| row.path == path);
  assert!(row.is_some(), "no sweep row for {}", path.display());
  row.unwrap()
}

fn set_tree_mtime(path: &Path, timestamp: SystemTime) {
  for entry in fs::read_dir(path).unwrap() {
    let entry = entry.unwrap();
    let entry_path = entry.path();
    if entry.file_type().unwrap().is_dir() {
      set_tree_mtime(&entry_path, timestamp);
    }
    File::open(&entry_path).unwrap().set_times(FileTimes::new().set_modified(timestamp)).unwrap();
  }
  File::open(path).unwrap().set_times(FileTimes::new().set_modified(timestamp)).unwrap();
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
    fs::set_permissions(&self.0, Permissions::from_mode(0o700)).unwrap();
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

  let report = run(&roots, false, now).await;
  assert_eq!(row_by_path(&report, &due).action, SweepAction::Delete);
  assert_eq!(row_by_path(&report, &kept).action, SweepAction::Delete);
  assert_eq!(row_by_path(&report, &reopened).reason, "task-open");
  assert_eq!(row_by_path(&report, &unknown).reason, "unknown-owner");
  assert!(report.rows.iter().all(|row| row.path != young));
  assert!(report.rows.iter().all(|row| row.path != kept_young));
}

#[tokio::test]
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

  let report = run(&roots, false, now).await;
  assert!(report.rows.iter().all(|row| row.path != recent));
  assert_eq!(row_by_path(&report, &kept).action, SweepAction::Delete);
}

#[tokio::test]
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
    if change == "tracked" {
      fs::write(worktree.join("tracked.txt"), b"changed\n").unwrap();
    } else {
      fs::write(worktree.join("new.txt"), b"untracked contents\n").unwrap();
    }
    let report = run(&roots, true, utc("2026-01-20T00:00:00Z")).await;
    let row = row_by_path(&report, &worktree);
    let archive = run_dir.join("worktree-changes.tar");
    assert_eq!(row.action, SweepAction::ArchiveDelete);
    assert!(!worktree.exists());
    assert!(archive.is_file());
    let files = tar_files(&archive);
    assert!(files.contains_key("tracked.patch"));
    if change == "tracked" {
      assert!(files["tracked.patch"].windows(b"changed".len()).any(|bytes| bytes == b"changed"));
      assert!(!files.contains_key("untracked/new.txt"));
    } else {
      assert!(files["tracked.patch"].is_empty());
      assert_eq!(files["untracked/new.txt"], b"untracked contents\n");
    }
  }
}

#[tokio::test]
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

  let report = run(&roots, false, utc("2026-01-20T00:00:00Z")).await;
  assert_eq!(row_by_path(&report, &large).reason, "large-untracked");
  assert_eq!(row_by_path(&report, &dirty).reason, "dirty-submodule");
  assert!(large.is_dir());
  assert!(dirty.is_dir());
}

#[tokio::test]
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
    let cwd = if source == "cwd" { worktree.as_path() } else { leg.as_path() };
    let target = if source == "cmdline" { worktree.join("tracked.txt") } else { outside.clone() };
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

    let report = run(&roots, false, utc("2026-01-20T00:00:00Z")).await;
    let row = row_by_path(&report, &worktree);
    assert_eq!(row.reason, "in-use");
    assert_eq!(row.action, SweepAction::Skip);
    assert!(worktree.is_dir());
  }
}

#[tokio::test]
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
    let exe = if source == "exe" { target.as_path() } else { Path::new("/bin/sh") };
    let fd = if source == "fd" { target.as_path() } else { outside.as_path() };
    let process_root = if source == "root" { worktree.as_path() } else { Path::new("/") };
    let maps = if source == "maps" {
      format!("1000-2000 r-xp 00000000 08:01 123 {}\n", target.display())
    } else {
      String::new()
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

    let report = run(&roots, false, utc("2026-01-20T00:00:00Z")).await;
    let row = row_by_path(&report, &worktree);
    assert_eq!(row.reason, "in-use");
    assert_eq!(row.action, SweepAction::Skip);
    assert!(worktree.is_dir());
  }
}

#[tokio::test]
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

  let report = run(&roots, false, now).await;
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

  let report = run(&roots, false, utc("2026-05-20T00:00:00Z")).await;
  let row = row_by_path(&report, &unreadable);
  assert_eq!(row.action, SweepAction::Skip);
  assert!(row.reason.starts_with("unreadable-path:"));
  assert!(unreadable.is_dir());
  fs::set_permissions(&unreadable, Permissions::from_mode(0o700)).unwrap();
}

#[tokio::test]
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

  let report = run(&roots, false, utc("2026-01-20T00:00:00Z")).await;
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
