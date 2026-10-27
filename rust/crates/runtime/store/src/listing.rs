use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::metadata::{SessionMetadata, SessionStatus, UtcDateTime};
use crate::store::{CachedListing, DirNames};
use crate::{Store, StoreError, StoreResult};

/// One read-only view of session metadata and the sequence fields derived for its rows.
#[derive(Clone, Debug)]
pub struct MetadataListing {
  /// Session rows in newest-first order. Each row is shared with the memoized corpus: a listing hit
  /// clones pointers, not metadata, the way Python's `load_session_metas` hands out its cached rows.
  pub rows: Vec<Arc<SessionMetadata>>,
  /// Sequence-owned fields keyed by session id.
  pub sequence_fields: std::collections::HashMap<String, serde_json::Map<String, Value>>,
}

/// Shared sequence and view membership maps for one cached metadata-list identity.
#[derive(Clone)]
pub struct SharedSubtreeRoots {
  /// Sessions that belong to sequence subtrees, mapped to the owning session.
  pub sequence: Arc<HashMap<String, String>>,
  /// View subtree membership, mapped by view name and session id to the view root.
  pub views: Arc<HashMap<String, HashMap<String, String>>>,
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

/// How long a memoized listing answers before the next call walks the sessions again (`_LISTINGS_SWEEP_INTERVAL`).
const LISTINGS_SWEEP_INTERVAL: Duration = Duration::from_secs(10);

impl Store {
  /// Read all metadata rows, optionally restricted to one stored status.
  ///
  /// As Python's `load_session_metas`, each filter's result is memoized on the listings revision, the sessions root's
  /// signature and a [`LISTINGS_SWEEP_INTERVAL`] clock. A metadata write through this store moves the revision, and a
  /// created or removed session directory moves the root's signature, so both walk again on the next call. The sweep
  /// bounds an out-of-band edit to the cache entry's expiry plus one interval.
  pub fn load_session_metas(&self, status: Option<SessionStatus>) -> StoreResult<Vec<SessionMetadata>> {
    Ok(self.load_session_metas_versioned(status)?.0.iter().map(|meta| meta.as_ref().clone()).collect())
  }

  /// Read metadata rows with the identity of the memoized corpus that supplied them.
  pub fn load_session_metas_versioned(
    &self,
    status: Option<SessionStatus>,
  ) -> StoreResult<(Arc<Vec<Arc<SessionMetadata>>>, u64)> {
    self.load_session_metas_snapshot_versioned(status)
  }

  /// Read the immutable metadata corpus and its generation without cloning its rows.
  ///
  /// Search holds the same listing snapshot across its query memo and borrows only the matching rows, as Python's
  /// `load_session_metas` does while its list identity remains current (`src/runtime/session_search.py:266-295`).
  pub fn load_session_metas_snapshot_versioned(
    &self,
    status: Option<SessionStatus>,
  ) -> StoreResult<(Arc<Vec<Arc<SessionMetadata>>>, u64)> {
    let root = self.cfg.sessions_dir();
    // The signature is read before the walk, so a directory created during the walk moves it past the memo's.
    let root_signature = stat_signature(&root);
    if let Some(signature) = root_signature
      && let Some(hit) = self.listings_memo.lock().expect("listings memo mutex is not poisoned").get(&status)
      && hit.revision == self.listings_revision.load(Ordering::Acquire)
      && hit.at.elapsed() < LISTINGS_SWEEP_INTERVAL
      && hit.root == signature
    {
      return Ok((Arc::clone(&hit.rows), hit.generation));
    }
    let revision = self.listings_revision.load(Ordering::Acquire);
    let names = match self.session_dir_names(&root, root_signature)? {
      Some(names) => names,
      // No readable root: the walk has nothing to list.
      None => return Ok((Arc::new(Vec::new()), 0)),
    };
    let cached = self.fresh_cached_metas()?;
    let mut rows = Vec::new();
    for session_id in names.iter() {
      let metadata_path = root.join(session_id).join("metadata.json");
      let meta = match cached.get(session_id) {
        Some(meta) => Some(Arc::clone(meta)),
        None => {
          let signature = stat_signature(&metadata_path);
          match self.read_metadata_path(&metadata_path) {
            Ok(Some(meta)) => {
              let meta = Arc::new(meta);
              self
                .metadata_cache
                .lock()
                .expect("metadata cache mutex is not poisoned")
                .entry(session_id.clone())
                .or_insert_with(|| super::store::CachedMetadata {
                  value: Arc::clone(&meta),
                  at: std::time::Instant::now(),
                  signature,
                  identity: std::sync::Arc::new(()),
                });
              Some(meta)
            }
            // A directory without metadata.json drops out silently, as `load_session_metas` skips a missing file.
            Ok(None) if !metadata_path.exists() => continue,
            Ok(None) => {
              tracing::warn!(path = %metadata_path.display(), "session_metadata_empty");
              continue;
            }
            // A malformed file logs at warning level and drops out of the listing, the way
            // `session_store.load_session_metas` reports `session_load_failed` and skips.
            Err(error) => {
              tracing::warn!(session_id = %session_id, error = %error, "session_load_failed");
              continue;
            }
          }
        }
      };
      let Some(meta) = meta else { continue };
      if status.is_none_or(|status| meta.status == status) {
        rows.push(Arc::clone(&meta));
      }
    }
    let generation = self.next_listing_generation.fetch_add(1, Ordering::AcqRel);
    let rows: Arc<Vec<Arc<SessionMetadata>>> = Arc::new(rows);
    if let Some(root) = root_signature {
      let listing = CachedListing { revision, generation, at: Instant::now(), root, rows: Arc::clone(&rows) };
      self.listings_memo.lock().expect("listings memo mutex is not poisoned").insert(status, listing);
    }
    Ok((rows, generation))
  }

