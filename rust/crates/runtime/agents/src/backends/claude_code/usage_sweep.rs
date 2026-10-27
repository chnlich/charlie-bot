//! Claude Code's part of the cold-storage sweep (`src/backends/claude_code/usage_sweep.py`): the transcript directories.
//!
//! A transcript directory belongs to CharlieBot when the cwd encoding the CLI applies (separators, dots and underscores
//! become hyphens) leaves a session id in its name. Any other name encodes a cwd that CharlieBot never handed a claude
//! process, and the sweep never touches it.

use std::collections::{BTreeSet, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::Context;
use charliebot_core::config::CharlieBotConfig;
use charliebot_registry::usage::{CategoryResult, SourceSweep, SweepScope};
use regex::Regex;
use walkdir::WalkDir;

use super::claude_config::AccountsConfig;
use super::login_dirs::ClaudeHost;

/// The on-disk name of the worktree quarantine directory (`src/runtime/worktree_trash.py:18`). The live-worktree list
/// skips it.
pub const TRASH_DIR_NAME: &str = ".trash";

const CATEGORY: &str = "claude-transcripts";
const UNIT: &str = "dirs";

/// Every cwd CharlieBot hands a claude process is the session directory or one inside it, so the encoded name carries
/// the session id after the encoded `sessions` path segment.
const SESSIONS_SEGMENT: &str = "-sessions-";

/// A canonical UUID at the start of a string. The pattern is fixed-width, so the match's end is the id's end.
static SESSION_ID_PREFIX: LazyLock<Regex> = LazyLock::new(|| {
  Regex::new(r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}")
    .expect("the session id pattern is valid")
});

/// The transcript directory name Claude Code derives from a process cwd: separators, dots and underscores each become
/// a hyphen, and every other character passes through.
pub fn claude_project_dir_name(cwd: &Path) -> String {
  let normalized: PathBuf = cwd.components().collect();
  normalized.to_string_lossy().replace(['/', '.', '_'], "-")
}

/// Every `projects` tree a cc-claude backend may have written transcripts into, sorted. The default home and the
/// `CLAUDE_CONFIG_DIR` home are searched beside the configured pool accounts.
pub fn claude_projects_roots(host: &ClaudeHost, cfg: &CharlieBotConfig) -> Vec<PathBuf> {
  let mut homes = BTreeSet::from([host.default_dir(), host.claude_config_dir.clone()]);
  for account in &cfg.section::<AccountsConfig>("accounts").claude {
    homes.insert(host.expand(&account.config_dir));
  }
  homes.into_iter().map(|home| home.join("projects")).collect()
}

/// The CharlieBot session id a transcript directory name encodes, or `None`. The tail after the last `-sessions-`
/// starts with a full session id, and that id ends the name or precedes another path segment.
fn encoded_session_id(dir_name: &str) -> Option<String> {
  let start = dir_name.rfind(SESSIONS_SEGMENT)?;
  let tail = &dir_name[start + SESSIONS_SEGMENT.len()..];
  let session = SESSION_ID_PREFIX.find(tail)?.as_str();
  let rest = &tail[session.len()..];
  if !rest.is_empty() && !rest.starts_with('-') {
    return None;
  }
  Some(session.to_string())
}

/// The encoded cwd names of the worktrees on disk. Their runs may still write, so the sweep keeps them.
fn live_worktree_dir_names(cfg: &CharlieBotConfig) -> anyhow::Result<HashSet<String>> {
  let worktree_dir = PathBuf::from(&cfg.paths.worktree_dir);
  if !worktree_dir.is_dir() {
    return Ok(HashSet::new());
  }
  let mut names = HashSet::new();
  for child in fs::read_dir(&worktree_dir).with_context(|| format!("listing {}", worktree_dir.display()))? {
    let child = child.with_context(|| format!("listing {}", worktree_dir.display()))?.path();
    if child.is_dir() && child.file_name() != Some(OsStr::new(TRASH_DIR_NAME)) {
      names.insert(claude_project_dir_name(&child));
    }
  }
  Ok(names)
}

/// The newest file mtime under `root`, or the directory's own when it holds no file (`_newest_mtime`). A symlinked
/// directory is not entered, as `os.walk` does not enter it. A listing or stat failure skips its entry.
fn newest_mtime(root: &Path) -> Option<f64> {
  let mut newest: Option<f64> = None;
  for entry in WalkDir::new(root).follow_links(false) {
    let entry = match entry {
      Ok(entry) => entry,
      Err(error) => {
        tracing::debug!(target: "charliebot_agents", path = %error_path(&error), error = %error, "storage_cool_mtime_skip");
        continue;
      }
    };
    if entry.depth() == 0 || entry.path().is_dir() {
      continue;
    }
    match fs::metadata(entry.path()) {
      Ok(metadata) => {
        let mtime = mtime_secs(&metadata);
        newest = Some(newest.map_or(mtime, |best| best.max(mtime)));
      }
      Err(error) => {
        tracing::debug!(target: "charliebot_agents", path = %entry.path().display(), error = %error, "storage_cool_mtime_skip");
      }
    }
  }
  newest.or_else(|| fs::metadata(root).ok().map(|metadata| mtime_secs(&metadata)))
}

/// The path a walk error names, for the skip log line.
fn error_path(error: &walkdir::Error) -> String {
  error.path().map(|path| path.display().to_string()).unwrap_or_default()
}

/// The newest-mtime form of the idle judgment for a transcript directory. A probe that finds no time is logged and
/// reads as not idle.
fn idle_past_window(path: &Path, scope: &SweepScope) -> bool {
  match newest_mtime(path) {
    Some(newest) => scope.idle_past(newest),
    None => {
      tracing::warn!(target: "charliebot_agents", path = %path.display(), "storage_cool_idle_probe_failed");
      false
    }
  }
}

/// The files under a transcript directory, sorted. Like `rglob`, the walk does not enter a symlinked directory.
fn project_files(root: &Path) -> Vec<PathBuf> {
  let mut files = Vec::new();
  for entry in WalkDir::new(root).follow_links(false) {
    let entry = match entry {
      Ok(entry) => entry,
      Err(error) => {
        tracing::warn!(target: "charliebot_agents", path = %error_path(&error), error = %error, "storage_cool_dir_scan_failed");
        continue;
      }
    };
    if entry.depth() == 0 || entry.path().is_dir() {
      continue;
    }
    if entry.path().is_file() {
      files.push(entry.into_path());
    }
  }
  files.sort();
  files
}

/// Each root's sorted entries. A missing root is skipped, and an unreadable listing is logged and skipped
/// (`sweep_root_entries`).
fn root_entries(roots: &[PathBuf]) -> Vec<PathBuf> {
  let mut entries = Vec::new();
  for root in roots.iter().filter(|root| root.is_dir()) {
    match sorted_children(root) {
      Ok(children) => entries.extend(children),
      Err(error) => {
        tracing::warn!(target: "charliebot_agents", dir = %root.display(), error = %error, "storage_cool_dir_scan_failed");
      }
    }
  }
  entries
}

fn sorted_children(root: &Path) -> io::Result<Vec<PathBuf>> {
  let mut children =
    fs::read_dir(root)?.map(|entry| entry.map(|entry| entry.path())).collect::<io::Result<Vec<_>>>()?;
  children.sort();
  Ok(children)
}

/// The count of directories and the freed bytes of the category. A transcript directory's files add bytes, and the
/// directory adds one count (`SweepCounter` with `count=False` for each file).
#[derive(Default)]
struct Counter {
  count: u64,
  bytes: u64,
}

impl Counter {
  /// Deletes one file, or only counts it in a dry run, and adds its bytes. A file that cannot be statted or removed is
  /// logged and skipped.
  fn delete_file(&mut self, path: &Path, dry_run: bool) {
    let size = match fs::metadata(path) {
      Ok(metadata) => metadata.len(),
      Err(error) => {
        tracing::warn!(target: "charliebot_agents", path = %path.display(), error = %error, "storage_cool_file_stat_failed");
        return;
      }
    };
    if !dry_run && let Err(error) = fs::remove_file(path) {
      tracing::warn!(target: "charliebot_agents", path = %path.display(), error = %error, "storage_cool_file_delete_failed");
      return;
    }
    self.bytes += size;
  }

  /// Deletes a transcript directory file by file, then its skeleton, and counts it once. A dry run only counts it.
  fn delete_project_dir(&mut self, path: &Path, dry_run: bool) {
    for file in project_files(path) {
      self.delete_file(&file, dry_run);
    }
    if dry_run {
      self.count += 1;
      return;
    }
    match fs::remove_dir_all(path) {
      Ok(()) => self.count += 1,
      Err(error) => {
        tracing::warn!(target: "charliebot_agents", path = %path.display(), error = %error, "storage_cool_dir_delete_failed");
      }
    }
  }
}

fn mtime_secs(metadata: &fs::Metadata) -> f64 {
  metadata.mtime() as f64 + metadata.mtime_nsec() as f64 / 1e9
}

/// Deletes the cold sessions' transcript directories and the unreferenced orphan directories.
///
/// A transcript directory whose session is known is deleted once that session is cold. One whose session metadata is
/// gone is an orphan, deleted once idle past the window; a session directory that still exists keeps its transcripts.
/// A worktree-encoded directory follows the orphan rule once its worktree is gone. A scoped run touches only the
/// scoped session's directories.
fn sweep_transcripts(host: &ClaudeHost, scope: &SweepScope, counter: &mut Counter) -> anyhow::Result<()> {
  let cfg = scope.cfg.as_ref();
  let worktree_prefix = format!("{}-", claude_project_dir_name(Path::new(&cfg.paths.worktree_dir)));
  let live_worktrees = if scope.session_id.is_none() { live_worktree_dir_names(cfg)? } else { HashSet::new() };
  for entry in root_entries(&claude_projects_roots(host, cfg)) {
    if !entry.is_dir() {
      continue;
    }
    let name = entry.file_name().unwrap_or_default().to_string_lossy().into_owned();
    match encoded_session_id(&name) {
      Some(encoded) => match scope.facts.get(&encoded) {
        // The session still exists: only the cold rule decides.
        Some(owner) => {
          if owner.cold {
            counter.delete_project_dir(&entry, scope.dry_run);
          }
        }
        // An unreadable session metadata file is not proof that the session was deleted. Only a missing session
        // directory makes this an orphan.
        None if cfg.sessions_dir().join(&encoded).is_dir() => {}
        // No metadata references the session any more: an orphan past the window.
        None if scope.session_id.is_none() && idle_past_window(&entry, scope) => {
          counter.delete_project_dir(&entry, scope.dry_run);
        }
        None => {}
      },
      // A worktree's transcripts are orphans once the worktree is gone.
      None
        if scope.session_id.is_none()
          && name.starts_with(&worktree_prefix)
          && !live_worktrees.contains(&name)
          && idle_past_window(&entry, scope) =>
      {
        counter.delete_project_dir(&entry, scope.dry_run);
      }
      None => {}
    }
  }
  Ok(())
}

/// Claude Code's part of the sweep: one `claude-transcripts` category, counted in directories.
pub fn sweep(host: &ClaudeHost, scope: &SweepScope) -> anyhow::Result<SourceSweep> {
  let mut counter = Counter::default();
  sweep_transcripts(host, scope, &mut counter)?;
  Ok(SourceSweep {
    categories: vec![CategoryResult { name: CATEGORY, unit: UNIT, count: counter.count, bytes: counter.bytes }],
    freelist: None,
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn project_dir_names_replace_separators_dots_and_underscores() {
    assert_eq!(
      claude_project_dir_name(Path::new("/home/dev/.cb_home/sessions/abc")),
      "-home-dev--cb-home-sessions-abc"
    );
    assert_eq!(claude_project_dir_name(Path::new("/srv/worktrees/")), "-srv-worktrees");
  }

  #[test]
  fn encoded_session_ids_need_a_whole_id_and_a_segment_boundary() {
    let id = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
    assert_eq!(encoded_session_id(&format!("-home-dev-sessions-{id}")), Some(id.to_string()));
    assert_eq!(encoded_session_id(&format!("-home-dev-sessions-{id}-threads-x")), Some(id.to_string()));
    assert_eq!(encoded_session_id(&format!("-home-dev-sessions-{id}x")), None);
    assert_eq!(encoded_session_id("-home-dev-workspace-charlie-bot"), None);
    assert_eq!(encoded_session_id(&format!("-sessions-{}", &id[..20])), None);
  }

  use std::collections::{BTreeMap, HashMap};
  use std::sync::Arc;
  use std::time::{Duration, SystemTime};

  use charliebot_registry::usage::SessionFacts;
  use charliebot_store::metadata::UtcDateTime;

  use crate::backends::claude_code::test_support;

  const SID_COLD: &str = "11111111-2222-4333-8444-555555555555";
  const SID_LIVE: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
  const TID: &str = "f0e1d2c3-a4b5-4c6d-8e7f-0a1b2c3d4e5f";
  const TRANSCRIPT: &[u8] = b"claude-transcript";

  /// A host whose Claude config dir sits under `root`, and a config whose sessions and worktrees sit there too.
  fn host_and_config(root: &Path) -> (ClaudeHost, Arc<CharlieBotConfig>) {
    let host = ClaudeHost { home: root.to_path_buf(), claude_config_dir: root.join("claude-home") };
    let cfg = test_support::load_config(&root.join("home"), &[], &root.join("worktrees"));
    (host, Arc::new(cfg))
  }

  fn encoded_session_dir(cfg: &CharlieBotConfig, sid: &str) -> String {
    claude_project_dir_name(&cfg.sessions_dir().join(sid))
  }

  /// One transcript directory holding one transcript file.
  fn project_dir(projects: &Path, name: &str) -> PathBuf {
    let dir = projects.join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("transcript.jsonl"), TRANSCRIPT).unwrap();
    dir
  }

  fn age(path: &Path, age: Duration) {
    let stamp = SystemTime::now() - age;
    fs::OpenOptions::new().write(true).open(path).unwrap().set_modified(stamp).unwrap();
  }

  fn sweep_scope(cfg: &Arc<CharlieBotConfig>, facts: &[(&str, bool)], dry_run: bool) -> SweepScope {
    let facts = facts
      .iter()
      .map(|(id, cold)| (id.to_string(), SessionFacts { id: id.to_string(), cold: *cold, cc_session_id: None }))
      .collect::<HashMap<_, _>>();
    SweepScope {
      cfg: Arc::clone(cfg),
      now: UtcDateTime::now(),
      dry_run,
      session_id: None,
      facts,
      references: HashMap::new(),
      orphan_idle_days: 14,
      vacuum: false,
      force: false,
    }
  }

  fn tree_bytes(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    project_files(root)
      .into_iter()
      .map(|file| (file.strip_prefix(root).unwrap().to_path_buf(), fs::read(&file).unwrap()))
      .collect()
  }

  #[test]
  fn claude_dirs_delete_for_cold_sessions_and_keep_live_ones() {
    let root = tempfile::tempdir().unwrap();
    let (host, cfg) = host_and_config(root.path());
    let projects = host.claude_config_dir.join("projects");
    let cold_name = encoded_session_dir(&cfg, SID_COLD);
    let cold = project_dir(&projects, &cold_name);
    let cold_thread = project_dir(&projects, &format!("{cold_name}-threads-{TID}"));
    let live = project_dir(&projects, &encoded_session_dir(&cfg, SID_LIVE));

    let result = sweep(&host, &sweep_scope(&cfg, &[(SID_COLD, true), (SID_LIVE, false)], false)).unwrap();

    assert!(!cold.exists());
    assert!(!cold_thread.exists());
    assert!(live.exists());
    let category = &result.categories[0];
    assert_eq!((category.count, category.bytes), (2, 2 * TRANSCRIPT.len() as u64));
  }

  #[test]
  fn claude_user_cwd_dirs_are_never_touched() {
    let root = tempfile::tempdir().unwrap();
    let (host, cfg) = host_and_config(root.path());
    let projects = host.claude_config_dir.join("projects");
    // Idle past the orphan window, so a rule that matched them would delete them.
    let user_dirs: Vec<PathBuf> = ["-home-dev", "-home-dev-workspace-charlie-bot", "-tmp-cb-e2e-home-sessions"]
      .iter()
      .map(|name| {
        let dir = project_dir(&projects, name);
        age(&dir.join("transcript.jsonl"), Duration::from_secs(30 * 86400));
        dir
      })
      .collect();

    let result = sweep(&host, &sweep_scope(&cfg, &[], false)).unwrap();

    for dir in &user_dirs {
      assert!(dir.exists(), "{} was deleted", dir.display());
    }
    assert_eq!(result.categories[0].count, 0);
  }

  #[test]
  fn a_dry_run_leaves_every_byte_untouched_and_counts_what_the_real_run_deletes() {
    let root = tempfile::tempdir().unwrap();
    let (host, cfg) = host_and_config(root.path());
    let projects = host.claude_config_dir.join("projects");
    project_dir(&projects, &encoded_session_dir(&cfg, SID_COLD));
    project_dir(&projects, &encoded_session_dir(&cfg, SID_LIVE));
    project_dir(&projects, "-home-dev");
    let facts = [(SID_COLD, true), (SID_LIVE, false)];
    let before = tree_bytes(root.path());

    let dry = sweep(&host, &sweep_scope(&cfg, &facts, true)).unwrap();

    assert_eq!(tree_bytes(root.path()), before);
    let real = sweep(&host, &sweep_scope(&cfg, &facts, false)).unwrap();
    let (dry, real) = (&dry.categories[0], &real.categories[0]);
    assert_eq!((dry.count, dry.bytes), (real.count, real.bytes));
    assert_eq!(real.count, 1);
  }
}
