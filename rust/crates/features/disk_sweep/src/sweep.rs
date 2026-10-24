use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::{event_types, paths};
use charliebot_store::chat_events::is_chat_events_archive;
use charliebot_store::metadata::TaskSpec;
use charliebot_store::run_records::{RUN_METADATA_NAME, RUNS_DIR_NAME, RunRecord};
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::{archive, git, process};

const CLOSED_WORKTREE_DAYS: i64 = 3;
const KEPT_WORKTREE_DAYS: i64 = 14;
const SCRATCH_DAYS: i64 = 14;

#[derive(Clone, Debug)]
pub struct SweepRoots {
  pub worktree_root: PathBuf,
  pub scratch_root: PathBuf,
  pub sessions_root: PathBuf,
  pub proc_root: PathBuf,
}

impl SweepRoots {
  pub fn from_config(cfg: &CharlieBotConfig) -> Self {
    Self {
      worktree_root: PathBuf::from(&cfg.paths.worktree_dir),
      scratch_root: PathBuf::from(&cfg.paths.scratch_dir),
      sessions_root: cfg.sessions_dir(),
      proc_root: PathBuf::from("/proc"),
    }
  }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SweepKind {
  Worktree,
  Scratch,
}

impl SweepKind {
  fn as_str(self) -> &'static str {
    match self {
      Self::Worktree => "worktree",
      Self::Scratch => "scratch",
    }
  }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SweepAction {
  Skip,
  Delete,
  ArchiveDelete,
}

impl SweepAction {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Skip => "skip",
      Self::Delete => "delete",
      Self::ArchiveDelete => "archive+delete",
    }
  }
}

#[derive(Clone, Debug)]
pub struct SweepRow {
  pub kind: SweepKind,
  pub path: PathBuf,
  pub freed_bytes: u64,
  pub action: SweepAction,
  pub reason: String,
  repo_path: Option<String>,
  branch_name: Option<String>,
  owner_run: Option<RunRecord>,
  owner_session_id: Option<String>,
  archive_path: Option<PathBuf>,
}