  /// The sessions directory's entry names, from the memo when the root signature still matches.
  ///
  /// The key is the sessions root's `(mtime_ns, size)` and nothing else, as `_session_dir_names` keys
  /// `session_store.py`'s `_dir_names_memo`: a create or delete moves the signature, so an unchanged signature proves
  /// the name list unchanged and no time bound is needed. A hit costs one stat instead of a scandir plus one
  /// `file_type` per entry, which the production copy pays about 6,000 times per listing walk.
  fn session_dir_names(
    &self,
    root: &std::path::Path,
    signature: Option<(i64, i64)>,
  ) -> StoreResult<Option<Arc<Vec<String>>>> {
    if let Some(signature) = signature
      && let Some(memo) = self.dir_names_memo.lock().expect("dir names memo mutex is not poisoned").as_ref()
      && memo.signature == signature
    {
      return Ok(Some(Arc::clone(&memo.names)));
    }
    let entries = match fs::read_dir(root) {
      Ok(entries) => entries,
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
      Err(error) => return Err(StoreError::new(format!("{}: {error}", root.display()))),
    };
    #[cfg(test)]
    self.directory_scans.fetch_add(1, Ordering::Relaxed);
    let mut names = Vec::new();
    for entry in entries {
      let entry = entry.map_err(|error| StoreError::new(format!("{}: {error}", root.display())))?;
      if !entry.file_type().map_err(|error| StoreError::new(format!("{}: {error}", entry.path().display())))?.is_dir() {
        continue;
      }
      names.push(entry.file_name().to_string_lossy().into_owned());
    }
    let names = Arc::new(names);
    if let Some(signature) = signature {
      *self.dir_names_memo.lock().expect("dir names memo mutex is not poisoned") =
        Some(DirNames { signature, names: Arc::clone(&names) });
    }
    Ok(Some(names))
  }

  /// List sessions with optional stored-status, star, and sequence ownership filters.
  pub fn list_sessions(
    &self,
    status: Option<SessionStatus>,
    starred: Option<bool>,
    scheduled: Option<bool>,
  ) -> StoreResult<MetadataListing> {
    let (snapshot, _) = self.load_session_metas_snapshot_versioned(status)?;
    let mut rows = self.apply_derived_archive_shared(snapshot.as_slice(), status)?;
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
    let roots = self.subtree_roots_shared()?;
    Ok((*roots.sequence).clone())
  }

  /// Return the view subtree roots over all stored metadata.
  pub fn view_subtree_roots(
    &self,
  ) -> StoreResult<std::collections::HashMap<String, std::collections::HashMap<String, String>>> {
    let roots = self.subtree_roots_shared()?;
    Ok((*roots.views).clone())
  }

