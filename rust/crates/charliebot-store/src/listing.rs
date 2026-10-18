use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use serde_json::Value;

use crate::metadata::{SessionMetadata, SessionStatus, UtcDateTime};
use crate::{Store, StoreError, StoreResult};

/// One read-only view of session metadata and the sequence fields derived for its rows.
#[derive(Clone, Debug)]
pub struct MetadataListing {
  /// Session rows in newest-first order.
  pub rows: Vec<SessionMetadata>,
  /// Sequence-owned fields keyed by session id.
  pub sequence_fields: std::collections::HashMap<String, serde_json::Map<String, Value>>,
}

/// One unvalidated metadata file from the raw read-only corpus scan.
#[derive(Clone, Debug)]
pub struct RawMetadata {
  /// Session directory name.
  pub session_id: String,
  /// Parsed JSON object as stored.
  pub value: Value,
}

/// One archived-session listing page.
#[derive(Clone, Debug)]
pub struct ArchivedPage {
  /// Archived session rows in descending `(updated_at, id)` order.
  pub sessions: Vec<SessionMetadata>,
  /// Whether another page follows this one.
  pub has_more: bool,
  /// The next page's timestamp cursor, in Python `datetime.isoformat()` form.
  pub next_before: Option<String>,
  /// The next page's id cursor.
  pub next_before_id: Option<String>,
  /// Group counts over the entire archived listing set.
  pub groups: Vec<(Option<String>, usize)>,
}

impl Store {
  /// Read all metadata rows, optionally restricted to one stored status.
  pub fn load_session_metas(&self, status: Option<SessionStatus>) -> StoreResult<Vec<SessionMetadata>> {
    let root = self.cfg.sessions_dir();
    let entries = match fs::read_dir(&root) {
      Ok(entries) => entries,
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
      Err(error) => return Err(StoreError::new(format!("{}: {error}", root.display()))),
    };
    let cached = self.fresh_cached_metas()?;
    let mut rows = Vec::new();
    for entry in entries {
      let entry = entry.map_err(|error| StoreError::new(format!("{}: {error}", root.display())))?;
      if !entry.file_type().map_err(|error| StoreError::new(format!("{}: {error}", entry.path().display())))?.is_dir() {
        continue;
      }
      let session_id = entry.file_name().to_string_lossy().into_owned();
      let metadata_path = entry.path().join("metadata.json");
      let meta = match cached.get(&session_id) {
        Some(meta) => Some(meta.clone()),
        None => {
          let signature = stat_signature(&metadata_path);
          let Some(meta) = self.read_metadata_path(&metadata_path)? else { continue };
          self
            .metadata_cache
            .lock()
            .expect("metadata cache mutex is not poisoned")
            .entry(session_id.clone())
            .or_insert_with(|| super::store::CachedMetadata {
              value: meta.clone(),
              at: std::time::Instant::now(),
              signature,
            });
          Some(meta)
        }
      };
      let Some(meta) = meta else { continue };
      if status.is_none_or(|status| meta.status == status) {
        rows.push(meta);
      }
    }
    Ok(rows)
  }

  /// List sessions with optional stored-status, star, and sequence ownership filters.
  pub fn list_sessions(
    &self,
    status: Option<SessionStatus>,
    starred: Option<bool>,
    scheduled: Option<bool>,
  ) -> StoreResult<MetadataListing> {
    let mut rows = self.apply_derived_archive(self.load_session_metas(status)?, status)?;
    rows.retain(|meta| {
      starred.is_none_or(|value| meta.starred == value)
        && scheduled.is_none_or(|value| self.sequence_listing.owns_session(meta) == value)
    });
    rows.sort_by_key(|row| std::cmp::Reverse(row.updated_at));
    let ids: Vec<String> = rows.iter().map(|meta| meta.id.clone()).collect();
    let sequence_fields = self.sequence_listing.listing_fields(&ids, UtcDateTime::now())?;
    Ok(MetadataListing { rows, sequence_fields })
  }