impl SweepRow {
  pub fn to_tsv(&self) -> String {
    [
      self.kind.as_str().to_string(),
      paths::display(&self.path),
      self.freed_bytes.to_string(),
      self.action.as_str().to_string(),
      self.reason.clone(),
    ]
    .iter()
    .map(|field| field.replace(['\t', '\r', '\n'], " "))
    .collect::<Vec<_>>()
    .join("\t")
  }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SweepFailure {
  pub path: PathBuf,
  pub reason: String,
}

#[derive(Clone, Debug, Default)]
pub struct SweepReport {
  pub rows: Vec<SweepRow>,
  pub deleted_count: usize,
  pub freed_bytes: u64,
  pub skipped_count: usize,
  pub unreadable_proc_count: usize,
  pub failures: Vec<SweepFailure>,
}

impl SweepReport {
  pub fn total_line(&self) -> String {
    format!(
      "total\t{}\t{}\t{}\t{}",
      self.deleted_count, self.freed_bytes, self.skipped_count, self.unreadable_proc_count
    )
  }
}

/// Lists or applies one sweep at the supplied time and roots.
pub async fn run_sweep(roots: SweepRoots, apply: bool, now: DateTime<Utc>) -> Result<SweepReport> {
  let scan_roots = roots.clone();
  let scanned = on_blocking(move || scan(scan_roots, now)).await?;
  let mut report = scanned.report;
  let mut unreadable = scanned.unreadable;
  if apply {
    report.deleted_count = 0;
    report.freed_bytes = 0;
    for index in 0..report.rows.len() {
      if report.rows[index].action == SweepAction::Skip {
        continue;
      }
      let path = report.rows[index].path.clone();
      let proc_root = roots.proc_root.clone();
      let (used, unreadable_now) = on_blocking(move || {
        let (used_paths, unreadable) = process::in_use_paths(std::slice::from_ref(&path), &proc_root)?;
        Ok((used_paths.contains(&path), unreadable))
      })
      .await?;
      unreadable.extend(unreadable_now);
      if used {
        mark_skipped(&mut report.rows[index], "in-use");
        continue;
      }
      let row = report.rows[index].clone();
      let apply_roots = roots.clone();
      let (prepared, unreadable_now) = on_blocking(move || Ok(prepare_apply(row, apply_roots, now))).await?;
      unreadable.extend(unreadable_now);
      match prepared {
        Ok(PreparedApply::Skip(row)) => report.rows[index] = row,
        Ok(PreparedApply::ScratchRemoved(row)) => {
          report.freed_bytes += row.freed_bytes;
          report.deleted_count += 1;
          report.rows[index] = row;
        }
        Ok(PreparedApply::RemoveWorktree(row)) => match remove_worktree(&row, &roots).await {
          Ok(()) => {
            report.freed_bytes += row.freed_bytes;
            report.deleted_count += 1;
            report.rows[index] = row;
          }
          Err(error) => record_apply_failure(&mut report, index, row, error.to_string()),
        },
        Err(error) => {
          let row = report.rows[index].clone();
          record_apply_failure(&mut report, index, row, error.to_string());
        }
      }
    }
    report.skipped_count = report.rows.iter().filter(|row| row.action == SweepAction::Skip).count();
  }
  report.unreadable_proc_count = unreadable.len();
  Ok(report)
}

struct ScanResult {
  report: SweepReport,
  unreadable: HashSet<u32>,
}

fn scan(roots: SweepRoots, now: DateTime<Utc>) -> Result<ScanResult> {
  let worktree_root = absolute(&roots.worktree_root)?;
  let scratch_root = absolute(&roots.scratch_root)?;
  let mut rows = worktree_rows(&roots.sessions_root, &worktree_root, now)?;
  rows.extend(scratch_rows(&scratch_root, now)?);
  let resolved_scratch_root = paths::realpath(&scratch_root);
  for row in &mut rows {
    if row.action != SweepAction::Skip
      && ((row.kind == SweepKind::Worktree && row.path == worktree_root)
        || (row.kind == SweepKind::Scratch && row.path == resolved_scratch_root))
    {
      mark_skipped(row, "unsafe-root");
    }
  }
  let eligible =
    rows.iter().filter(|row| row.action != SweepAction::Skip).map(|row| row.path.clone()).collect::<Vec<_>>();
  let (used, unreadable) = process::in_use_paths(&eligible, &roots.proc_root)?;
  for row in &mut rows {
    if used.contains(&row.path) {
      mark_skipped(row, "in-use");
    }
  }
  for row in &mut rows {
    if row.kind == SweepKind::Worktree && row.action != SweepAction::Skip {
      worktree_preflight(row)?;
    }
  }
  let eligible =
    rows.iter().filter(|row| row.action != SweepAction::Skip).map(|row| row.path.clone()).collect::<Vec<_>>();
  let freed = inode_blocks(&eligible)?;
  for row in &mut rows {
    if row.action != SweepAction::Skip {
      row.freed_bytes = freed.get(&row.path).copied().unwrap_or_default();
    }
  }
  let deleted_count = rows.iter().filter(|row| row.action != SweepAction::Skip).count();
  let freed_bytes = rows.iter().filter(|row| row.action != SweepAction::Skip).map(|row| row.freed_bytes).sum();
  let skipped_count = rows.iter().filter(|row| row.action == SweepAction::Skip).count();
  Ok(ScanResult {
    report: SweepReport {
      rows,
      deleted_count,
      freed_bytes,
      skipped_count,
      unreadable_proc_count: unreadable.len(),
      failures: Vec::new(),
    },
    unreadable,
  })
}

type RunGroups = BTreeMap<PathBuf, BTreeMap<String, Vec<RunRecord>>>;

fn load_run_groups(sessions_root: &Path) -> Result<RunGroups> {
  let mut groups: RunGroups = BTreeMap::new();
  if !sessions_root.is_dir() {
    return Ok(groups);
  }
  for session_dir in git::sorted_entries(sessions_root)? {
    if !session_dir.path().is_dir() {
      continue;
    }
    let runs_dir = session_dir.path().join("data").join(RUNS_DIR_NAME);
    if !runs_dir.is_dir() {
      continue;
    }
    for run_dir in git::sorted_entries(&runs_dir)? {
      if !run_dir.path().is_dir() {
        continue;
      }
      let metadata_path = run_dir.path().join(RUN_METADATA_NAME);
      if !metadata_path.is_file() {
        continue;
      }
      let content =
        fs::read(&metadata_path).with_context(|| format!("reading run metadata {}", metadata_path.display()))?;
      let record: RunRecord = serde_json::from_slice(&content)
        .with_context(|| format!("unreadable run metadata at {}", metadata_path.display()))?;
      let session_id = session_dir.file_name().to_string_lossy().into_owned();
      let run_id = run_dir.file_name().to_string_lossy().into_owned();
      if record.session_id != session_id || record.id != run_id {
        bail!(
          "run metadata at {} names session/run {:?}/{:?}, but its directory is {:?}/{:?}",
          metadata_path.display(),
          record.session_id,
          record.id,
          session_id,
          run_id
        );
      }
      let Some(worktree_path) = record.worktree_path.as_deref() else {
        continue;
      };
      let worktree_path = absolute(&PathBuf::from(paths::expanduser(worktree_path)))?;
      groups.entry(worktree_path).or_default().entry(record.session_id.clone()).or_default().push(record);
    }
  }
  Ok(groups)
}

fn worktree_rows(sessions_root: &Path, worktree_root: &Path, now: DateTime<Utc>) -> Result<Vec<SweepRow>> {
  let groups = load_run_groups(sessions_root)?;
  let mut rows = Vec::new();
  for (path, owners) in groups {
    if !path.is_dir() {
      continue;
    }
    let mut row = SweepRow {
      kind: SweepKind::Worktree,
      path: path.clone(),
      freed_bytes: 0,
      action: SweepAction::Skip,
      reason: String::new(),
      repo_path: None,
      branch_name: None,
      owner_run: None,
      owner_session_id: None,
      archive_path: None,
    };
    if fs::symlink_metadata(&path)?.file_type().is_symlink() {
      row.reason = "worktree-symlink".to_string();
      rows.push(row);
      continue;
    }
    if !path.starts_with(worktree_root) {
      row.reason = "outside-worktree-root".to_string();
      rows.push(row);
      continue;
    }
    let mut owner_closes = Vec::new();
    let mut any_keep = false;
    let mut owner_missing = false;
    let mut owner_open = false;
    for session_id in owners.keys() {
      let Some(keep) = task_keep_worktree(sessions_root, session_id)? else {
        owner_missing = true;
        continue;
      };
      any_keep |= keep;
      match latest_closed_at(&sessions_root.join(session_id))? {
        Some(closed_at) => owner_closes.push(closed_at),
        None => owner_open = true,
      }
    }
    if owner_missing {
      row.reason = "unknown-owner".to_string();
      rows.push(row);
      continue;
    }
    if owner_open {
      row.reason = "task-open".to_string();
      rows.push(row);
      continue;
    }
    if owner_closes.len() != owners.len() {
      row.reason = "unknown-owner".to_string();
      rows.push(row);
      continue;
    }
    let newest_close = owner_closes.into_iter().max().expect("every owner has one close time");
    let age_days = (now - newest_close).num_seconds() as f64 / 86_400.0;
    let wait_days = if any_keep { KEPT_WORKTREE_DAYS } else { CLOSED_WORKTREE_DAYS };
    if age_days < wait_days as f64 {
      continue;
    }
    let Some((owner_session_id, run)) = choose_owner_run(&owners) else {
      row.reason = "unknown-owner".to_string();
      rows.push(row);
      continue;
    };
    row.owner_session_id = Some(owner_session_id.clone());
    row.repo_path = run.repo_path.clone();
    row.branch_name = run.branch_name.clone();
    row.archive_path = Some(
      sessions_root.join(owner_session_id).join("data").join(RUNS_DIR_NAME).join(&run.id).join("worktree-changes.tar"),
    );
    row.owner_run = Some(run);
    row.action = SweepAction::Delete;
    row.reason = "closed-task".to_string();
    rows.push(row);
  }
  Ok(rows)
}

fn task_keep_worktree(sessions_root: &Path, session_id: &str) -> Result<Option<bool>> {
  let metadata_path = sessions_root.join(session_id).join("metadata.json");
  if !metadata_path.is_file() {
    return Ok(None);
  }
  let content =
    fs::read(&metadata_path).with_context(|| format!("reading session metadata {}", metadata_path.display()))?;
  let metadata: Value = serde_json::from_slice(&content)
    .with_context(|| format!("reading session metadata {}", metadata_path.display()))?;
  let metadata = metadata
    .as_object()
    .with_context(|| format!("session metadata at {} is not an object", metadata_path.display()))?;
  let Some(task) = metadata.get("task").filter(|task| task.is_object()) else {
    return Ok(None);
  };
  let task: TaskSpec = serde_json::from_value(task.clone())
    .with_context(|| format!("invalid task metadata at {}", metadata_path.display()))?;
  Ok(Some(task.keep_worktree))
}

fn latest_closed_at(session_dir: &Path) -> Result<Option<DateTime<Utc>>> {
  let archives_dir = session_dir.join("data/archives");
  let mut event_paths = if archives_dir.is_dir() {
    git::sorted_entries(&archives_dir)?
      .into_iter()
      .map(|entry| entry.path())
      .filter(|path| is_chat_events_archive(&path.file_name().unwrap_or_default().to_string_lossy()))
      .collect::<Vec<_>>()
  } else {
    Vec::new()
  };
  let live_path = session_dir.join("data/chat_events.jsonl");
  if live_path.is_file() {
    event_paths.push(live_path);
  }
  let mut latest = None;
  for event_path in event_paths {
    let file = fs::File::open(&event_path).with_context(|| format!("reading chat events {}", event_path.display()))?;
    for (line_index, line) in BufReader::new(file).lines().enumerate() {
      let line = line.with_context(|| format!("reading {}:{}", event_path.display(), line_index + 1))?;
      if line.trim().is_empty() {
        continue;
      }
      let event: Value = serde_json::from_str(&line)
        .with_context(|| format!("invalid chat event at {}:{}", event_path.display(), line_index + 1))?;
      event
        .as_object()
        .with_context(|| format!("chat event at {}:{} is not an object", event_path.display(), line_index + 1))?;
      let kind = event.get("type").and_then(Value::as_str).unwrap_or_default();
      if [event_types::CLONE_START, event_types::TASK_CLOSED, event_types::TASK_REOPENED].contains(&kind) {
        latest = Some(event);
      }
    }
  }
  let Some(event) = latest else {
    return Ok(None);
  };
  if event.get("type").and_then(Value::as_str) != Some(event_types::TASK_CLOSED) {
    return Ok(None);
  }
  let timestamp =
    event.get("timestamp").and_then(Value::as_str).with_context(|| "task_closed event has no timestamp")?;
  let closed_at = DateTime::parse_from_rfc3339(timestamp)
    .with_context(|| format!("invalid task_closed timestamp: {timestamp}"))?
    .with_timezone(&Utc);
  Ok(Some(closed_at))
}

fn choose_owner_run(owners: &BTreeMap<String, Vec<RunRecord>>) -> Option<(String, RunRecord)> {
  owners
    .iter()
    .flat_map(|(session_id, records)| {
      records.iter().filter_map(|record| {
        record
          .repo_path
          .as_deref()
          .filter(|path| !path.is_empty())
          .zip(record.branch_name.as_deref().filter(|branch| !branch.is_empty()))
          .map(|_| (session_id.clone(), record.clone()))
      })
    })
    .min_by(|(_, left), (_, right)| left.started_at.cmp(&right.started_at).then_with(|| left.id.cmp(&right.id)))
}

fn scratch_rows(scratch_root: &Path, now: DateTime<Utc>) -> Result<Vec<SweepRow>> {
  if !scratch_root.is_dir() {
    return Ok(Vec::new());
  }
  let cutoff = now.timestamp() as f64 + now.timestamp_subsec_nanos() as f64 / 1_000_000_000.0
    - (SCRATCH_DAYS * 24 * 60 * 60) as f64;
  let mut rows = Vec::new();
  for entry in git::sorted_entries(scratch_root)? {
    let path = absolute(&entry.path())?;
    if !path.is_dir() {
      continue;
    }
    let mut row = SweepRow {
      kind: SweepKind::Scratch,
      path: path.clone(),
      freed_bytes: 0,
      action: SweepAction::Skip,
      reason: String::new(),
      repo_path: None,
      branch_name: None,
      owner_run: None,
      owner_session_id: None,
      archive_path: None,
    };
    if fs::symlink_metadata(&path)?.file_type().is_symlink() {
      row.reason = "scratch-symlink".to_string();
      rows.push(row);
      continue;
    }
    let newest = match tree_latest_mtime(&path) {
      Ok(newest) => newest,
      Err(error) if error.downcast_ref::<io::Error>().is_some() => {
        row.reason = format!("unreadable-path: {error}");
        rows.push(row);
        continue;
      }
      Err(error) => return Err(error),
    };
    if newest >= cutoff {
      continue;
    }
    let mut has_unsaved_repo = false;
    for repo in nested_repositories(&path)? {
      if git::repo_has_unsaved_changes(&repo)? {
        has_unsaved_repo = true;
        break;
      }
    }
    if has_unsaved_repo {
      row.reason = "has-unsaved-repo".to_string();
      rows.push(row);
      continue;
    }
    row.action = SweepAction::Delete;
    row.reason = "old-scratch".to_string();
    rows.push(row);
  }
  Ok(rows)
}

fn tree_latest_mtime(path: &Path) -> Result<f64> {
  let metadata = fs::symlink_metadata(path).with_context(|| format!("inspecting {}", path.display()))?;
  let mut latest = mtime(&metadata);
  walk_tree(path, &mut |_entry, metadata| {
    latest = latest.max(mtime(metadata));
    Ok(())
  })?;
  Ok(latest)
}

fn nested_repositories(path: &Path) -> Result<Vec<PathBuf>> {
  fn visit(path: &Path, repositories: &mut Vec<PathBuf>) -> Result<()> {
    let entries = git::sorted_entries(path)?;
    if entries.iter().any(|entry| entry.file_name() == ".git") {
      repositories.push(path.to_path_buf());
    }
    for entry in entries {
      if entry.file_name() == ".git" {
        continue;
      }
      let metadata = fs::symlink_metadata(entry.path())
        .with_context(|| format!("inspecting scratch path {}", entry.path().display()))?;
      if metadata.is_dir() && !metadata.file_type().is_symlink() {
        visit(&entry.path(), repositories)?;
      }
    }
    Ok(())
  }
  let mut result = Vec::new();
  visit(path, &mut result)?;
  Ok(result)
}

fn walk_tree(path: &Path, visit: &mut impl FnMut(&Path, &fs::Metadata) -> Result<()>) -> Result<()> {
  for entry in git::sorted_entries(path)? {
    let entry_path = entry.path();
    let metadata =
      fs::symlink_metadata(&entry_path).with_context(|| format!("inspecting scratch path {}", entry_path.display()))?;
    visit(&entry_path, &metadata)?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
      walk_tree(&entry_path, visit)?;
    }
  }
  Ok(())
}