  /// Return both cached subtree maps for the cached metadata-list identity.
  ///
  /// This follows Python's `session_listing.py:139-164` memo key: reuse both maps while the cached metadata list
  /// remains the same, and rebuild after a metadata cache identity or directory membership changes.
  pub fn subtree_roots_shared(&self) -> StoreResult<SharedSubtreeRoots> {
    let rows = self.load_session_metas(None)?;
    // A concurrent read prunes an entry that outlived the cache TTL and a write invalidates one, so a row of `rows`
    // can leave the metadata cache before this lookup. A missing entry proves nothing about the cached maps: the
    // call then rebuilds them and leaves the cache alone, which the next call replaces with a matching identity.
    let metadata_identity = {
      let metadata_cache = self.metadata_cache.lock().expect("metadata cache mutex is not poisoned");
      let mut metadata_identity = rows
        .iter()
        .map(|row| metadata_cache.get(&row.id).map(|entry| (row.id.clone(), Arc::clone(&entry.identity))))
        .collect::<Option<Vec<_>>>();
      if let Some(metadata_identity) = metadata_identity.as_mut() {
        metadata_identity.sort_by(|left, right| left.0.cmp(&right.0));
      }
      metadata_identity
    };
    let mut cached = self.subtree_roots_cache.lock().expect("subtree roots cache mutex is not poisoned");
    if let (Some(metadata_identity), Some(snapshot)) = (metadata_identity.as_ref(), cached.as_ref())
      && snapshot.metadata_identity.len() == metadata_identity.len()
      && snapshot
        .metadata_identity
        .iter()
        .zip(metadata_identity)
        .all(|((left_id, left), (right_id, right))| left_id == right_id && Arc::ptr_eq(left, right))
    {
      return Ok(SharedSubtreeRoots { sequence: Arc::clone(&snapshot.sequence), views: Arc::clone(&snapshot.views) });
    }
    let sequence = Arc::new(self.sequence_listing.sequence_subtree_roots(&rows)?);
    let views = Arc::new(self.sequence_listing.view_subtree_roots(&rows)?);
    if let Some(metadata_identity) = metadata_identity {
      *cached = Some(super::store::CachedSubtreeRoots {
        metadata_identity,
        sequence: Arc::clone(&sequence),
        views: Arc::clone(&views),
      });
    }
    Ok(SharedSubtreeRoots { sequence, views })
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
        // A missing or unreadable or malformed file logs at debug level and drops out of the
        // scan, the way `json_utils.load_json_meta` reports its log event and answers None.
        Err(error) => {
          tracing::debug!(path = %path.display(), error = %error, "metadata_raw_scan_read_failed");
          continue;
        }
      };
      let value = match serde_json::from_str(&raw) {
        Ok(value) => value,
        Err(error) => {
          tracing::debug!(path = %path.display(), error = %error, "metadata_raw_scan_parse_skip");
          continue;
        }
      };
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
    let derived: HashSet<String> = self.derived_archived_ids()?.into_iter().collect();
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

  /// The shared-row form of [`Self::apply_derived_archive`]: rows keep their `Arc` and only a
  /// derived-archived row is replaced with a modified copy, so a listing hit clones no metadata.
  fn apply_derived_archive_shared(
    &self,
    snapshot: &[Arc<SessionMetadata>],
    status: Option<SessionStatus>,
  ) -> StoreResult<Vec<Arc<SessionMetadata>>> {
    let derived: HashSet<String> = self.derived_archived_ids()?.into_iter().collect();
    if derived.is_empty() {
      return Ok(snapshot.iter().map(Arc::clone).collect());
    }
    if status == Some(SessionStatus::Active) {
      return Ok(snapshot.iter().filter(|meta| !derived.contains(&meta.id)).map(Arc::clone).collect());
    }
    let mut rows = Vec::with_capacity(snapshot.len() + if status.is_none() { derived.len() } else { 0 });
    for meta in snapshot {
      if derived.contains(&meta.id) {
        let mut copy = meta.as_ref().clone();
        copy.status = SessionStatus::Archived;
        rows.push(Arc::new(copy));
      } else {
        rows.push(Arc::clone(meta));
      }
    }
    if status == Some(SessionStatus::Archived) {
      for meta in self.load_session_metas(Some(SessionStatus::Active))? {
        if derived.contains(&meta.id) {
          rows.push(Arc::new(meta));
        }
      }
    }
    Ok(rows)
  }