  /// Read rows of one stored status without projecting sequence fields.
  pub fn list_sessions_readonly(&self, status: SessionStatus) -> StoreResult<Vec<SessionMetadata>> {
    let mut rows = self.apply_derived_archive(self.load_session_metas(Some(status))?, Some(status))?;
    rows.sort_by_key(|row| std::cmp::Reverse(row.updated_at));
    Ok(rows)
  }

  /// Return sorted, distinct, nonempty group names.
  pub fn list_group_names(&self) -> StoreResult<Vec<String>> {
    let mut groups: Vec<String> = self
      .load_session_metas(None)?
      .into_iter()
      .filter_map(|meta| meta.group.filter(|group| !group.is_empty()))
      .collect();
    groups.sort();
    groups.dedup();
    Ok(groups)
  }

  /// Return the sequence subtree root map over all stored metadata.
  pub fn sequence_subtree_roots(&self) -> StoreResult<std::collections::HashMap<String, String>> {
    self.sequence_listing.sequence_subtree_roots(&self.load_session_metas(None)?)
  }

  /// Return the view subtree roots over all stored metadata.
  pub fn view_subtree_roots(
    &self,
  ) -> StoreResult<std::collections::HashMap<String, std::collections::HashMap<String, String>>> {
    self.sequence_listing.view_subtree_roots(&self.load_session_metas(None)?)
  }