fn mtime(metadata: &fs::Metadata) -> f64 {
  metadata.mtime() as f64 + metadata.mtime_nsec() as f64 / 1_000_000_000.0
}

fn worktree_preflight(row: &mut SweepRow) -> Result<()> {
  let inspection = git::inspect_worktree(&row.path)?;
  if let Some(reason) = inspection.skip_reason {
    mark_skipped(row, reason);
  } else if inspection.status.is_empty() {
    row.action = SweepAction::Delete;
    row.reason = "closed-task".to_string();
  } else {
    row.action = SweepAction::ArchiveDelete;
    row.reason = "uncommitted-changes".to_string();
  }
  Ok(())
}

fn inode_blocks(paths: &[PathBuf]) -> Result<HashMap<PathBuf, u64>> {
  let mut inodes: HashMap<(u64, u64), (u64, u64, u64, PathBuf)> = HashMap::new();
  for path in paths {
    collect_inodes(path, path, &mut inodes)?;
  }
  let mut freed = HashMap::new();
  for (links, blocks, seen, first_path) in inodes.values() {
    if seen >= links || *links == 0 {
      *freed.entry(first_path.clone()).or_insert(0) += *blocks;
    }
  }
  Ok(freed)
}

fn collect_inodes(
  path: &Path,
  candidate: &Path,
  inodes: &mut HashMap<(u64, u64), (u64, u64, u64, PathBuf)>,
) -> Result<()> {
  for entry in git::sorted_entries(path)? {
    let entry_path = entry.path();
    let metadata = fs::symlink_metadata(&entry_path)
      .with_context(|| format!("inspecting removal candidate {}", entry_path.display()))?;
    let key = (metadata.dev(), metadata.ino());
    match inodes.get_mut(&key) {
      Some(record) => record.2 += 1,
      None => {
        inodes.insert(key, (metadata.nlink(), metadata.blocks() * 512, 1, candidate.to_path_buf()));
      }
    }
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
      collect_inodes(&entry_path, candidate, inodes)?;
    }
  }
  Ok(())
}