  fn apply_derived_archive(
    &self,
    mut metas: Vec<SessionMetadata>,
    status: Option<SessionStatus>,
  ) -> StoreResult<Vec<SessionMetadata>> {
    let derived: HashSet<String> = self.derived_archived_ids()?.into_iter().collect();
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

#[cfg(test)]
mod tests {
  use super::*;
  use crate::StoreHooks;
  use crate::metadata::TaskProfile;
  use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot, SlotField};
  use serde_json::json;
  use std::sync::Arc;
  use std::sync::atomic::{AtomicUsize, Ordering};

  static SUBTREE_MAP_BUILDS: AtomicUsize = AtomicUsize::new(0);

  struct Labels;
  impl crate::AccountLabelKeeper for Labels {
    fn record_account_label(&self, meta: &mut SessionMetadata, label: Option<&str>) -> StoreResult<bool> {
      let value = label.map_or(Value::Null, |label| Value::String(label.to_string()));
      if meta.extra("claude_account") == Some(&value) {
        return Ok(false);
      }
      meta.set_extra("claude_account", value);
      Ok(true)
    }

    fn account_label(&self, meta: &SessionMetadata) -> Option<String> {
      meta.extra("claude_account").and_then(Value::as_str).map(str::to_string)
    }
  }

  struct Anchors;
  impl crate::AnchorFieldReconciler for Anchors {
    fn reconcile(&self, disk: &SessionMetadata, attempted: &mut SessionMetadata) -> StoreResult<()> {
      attempted.cc_session_id.clone_from(&disk.cc_session_id);
      attempted.native_backend.clone_from(&disk.native_backend);
      Ok(())
    }
  }

  struct Sequence;
  impl crate::SequenceListingFields for Sequence {
    fn owns_session(&self, _meta: &SessionMetadata) -> bool {
      false
    }
    fn listing_fields(
      &self,
      _: &[String],
      _: UtcDateTime,
    ) -> StoreResult<HashMap<String, serde_json::Map<String, Value>>> {
      Ok(HashMap::new())
    }
    fn sequence_subtree_roots(&self, _: &[SessionMetadata]) -> StoreResult<HashMap<String, String>> {
      Ok(HashMap::new())
    }
    fn view_subtree_roots(&self, _: &[SessionMetadata]) -> StoreResult<HashMap<String, HashMap<String, String>>> {
      Ok(HashMap::new())
    }
  }

  struct CountingSequence;
  impl crate::SequenceListingFields for CountingSequence {
    fn owns_session(&self, _meta: &SessionMetadata) -> bool {
      false
    }
    fn listing_fields(
      &self,
      _: &[String],
      _: UtcDateTime,
    ) -> StoreResult<HashMap<String, serde_json::Map<String, Value>>> {
      Ok(HashMap::new())
    }
    fn sequence_subtree_roots(&self, _: &[SessionMetadata]) -> StoreResult<HashMap<String, String>> {
      SUBTREE_MAP_BUILDS.fetch_add(1, Ordering::Relaxed);
      Ok(HashMap::new())
    }
    fn view_subtree_roots(&self, _: &[SessionMetadata]) -> StoreResult<HashMap<String, HashMap<String, String>>> {
      SUBTREE_MAP_BUILDS.fetch_add(1, Ordering::Relaxed);
      Ok(HashMap::new())
    }
  }

  fn slots() -> Vec<MetadataSlot> {
    vec![MetadataSlot {
      owner: "probe",
      on: MetadataFile::Session,
      after: Some("group"),
      fields: vec![
        SlotField { name: "probe_note", default: Value::Null },
        SlotField { name: "probe_count", default: json!(0) },
      ],
    }]
  }

  fn test_store(home: &std::path::Path) -> Store {
    let cfg = charliebot_core::config::load_config(home, &charliebot_core::config::ConfigSchema::default()).unwrap();
    Store::new(
      cfg,
      slots(),
      StoreHooks {
        account_labels: Some(Arc::new(Labels)),
        anchor_reconciler: Some(Arc::new(Anchors)),
        sequence_listing: Some(Arc::new(Sequence)),
        runtime_events: Some(Arc::new(crate::hooks::EmptyRuntimeEventHooks)),
        sequence_archive: Some(Arc::new(crate::store::test_support::SequenceArchive)),
        sequence_binding_mover: Some(Arc::new(crate::store::test_support::BindingMover)),
      },
    )
    .unwrap()
  }

  fn counting_store(home: &std::path::Path) -> Store {
    let cfg = charliebot_core::config::load_config(home, &charliebot_core::config::ConfigSchema::default()).unwrap();
    Store::new(
      cfg,
      slots(),
      StoreHooks {
        account_labels: Some(Arc::new(Labels)),
        anchor_reconciler: Some(Arc::new(Anchors)),
        sequence_listing: Some(Arc::new(CountingSequence)),
        runtime_events: Some(Arc::new(crate::hooks::EmptyRuntimeEventHooks)),
        sequence_archive: Some(Arc::new(crate::store::test_support::SequenceArchive)),
        sequence_binding_mover: Some(Arc::new(crate::store::test_support::BindingMover)),
      },
    )
    .unwrap()
  }

  fn write_metadata(store: &Store, id: &str, status: SessionStatus, updated_at: &str, group: Option<&str>) {
    let mut meta = SessionMetadata::new(id, id, TaskProfile::Manager);
    meta.status = status;
    meta.updated_at = UtcDateTime::parse(updated_at).unwrap();
    meta.group = group.map(str::to_string);
    let path = store.metadata_path(id);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, meta.to_json_string(&store.slots, true)).unwrap();
  }