  /// Scan every `sessions/*/metadata.json` file as raw JSON, including archived sessions.
  pub fn raw_metadata_scan(&self) -> StoreResult<Vec<RawMetadata>> {
    let root = self.cfg.sessions_dir();
    let entries = match fs::read_dir(&root) {
      Ok(entries) => entries,
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
      Err(error) => return Err(StoreError::new(format!("{}: {error}", root.display()))),
    };
    let mut rows = Vec::new();
    for entry in entries {
      let entry = entry.map_err(|error| StoreError::new(format!("{}: {error}", root.display())))?;
      if !entry.file_type().map_err(|error| StoreError::new(format!("{}: {error}", entry.path().display())))?.is_dir() {
        continue;
      }
      let session_id = entry.file_name().to_string_lossy().into_owned();
      let path: PathBuf = entry.path().join("metadata.json");
      let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
        Err(error) => return Err(StoreError::new(format!("{}: {error}", path.display()))),
      };
      if raw.trim().is_empty() {
        continue;
      }
      let value =
        serde_json::from_str(&raw).map_err(|error| StoreError::new(format!("{}: {error}", path.display())))?;
      rows.push(RawMetadata { session_id, value });
    }
    Ok(rows)
  }

  /// Return one archived-session page and group counts over the complete archived set.
  pub fn list_archived_page(
    &self,
    group: Option<&str>,
    limit: usize,
    before: Option<&str>,
    before_id: Option<&str>,
  ) -> StoreResult<ArchivedPage> {
    if before.is_some() != before_id.is_some() {
      return Err(StoreError::new("before and before_id form one cursor: pass both or neither"));
    }
    let limit = limit.clamp(1, 500);
    let mut metas = self
      .apply_derived_archive(self.load_session_metas(Some(SessionStatus::Archived))?, Some(SessionStatus::Archived))?;
    let subtree: HashSet<String> = self.sequence_subtree_roots()?.into_keys().collect();
    metas.retain(|meta| !subtree.contains(&meta.id));

    let mut group_counts: HashMap<Option<String>, usize> = HashMap::new();
    for meta in &metas {
      *group_counts.entry(meta.group.clone().filter(|value| !value.is_empty())).or_default() += 1;
    }
    let mut groups: Vec<(Option<String>, usize)> =
      group_counts.iter().filter_map(|(name, count)| name.clone().map(|name| (Some(name), *count))).collect();
    groups.sort_by(|left, right| left.0.cmp(&right.0));
    if let Some(count) = group_counts.get(&None) {
      groups.push((None, *count));
    }

    let mut rows: Vec<SessionMetadata> = metas
      .into_iter()
      .filter(|meta| match group {
        None => true,
        Some("") => meta.group.as_deref().is_none_or(str::is_empty),
        Some(name) => meta.group.as_deref() == Some(name),
      })
      .collect();
    rows.sort_by(|left, right| right.updated_at.cmp(&left.updated_at).then_with(|| right.id.cmp(&left.id)));
    if let Some(before) = before {
      let cursor_time = UtcDateTime::parse(before)
        .map_err(|error| StoreError::new(format!("before is not a valid ISO timestamp: {error}")))?;
      if !has_timezone(before) {
        return Err(StoreError::new("before must be a timezone-aware ISO timestamp"));
      }
      let before_id = before_id.expect("cursor pair validation requires an id");
      rows.retain(|meta| {
        meta.updated_at < cursor_time || (meta.updated_at == cursor_time && meta.id.as_str() < before_id)
      });
    }
    let has_more = rows.len() > limit;
    rows.truncate(limit);
    let next = if has_more { rows.last() } else { None };
    Ok(ArchivedPage {
      next_before: next.map(|meta| meta.updated_at.to_python_isoformat()),
      next_before_id: next.map(|meta| meta.id.clone()),
      sessions: rows,
      has_more,
      groups,
    })
  }

  /// Collect unarchived task ancestors needed to render an archived page as a tree.
  pub async fn archived_context_rows(&self, rows: &[SessionMetadata]) -> StoreResult<Vec<SessionMetadata>> {
    let present: HashSet<&str> = rows.iter().map(|row| row.id.as_str()).collect();
    let derived: HashSet<String> = self.sequence_listing.derived_archived_ids()?.into_iter().collect();
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for row in rows {
      let mut parent_id = row.task_parent_id.clone();
      while let Some(id) = parent_id {
        if present.contains(id.as_str()) || !seen.insert(id.clone()) {
          break;
        }
        let Some(ancestor) = self.get_session(&id).await? else { break };
        if ancestor.status != SessionStatus::Archived && !derived.contains(&ancestor.id) {
          out.push(ancestor.clone());
        }
        parent_id = ancestor.task_parent_id.clone();
      }
    }
    Ok(out)
  }

  fn apply_derived_archive(
    &self,
    mut metas: Vec<SessionMetadata>,
    status: Option<SessionStatus>,
  ) -> StoreResult<Vec<SessionMetadata>> {
    let derived: HashSet<String> = self.sequence_listing.derived_archived_ids()?.into_iter().collect();
    if derived.is_empty() {
      return Ok(metas);
    }
    if status == Some(SessionStatus::Active) {
      metas.retain(|meta| !derived.contains(&meta.id));
      return Ok(metas);
    }
    if status == Some(SessionStatus::Archived) {
      for mut meta in self.load_session_metas(Some(SessionStatus::Active))? {
        if derived.contains(&meta.id) {
          meta.status = SessionStatus::Archived;
          metas.push(meta);
        }
      }
      return Ok(metas);
    }
    for meta in &mut metas {
      if derived.contains(&meta.id) {
        meta.status = SessionStatus::Archived;
      }
    }
    Ok(metas)
  }
}

fn stat_signature(path: &std::path::Path) -> Option<(i64, i64)> {
  use std::os::unix::fs::MetadataExt;
  let metadata = fs::metadata(path).ok()?;
  Some((metadata.mtime_nsec() + metadata.mtime() * 1_000_000_000, metadata.size() as i64))
}

fn has_timezone(value: &str) -> bool {
  let time = value.split_once('T').map_or("", |(_, time)| time);
  time.ends_with('Z') || time.contains('+') || time.rfind('-').is_some()
}