fn prepare_apply(mut row: SweepRow, roots: SweepRoots, now: DateTime<Utc>) -> (Result<PreparedApply>, HashSet<u32>) {
  let mut unreadable = HashSet::new();
  let result = (|| -> Result<PreparedApply> {
    if row.kind == SweepKind::Worktree {
      let inspection = git::inspect_worktree(&row.path)?;
      if let Some(reason) = inspection.skip_reason {
        mark_skipped(&mut row, reason);
        return Ok(PreparedApply::Skip(row));
      }
      let has_changes = !inspection.status.is_empty();
      if !has_changes {
        row.action = SweepAction::Delete;
        row.reason = "closed-task".to_string();
      } else {
        row.action = SweepAction::ArchiveDelete;
        row.reason = "uncommitted-changes".to_string();
        let archive_path = row.archive_path.as_deref().context("worktree has no owning run archive path")?;
        archive::archive_worktree(&row.path, archive_path, &inspection.untracked)?;
      }
      if has_changes {
        let (used, unreadable_now) = process::in_use_paths(std::slice::from_ref(&row.path), &roots.proc_root)?;
        unreadable.extend(unreadable_now);
        if used.contains(&row.path) {
          mark_skipped(&mut row, "in-use");
          return Ok(PreparedApply::Skip(row));
        }
      }
      Ok(PreparedApply::RemoveWorktree(row))
    } else {
      scratch_preflight(&mut row, now)?;
      if row.action == SweepAction::Skip {
        return Ok(PreparedApply::Skip(row));
      }
      let (used, unreadable_now) = process::in_use_paths(&[row.path.clone()], &roots.proc_root)?;
      unreadable.extend(unreadable_now);
      if used.contains(&row.path) {
        mark_skipped(&mut row, "in-use");
        return Ok(PreparedApply::Skip(row));
      }
      fs::remove_dir_all(&row.path).with_context(|| format!("removing scratch directory {}", row.path.display()))?;
      Ok(PreparedApply::ScratchRemoved(row))
    }
  })();
  (result, unreadable)
}