  /// `load_session_metas`' memo: within the sweep interval a listing answers again until a write through the store or
  /// a new session directory moves its key.
  #[tokio::test]
  async fn a_listing_answers_again_until_a_store_write_or_a_new_session_directory() {
    let home = tempfile::tempdir().unwrap();
    let store = test_store(home.path());
    write_metadata(&store, "alpha", SessionStatus::Active, "2025-02-01T00:00:00Z", None);
    write_metadata(&store, "beta", SessionStatus::Active, "2025-02-02T00:00:00Z", None);
    let names = |store: &Store| {
      let mut rows: Vec<(String, String)> =
        store.load_session_metas(None).unwrap().into_iter().map(|meta| (meta.id, meta.name)).collect();
      rows.sort();
      rows
    };
    assert_eq!(names(&store), [("alpha".to_string(), "alpha".to_string()), ("beta".to_string(), "beta".to_string())]);

    // An out-of-band edit moves no key, so the memoized rows answer even after the entry's cache window.
    let edited = SessionMetadata::new("alpha", "alpha renamed", TaskProfile::Manager);
    std::fs::write(store.metadata_path("alpha"), edited.to_json_string(&store.slots, true)).unwrap();
    store.metadata_cache.lock().unwrap().get_mut("alpha").unwrap().at =
      Instant::now() - crate::store::METADATA_CACHE_TTL - Duration::from_secs(1);
    assert_eq!(names(&store)[0].1, "alpha");

    // A write through the store moves the revision: the walk revalidates the expired entry and reads the edit.
    let mut beta = store.get_session("beta").await.unwrap().unwrap();
    beta.group = Some("grouped".to_string());
    store.save_metadata_locked(&mut beta, false).unwrap();
    assert_eq!(names(&store)[0].1, "alpha renamed");

    // A new session directory moves the sessions root's signature.
    write_metadata(&store, "gamma", SessionStatus::Active, "2025-02-03T00:00:00Z", None);
    assert_eq!(names(&store).len(), 3);
  }