fn scratch_preflight(row: &mut SweepRow, now: DateTime<Utc>) -> Result<()> {
  let cutoff = now.timestamp() as f64 + now.timestamp_subsec_nanos() as f64 / 1_000_000_000.0
    - (SCRATCH_DAYS * 24 * 60 * 60) as f64;
  if tree_latest_mtime(&row.path)? >= cutoff {
    mark_skipped(row, "too-young");
    return Ok(());
  }
  for repo in nested_repositories(&row.path)? {
    if git::repo_has_unsaved_changes(&repo)? {
      mark_skipped(row, "has-unsaved-repo");
      return Ok(());
    }
  }
  Ok(())
}

enum PreparedApply {
  Skip(SweepRow),
  RemoveWorktree(SweepRow),
  ScratchRemoved(SweepRow),
}

async fn remove_worktree(row: &SweepRow, roots: &SweepRoots) -> Result<()> {
  let run = row.owner_run.as_ref().context("worktree has no owner run")?;
  let repo_path = row.repo_path.as_deref().context("worktree owner run has no repository path")?;
  let branch_name = row.branch_name.as_deref().context("worktree owner run has no branch name")?;
  let worktree_path = paths::display(&row.path);
  let run_id = run.id.clone();
  let owner_session_id = row.owner_session_id.clone().context("worktree has no owner session")?;
  let worktree_root = roots.worktree_root.clone();
  let repo_path = repo_path.to_string();
  let branch_name = branch_name.to_string();
  let handle = tokio::runtime::Handle::current();
  let log_fields = serde_json::Map::from_iter([
    ("run_id".to_string(), Value::String(run_id.clone())),
    ("session".to_string(), Value::String(owner_session_id)),
  ]);
  let error = on_blocking(move || {
    Ok(handle.block_on(async move {
      charliebot_tasks::git::git_worktree_remove_reporting(
        Some(&repo_path),
        Some(&worktree_path),
        Some(&branch_name),
        &run_id,
        &worktree_root,
        &log_fields,
        "Disk sweep worktree",
        "disk_sweep_remove_failed",
        "disk_sweep_remove_failed",
      )
      .await
    }))
  })
  .await?;
  if let Some(error) = error {
    bail!("{error}");
  }
  Ok(())
}

fn mark_skipped(row: &mut SweepRow, reason: &str) {
  row.action = SweepAction::Skip;
  row.reason = reason.to_string();
  row.freed_bytes = 0;
}

fn record_apply_failure(report: &mut SweepReport, index: usize, mut row: SweepRow, reason: String) {
  mark_skipped(&mut row, "apply-failed");
  report.failures.push(SweepFailure { path: row.path.clone(), reason });
  report.rows[index] = row;
}

async fn on_blocking<T, F>(operation: F) -> Result<T>
where
  T: Send + 'static,
  F: FnOnce() -> Result<T> + Send + 'static,
{
  tokio::task::spawn_blocking(operation).await.context("disk sweep blocking task panicked")?
}

fn absolute(path: &Path) -> Result<PathBuf> {
  let path = if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir()?.join(path) };
  let mut normalized = PathBuf::new();
  for component in path.components() {
    match component {
      Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
      Component::RootDir => normalized.push(component.as_os_str()),
      Component::CurDir => {}
      Component::ParentDir => {
        normalized.pop();
      }
      Component::Normal(part) => normalized.push(part),
    }
  }
  Ok(normalized)
}

#[cfg(test)]
mod tests;