  /// The sessions directory's name list is memoized on the root's signature, as `session_store.py`'s
  /// `_dir_names_memo` is: a write that moves the listings revision re-walks the rows but not the directory.
  #[tokio::test]
  async fn a_listing_walk_re_reads_the_directory_only_after_the_root_signature_moves() {
    let home = tempfile::tempdir().unwrap();
    let store = test_store(home.path());
    write_metadata(&store, "alpha", SessionStatus::Active, "2025-02-01T00:00:00Z", None);
    store.load_session_metas(None).unwrap();
    let scans = store.directory_scans.load(Ordering::Relaxed);
    assert_eq!(scans, 1, "the first listing walks the directory once");

    // A write through the store moves the listings revision, so the next listing re-walks its rows.
    let mut alpha = store.get_session("alpha").await.unwrap().unwrap();
    alpha.group = Some("grouped".to_string());
    store.save_metadata_locked(&mut alpha, false).unwrap();
    assert_eq!(store.load_session_metas(None).unwrap().len(), 1);
    assert_eq!(
      store.directory_scans.load(Ordering::Relaxed),
      scans,
      "an unchanged sessions root reuses the memoized name list"
    );

    // A new session directory moves the root's signature, so the next walk rescans.
    write_metadata(&store, "beta", SessionStatus::Active, "2025-02-02T00:00:00Z", None);
    assert_eq!(store.load_session_metas(None).unwrap().len(), 2);
    assert_eq!(store.directory_scans.load(Ordering::Relaxed), scans + 1, "a new directory moves the key");
  }

  /// Python's `load_session_metas` serves its memoized rows read-only on a hit
  /// (`src/runtime/session_store.py:316-330`), so a listing clones no metadata. The Rust corpus must hand out
  /// the same `Arc` rows on a hit, or every sidebar listing pays a full per-row copy again.
  #[test]
  fn a_listing_memo_hit_hands_out_the_memoized_rows_without_copying_them() {
    let home = tempfile::tempdir().unwrap();
    let store = test_store(home.path());
    write_metadata(&store, "alpha", SessionStatus::Active, "2025-02-01T00:00:00Z", None);
    write_metadata(&store, "beta", SessionStatus::Active, "2025-02-02T00:00:00Z", None);

    let (first, _) = store.load_session_metas_versioned(None).unwrap();
    let (second, _) = store.load_session_metas_versioned(None).unwrap();
    assert!(Arc::ptr_eq(&first, &second), "the second read serves the memoized corpus");
    assert!(Arc::ptr_eq(&first[0], &second[0]), "each row keeps its memoized allocation");
  }

  #[test]
  fn subtree_maps_build_once_per_metadata_identity_and_rebuild_after_invalidation() {
    let home = tempfile::tempdir().unwrap();
    let store = counting_store(home.path());
    write_metadata(&store, "node-a", SessionStatus::Active, "2026-08-01T12:00:00Z", None);
    write_metadata(&store, "node-b", SessionStatus::Active, "2026-08-01T12:01:00Z", None);
    SUBTREE_MAP_BUILDS.store(0, Ordering::Relaxed);

    let first = store.subtree_roots_shared().unwrap();
    let warm = store.subtree_roots_shared().unwrap();
    assert!(Arc::ptr_eq(&first.sequence, &warm.sequence));
    assert!(Arc::ptr_eq(&first.views, &warm.views));
    assert_eq!(SUBTREE_MAP_BUILDS.load(Ordering::Relaxed), 2);

    write_metadata(&store, "node-a", SessionStatus::Active, "2026-08-01T12:02:00Z", None);
    store.invalidate_cache("node-a");
    store.subtree_roots_shared().unwrap();
    assert_eq!(SUBTREE_MAP_BUILDS.load(Ordering::Relaxed), 4);
  }

  /// `test_sessions_batch_metadata_read.py::test_batch_output_matches_sequential_get_session_for_mixed_fixture`:
  /// the listing skips the malformed file while a single-session read keeps Python's behavior and
  /// fails loudly with the file's path.
  #[tokio::test]
  async fn listing_skips_a_malformed_metadata_file_and_a_single_session_read_fails_loudly() {
    let home = tempfile::tempdir().unwrap();
    let store = test_store(home.path());
    write_metadata(&store, "good-a", SessionStatus::Active, "2025-02-01T00:00:00Z", None);
    write_metadata(&store, "good-b", SessionStatus::Archived, "2025-02-02T00:00:00Z", None);
    let corrupt_path = store.metadata_path("corrupt");
    std::fs::create_dir_all(corrupt_path.parent().unwrap()).unwrap();
    std::fs::write(&corrupt_path, "{corrupt").unwrap();
    let empty_path = store.metadata_path("empty");
    std::fs::create_dir_all(empty_path.parent().unwrap()).unwrap();
    std::fs::write(&empty_path, "   ").unwrap();

    let mut ids: Vec<String> = store.load_session_metas(None).unwrap().into_iter().map(|meta| meta.id).collect();
    ids.sort();
    assert_eq!(ids.iter().map(String::as_str).collect::<Vec<_>>(), ["good-a", "good-b"]);
    let archived: Vec<String> =
      store.load_session_metas(Some(SessionStatus::Archived)).unwrap().into_iter().map(|meta| meta.id).collect();
    assert_eq!(archived.iter().map(String::as_str).collect::<Vec<_>>(), ["good-b"]);

    let error = store.get_session("corrupt").await.unwrap_err().to_string();
    assert!(error.contains(corrupt_path.to_str().unwrap()), "{error}");
    assert!(store.read_metadata_fresh("corrupt").is_err());

    let mut raw: Vec<(String, String)> = store
      .raw_metadata_scan()
      .unwrap()
      .into_iter()
      .map(|row| (row.session_id, row.value["name"].as_str().unwrap().to_string()))
      .collect();
    raw.sort();
    assert_eq!(raw, [("good-a".to_string(), "good-a".to_string()), ("good-b".to_string(), "good-b".to_string())]);
  }

  /// `test_sessions_batch_metadata_read.py::test_metadata_without_profile_names_the_file_and_conversion_tool`:
  /// a single-session read of a profile-less metadata file fails loudly, naming the file and the
  /// conversion tool.
  #[tokio::test]
  async fn a_metadata_file_without_a_profile_fails_loudly_naming_the_file_and_the_conversion_tool() {
    let home = tempfile::tempdir().unwrap();
    let store = test_store(home.path());
    let path = store.metadata_path("missing-profile");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, r#"{"id": "missing-profile", "name": "old session", "schema_version": 1}"#).unwrap();
    let error = store.get_session("missing-profile").await.unwrap_err().to_string();
    assert!(error.contains(path.to_str().unwrap()), "{error}");
    assert!(error.contains("scripts/v1_session_conversion.py"), "{error}");
  }

  /// `test_archived_pagination.py::test_keyset_pages_walk_newest_first`.
  #[test]
  fn archived_keyset_pages_walk_newest_first() {
    let home = tempfile::tempdir().unwrap();
    let store = test_store(home.path());
    let ids: Vec<String> = (0..5)
      .map(|index| {
        let id = format!("s{index}");
        write_metadata(&store, &id, SessionStatus::Archived, &format!("2026-08-01T12:{:02}:00Z", index), None);
        id
      })
      .collect();

    let first = store.list_archived_page(None, 2, None, None).unwrap();
    assert_eq!(first.sessions.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(), [&ids[4], &ids[3]]);
    assert!(first.has_more);
    assert_eq!(first.next_before.as_deref(), Some("2026-08-01T12:03:00+00:00"));
    assert_eq!(first.next_before_id.as_deref(), Some(ids[3].as_str()));

    let second =
      store.list_archived_page(None, 2, first.next_before.as_deref(), first.next_before_id.as_deref()).unwrap();
    assert_eq!(second.sessions.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(), [&ids[2], &ids[1]]);
    assert!(second.has_more);

    let third =
      store.list_archived_page(None, 2, second.next_before.as_deref(), second.next_before_id.as_deref()).unwrap();
    assert_eq!(third.sessions.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(), [&ids[0]]);
    assert!(!third.has_more);
    assert_eq!(third.next_before, None);
    assert_eq!(third.next_before_id, None);
  }

  /// `test_archived_pagination.py::test_bad_cursor_fails_loudly`.
  #[test]
  fn archived_page_cursor_fails_loudly() {
    let home = tempfile::tempdir().unwrap();
    let store = test_store(home.path());
    write_metadata(&store, "s0", SessionStatus::Archived, "2026-08-01T12:00:00Z", None);
    assert!(store.list_archived_page(None, 2, Some("not-a-timestamp"), Some("x")).is_err());
    assert!(store.list_archived_page(None, 2, Some("2026-08-01T12:00:00"), Some("x")).is_err());
    let error = store
      .list_archived_page(None, 2, Some("2026-08-01T12:00:00+00:00"), None)
      .expect_err("half a cursor must refuse");
    assert!(error.to_string().contains("both or neither"), "{error}");
  }

  /// `test_sessions_status_scope.py::test_status_returns_exactly_the_requested_ids`, at the store
  /// level: each status listing returns exactly that status's sessions.
  #[test]
  fn status_listings_return_exactly_the_requested_status() {
    let home = tempfile::tempdir().unwrap();
    let store = test_store(home.path());
    write_metadata(&store, "live-a", SessionStatus::Active, "2025-02-04T00:00:00Z", None);
    write_metadata(&store, "live-b", SessionStatus::Active, "2025-02-05T00:00:00Z", Some("Alpha"));
    write_metadata(&store, "archived-a", SessionStatus::Archived, "2025-02-01T00:00:00Z", None);
    write_metadata(&store, "archived-b", SessionStatus::Archived, "2025-02-02T00:00:00Z", Some("Beta"));

    let mut active: Vec<String> =
      store.load_session_metas(Some(SessionStatus::Active)).unwrap().into_iter().map(|meta| meta.id).collect();
    active.sort();
    assert_eq!(active.iter().map(String::as_str).collect::<Vec<_>>(), ["live-a", "live-b"]);
    let mut archived: Vec<String> =
      store.load_session_metas(Some(SessionStatus::Archived)).unwrap().into_iter().map(|meta| meta.id).collect();
    archived.sort();
    assert_eq!(archived.iter().map(String::as_str).collect::<Vec<_>>(), ["archived-a", "archived-b"]);
    assert_eq!(store.list_group_names().unwrap(), ["Alpha", "Beta"]);
  }

  /// `test_session_archive_empty.py::test_archive_non_empty_session_keeps_files_and_marks_archived`
  /// at store level: archiving a session with events keeps its directory and event log, and the
  /// session lists as archived.
  #[tokio::test]
  async fn archiving_a_session_with_events_keeps_its_files_and_lists_it_as_archived() {
    let home = tempfile::tempdir().unwrap();
    let store = test_store(home.path());
    write_metadata(&store, "non-empty", SessionStatus::Active, "2025-02-01T00:00:00Z", None);
    store.append_chat_event("non-empty", json!({"type": "user", "content": "hello"})).await.unwrap();
    let session_dir = store.session_dir("non-empty");
    let events_path = store.chat_events_path("non-empty");

    let archived = store.archive_session("non-empty").await.unwrap().unwrap();
    assert_eq!(archived.status, SessionStatus::Archived);
    assert!(session_dir.exists());
    assert!(events_path.exists());
    assert_eq!(store.get_session("non-empty").await.unwrap().unwrap().status, SessionStatus::Archived);
    let archived_ids: Vec<String> =
      store.load_session_metas(Some(SessionStatus::Archived)).unwrap().into_iter().map(|meta| meta.id).collect();
    assert_eq!(archived_ids, ["non-empty"]);
  }

  /// `test_sessions_batch_metadata_read.py`:
  /// `test_session_pinned_to_a_backend_the_config_no_longer_defines_loads_lists_and_refuses_a_new_run`
  /// at store level: a session pinned to a backend the config no longer defines still loads, lists,
  /// and reads its events. The refusal of a new run belongs to the backend resolver.
  #[tokio::test]
  async fn a_session_pinned_to_an_undefined_backend_still_loads_lists_and_reads_its_events() {
    let home = tempfile::tempdir().unwrap();
    let store = test_store(home.path());
    let mut meta = SessionMetadata::new("pinned", "stored", TaskProfile::Manager);
    meta.backend = "claude-fable-sub".to_string();
    let path = store.metadata_path("pinned");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, meta.to_json_string(&store.slots, true)).unwrap();
    store.append_chat_event("pinned", json!({"type": "user", "content": "hello"})).await.unwrap();

    let loaded = store.get_session("pinned").await.unwrap().unwrap();
    assert_eq!(loaded.backend, "claude-fable-sub");
    let listed: Vec<String> = store.load_session_metas(None).unwrap().into_iter().map(|meta| meta.id).collect();
    assert_eq!(listed, ["pinned"]);
    let events = store.load_chat_events_sync("pinned").unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["content"].as_str(), Some("hello"));
  }
}
