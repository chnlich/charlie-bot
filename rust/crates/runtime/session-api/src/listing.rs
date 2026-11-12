//! Session listing projections shared by sidebar routes.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex, Weak};

use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use charliebot_registry::Registry;
use charliebot_registry::thread_origin::is_thread_session;
use charliebot_store::metadata::{
  HAS_PENDING_TRIGGER, HAS_RUNNING_TASKS, HAS_UNREAD, NEXT_TRIGGER_AT, PENDING_TRIGGER_COUNT, ROUND_RATINGS,
  SCHEDULE_ENABLED, SCHEDULE_FIELDS, SCHEDULE_TASK, SCHEMA_VERSION_FIELD, SessionMetadata, SessionStatus, TaskProfile,
  UtcDateTime, WORK_STATE,
};
use charliebot_store::sidebar_state::HAS_PENDING_PLAN_APPROVAL;
use charliebot_store::{SharedSubtreeRoots, Store, StoreResult};
use http::HeaderValue;
use serde_json::{Map, Value, json};

use crate::sidebar;
pub use crate::sidebar::DerivedSidebarMap;

/// The fields one sidebar row renders: which derived facts it carries and whether it names the run's backend.
#[derive(Clone, Copy)]
pub struct SidebarProjection {
  pub include_running: bool,
  pub include_trigger: bool,
  pub include_plan: bool,
  pub display_backend: bool,
  pub include_run_backend: bool,
}

#[derive(Default)]
struct SessionsListMemoState {
  whole_body: Option<WholeListingBody>,
  #[cfg(test)]
  render_calls: usize,
}

/// One rendered listing body, its corpus, its inputs, and the gzip form a request asked for.
///
/// The gzip form lives beside the body it compressed. A request that matches the inputs serves both without hashing
/// or comparing the body, which the memo would otherwise do to find the compressed bytes it already holds. Both are
/// `Bytes` handles, so a served response shares the stored buffer through a refcount bump. A full listing's
/// compressed form is hundreds of kilobytes, and copying it per request cost more than the rest of the response.
struct WholeListingBody {
  source: ListingSource,
  inputs: Vec<RowInputs>,
  body: Bytes,
  gzip: Option<Bytes>,
}

/// The corpus one listing body rendered from, compared before the per-row inputs are rebuilt.
///
/// The rows compare by pointer. The store hands out one shared, immutable row while its content holds, so another
/// pointer means other content while the same pointer means an equal row. The derived overlays compare by pointer
/// too. Their builder keeps one map until a sidebar input moves, and every move bumps the generation that keys the
/// map, so the same map means the same overlays. The controllers build a fresh field map per request, so their
/// fields compare by value. Every other input a row reads from the store (busy state, run backend) moves through a
/// sidebar dirty mark, which also rebuilds the derived map.
struct ListingSource {
  /// The corpus when the caller handed out the listing memo's own allocation, compared by pointer.
  shared: Option<Arc<Vec<Arc<SessionMetadata>>>>,
  rows: Vec<Arc<SessionMetadata>>,
  derived: Arc<DerivedSidebarMap>,
  sequence_fields: HashMap<String, Map<String, Value>>,
}

impl ListingSource {
  fn of(
    rows: &[Arc<SessionMetadata>],
    shared: Option<&Arc<Vec<Arc<SessionMetadata>>>>,
    derived: &Arc<DerivedSidebarMap>,
    sequence_fields: &HashMap<String, Map<String, Value>>,
  ) -> Self {
    Self {
      shared: shared.cloned(),
      rows: rows.to_vec(),
      derived: Arc::clone(derived),
      sequence_fields: sequence_fields.clone(),
    }
  }

  fn holds(
    &mut self,
    rows: &[Arc<SessionMetadata>],
    shared: Option<&Arc<Vec<Arc<SessionMetadata>>>>,
    derived: &Arc<DerivedSidebarMap>,
    sequence_fields: &HashMap<String, Map<String, Value>>,
  ) -> bool {
    // The listing route hands out the memoized corpus itself, so one pointer comparison proves the same rows in the
    // same order instead of walking all of them. A route that filters the corpus into its own list passes no shared
    // allocation and keeps the row-by-row comparison. A rebuilt corpus that holds equal rows in a new allocation
    // still matches, and the source takes that allocation so the next request pays one comparison.
    let rows_hold = match (self.shared.as_ref(), shared) {
      (Some(before), Some(now)) if Arc::ptr_eq(before, now) => true,
      _ => {
        let equal = self.rows.len() == rows.len()
          && self.rows.iter().zip(rows.iter()).all(|(before, now)| Arc::ptr_eq(before, now));
        if equal {
          self.shared = shared.cloned();
          self.rows = rows.to_vec();
        }
        equal
      }
    };
    rows_hold && Arc::ptr_eq(&self.derived, derived) && self.sequence_fields == *sequence_fields
  }
}

/// What one sidebar row renders from: its metadata, its derived overlay, its busy mark, its run and display backends,
/// and the controller's listing fields. Rows with equal inputs render equal values.
pub(crate) struct RowInputs {
  meta: Arc<SessionMetadata>,
  derived: Option<Arc<Map<String, Value>>>,
  thinking_since: Value,
  run_backend: Option<String>,
  display_backend: Option<String>,
  listing: Option<Map<String, Value>>,
}

impl PartialEq for RowInputs {
  /// Equal when every input still names the same value.
  ///
  /// The metadata compares by identity. The store hands out one shared, immutable row for as long as its content
  /// holds, so another pointer means other content while the same pointer means an equal row. Reading it by content
  /// instead compares every field of every row's metadata on each request of a listing or a page.
  ///
  /// The derived overlay compares by content. A rebuilt overlay hands out new maps for rows whose derived state did
  /// not move, so the memo still serves the row it rendered before.
  fn eq(&self, other: &Self) -> bool {
    Arc::ptr_eq(&self.meta, &other.meta)
      && self.derived == other.derived
      && self.thinking_since == other.thinking_since
      && self.run_backend == other.run_backend
      && self.display_backend == other.display_backend
      && self.listing == other.listing
  }
}

/// The render and gzip memos owned by one sidebar listing route.
#[derive(Default)]
pub struct SessionsListMemos {
  state: Mutex<SessionsListMemoState>,
}

/// The active corpus a sidebar listing renders from: its rows, their derived overlays, and the controllers' row fields.
pub struct ListingCorpus {
  /// Active, unscheduled sessions, newest first.
  pub rows: Vec<Arc<SessionMetadata>>,
  /// The derived overlay of each row, keyed by session id.
  pub derived: Arc<DerivedSidebarMap>,
  /// The controllers' listing fields of each row, keyed by session id.
  pub sequence_fields: HashMap<String, Map<String, Value>>,
}

/// The active listing corpus one request built, reused while every input that shapes it still holds.
///
/// Building the corpus walks every active session: the store filters the corpus and asks each sequence controller
/// which rows it owns, and the sidebar resolves each row's overlay. A repeat listing reads the same corpus, so the
/// memo answers it from the identities that would move if any of it changed. The listing generation numbers the
/// metadata corpus, the derived archive ids name the archive filter, and the sidebar generation names the overlays.
/// Both identities are held weakly: a dropped store's corpus never answers another store's request.
static ACTIVE_CORPUS_MEMO: LazyLock<Mutex<Option<ActiveCorpusEntry>>> = LazyLock::new(|| Mutex::new(None));

/// One memo entry. The memo holds it by value; only the store and registry it names are weak.
struct ActiveCorpusEntry {
  store: Weak<Store>,
  registry: Weak<Registry>,
  generation: u64,
  archived: Arc<Vec<String>>,
  sidebar_generation: u64,
  rows: Arc<Vec<Arc<SessionMetadata>>>,
  derived: Arc<DerivedSidebarMap>,
}

impl ActiveCorpusEntry {
  /// Whether this entry was built from the corpus and overlays that `store` and `registry` hold now.
  fn holds(
    &self,
    store: &Arc<Store>,
    registry: &Arc<Registry>,
    generation: u64,
    archived: &Arc<Vec<String>>,
    sidebar_generation: u64,
  ) -> bool {
    // An upgrade proves the named store still lives: a dropped store's address may be reused by the next store, and
    // its entry must never answer that one.
    self.store.upgrade().is_some_and(|held| Arc::ptr_eq(&held, store))
      && self.registry.upgrade().is_some_and(|held| Arc::ptr_eq(&held, registry))
      && self.generation == generation
      // The same index answers the common request, so the shared list's pointer is the whole comparison. A rebuilt
      // index holds an equal list in a new allocation, and the value comparison keeps the corpus it answered for.
      && (Arc::ptr_eq(&self.archived, archived) || self.archived.as_slice() == archived.as_slice())
      && self.sidebar_generation == sidebar_generation
  }
}

/// Fetch active, non-scheduled sessions, their sidebar overlays and their controller row fields.
pub fn active_listing_corpus(store: &Arc<Store>, registry: &Arc<Registry>) -> StoreResult<ListingCorpus> {
  let (_, generation) = store.load_session_metas_snapshot_versioned(Some(SessionStatus::Active))?;
  let archived = store.derived_archived_ids()?;
  let sidebar_generation = charliebot_store::sidebar_state::derived_generation();
  let cached = {
    let memo = ACTIVE_CORPUS_MEMO.lock().expect("the active corpus memo mutex is not poisoned");
    memo
      .as_ref()
      .filter(|entry| entry.holds(store, registry, generation, &archived, sidebar_generation))
      .map(|entry| (Arc::clone(&entry.rows), Arc::clone(&entry.derived)))
  };
  if let Some((rows, derived)) = cached {
    // A memo hit still polls, because the periodic self-heal sweep is what catches an edit from outside the server.
    sidebar::count_memoized_read(store, registry, &rows)?;
    // The controllers' fields follow their own bindings, which move without the metadata corpus, so a repeat read
    // rebuilds them and serves only the rows and overlays from the memo.
    let sequence_fields = store.sequence_listing_fields(&rows)?;
    return Ok(ListingCorpus { rows: rows.as_ref().clone(), derived, sequence_fields });
  }
  let listed = store.list_sessions(Some(SessionStatus::Active), None, Some(false))?;
  let derived = sidebar::resolve_sidebar_state(store, registry, &listed.rows, true, true, true, false)?;
  // Resolving the overlays probes the sessions that moved, and one probe bumps the sidebar generation, so the build
  // itself can move an input. A build that moved one stores nothing: its overlays were read under the older
  // generation, and the next request rebuilds them under the newer one and stores them.
  if charliebot_store::sidebar_state::derived_generation() == sidebar_generation {
    *ACTIVE_CORPUS_MEMO.lock().expect("the active corpus memo mutex is not poisoned") = Some(ActiveCorpusEntry {
      store: Arc::downgrade(store),
      registry: Arc::downgrade(registry),
      generation,
      archived,
      sidebar_generation,
      rows: Arc::new(listed.rows.clone()),
      derived: Arc::clone(&derived),
    });
  }
  Ok(ListingCorpus { rows: listed.rows, derived, sequence_fields: listed.sequence_fields })
}

/// The sidebar corpus: active, unscheduled sessions outside every owned subtree, with their overlays.
///
/// The chat page renders these rows into its sidebar text, and the listing route answers them as JSON.
pub struct PageRows {
  rows: Arc<Vec<Arc<SessionMetadata>>>,
  /// Each listed id's position in `rows`, built with the rows.
  ///
  /// A row refresh names a few ids and merges those rows alone. Walking the corpus to find them hashed every listed
  /// id, and hashing 7,160 ids cost more than rendering the rows it found.
  index: Arc<HashMap<String, u32>>,
  derived: Arc<DerivedSidebarMap>,
  sequence_fields: HashMap<String, Map<String, Value>>,
}

impl PageRows {
  /// The corpus rows, in listing order.
  pub fn rows(&self) -> &[Arc<SessionMetadata>] {
    &self.rows
  }

  /// The rows `ids` name, in page order.
  fn rows_named(&self, ids: &[String]) -> Vec<Arc<SessionMetadata>> {
    let mut found: Vec<u32> = ids.iter().filter_map(|id| self.index.get(id.as_str()).copied()).collect();
    found.sort_unstable();
    found.dedup();
    found.into_iter().map(|index| Arc::clone(&self.rows[index as usize])).collect()
  }

  /// The derived sidebar overlay of each row.
  pub fn derived(&self) -> &Arc<DerivedSidebarMap> {
    &self.derived
  }

  /// The sequence controllers' listing fields of the rows they bind.
  pub fn sequence_fields(&self) -> &HashMap<String, Map<String, Value>> {
    &self.sequence_fields
  }
}

/// The page corpus one request built, reused while every input that shapes it still holds.
///
/// Building the corpus walks every active session: the store filters the corpus, the sidebar resolves each row's
/// overlay, and the page drops the rows that an owned subtree holds. A repeat page request reads the same corpus, so
/// the memo answers it from the identities that would move if any of it changed. The listing generation names the
/// metadata corpus: the store numbers every rebuild, so the same number is the same rows in the same order. The
/// derived archive ids and the subtree roots name the two filters, and the sidebar generation names the overlays.
/// The repository identities name the store and registry the corpus came from. Both are held weakly: a dropped
/// store's corpus never answers another store's request.
static PAGE_CORPUS_MEMO: LazyLock<Mutex<Option<PageCorpusEntry>>> = LazyLock::new(|| Mutex::new(None));

/// One memo entry. The memo holds it by value; only the store and registry it names are weak.
struct PageCorpusEntry {
  store: Weak<Store>,
  registry: Weak<Registry>,
  generation: u64,
  archived: Arc<Vec<String>>,
  sequence_roots: Arc<HashMap<String, String>>,
  view_roots: Arc<HashMap<String, HashMap<String, String>>>,
  sidebar_generation: u64,
  rows: Arc<Vec<Arc<SessionMetadata>>>,
  index: Arc<HashMap<String, u32>>,
  derived: Arc<DerivedSidebarMap>,
}

impl PageCorpusEntry {
  /// Whether this entry was built from the corpus and overlays that `store` and `registry` hold now.
  fn holds(
    &self,
    store: &Arc<Store>,
    registry: &Arc<Registry>,
    generation: u64,
    archived: &Arc<Vec<String>>,
    roots: &SharedSubtreeRoots,
    sidebar_generation: u64,
  ) -> bool {
    Weak::ptr_eq(&self.store, &Arc::downgrade(store))
      && Weak::ptr_eq(&self.registry, &Arc::downgrade(registry))
      && self.generation == generation
      // The same index answers the common request, so the shared list's pointer is the whole comparison. A rebuilt
      // index holds an equal list in a new allocation, and the value comparison keeps the corpus it answered for.
      && (Arc::ptr_eq(&self.archived, archived) || self.archived.as_slice() == archived.as_slice())
      && Arc::ptr_eq(&self.sequence_roots, &roots.sequence)
      && Arc::ptr_eq(&self.view_roots, &roots.views)
      && self.sidebar_generation == sidebar_generation
  }
}

/// Read the sidebar corpus without rendering a row. A memo hit still polls the sidebar state, because the periodic
/// self-heal sweep is what catches an edit from outside the server.
pub fn active_page_rows(store: &Arc<Store>, registry: &Arc<Registry>) -> StoreResult<PageRows> {
  let corpus = page_corpus(store, registry)?;
  let sequence_fields = store.sequence_listing_fields(&corpus.rows)?;
  Ok(PageRows { rows: corpus.rows, index: corpus.index, derived: corpus.derived, sequence_fields })
}

/// The Workspace page's row corpus and the derived overlay that names it, without the sequence controllers' row fields.
///
/// The status poll reads only the row ids and the overlay, and the overlay is the corpus' identity: a rebuild
/// replaces it whenever a row, an overlay or the order moves. Asking for the rendered page's controller fields here
/// would sort and clone every id on each poll, so this route reads the corpus alone.
pub struct PageCorpusRows {
  pub rows: Arc<Vec<Arc<SessionMetadata>>>,
  pub derived: Arc<DerivedSidebarMap>,
}

pub fn page_corpus_rows(store: &Arc<Store>, registry: &Arc<Registry>) -> StoreResult<PageCorpusRows> {
  let corpus = page_corpus(store, registry)?;
  Ok(PageCorpusRows { rows: corpus.rows, derived: corpus.derived })
}

/// The sidebar corpus with the sequence controller fields of `ids` alone. A row refresh reads this: it merges the
/// rows it names, so the corpus' other rows need no controller fields.
pub fn active_page_rows_for(store: &Arc<Store>, registry: &Arc<Registry>, ids: &[String]) -> StoreResult<PageRows> {
  let corpus = page_corpus(store, registry)?;
  let sequence_fields = store.sequence_listing_fields_for(ids)?;
  Ok(PageRows { rows: corpus.rows, index: corpus.index, derived: corpus.derived, sequence_fields })
}

/// The page corpus' rows, their id index, and the overlays.
struct PageCorpus {
  rows: Arc<Vec<Arc<SessionMetadata>>>,
  index: Arc<HashMap<String, u32>>,
  derived: Arc<DerivedSidebarMap>,
}

/// The id index of a page's rows.
fn page_row_index(rows: &[Arc<SessionMetadata>]) -> Arc<HashMap<String, u32>> {
  Arc::new(rows.iter().enumerate().map(|(index, row)| (row.id.clone(), index as u32)).collect())
}

/// The sidebar corpus, from the memo while every input that shapes it still holds.
fn page_corpus(store: &Arc<Store>, registry: &Arc<Registry>) -> StoreResult<PageCorpus> {
  let (_, generation) = store.load_session_metas_snapshot_versioned(Some(SessionStatus::Active))?;
  let archived = store.derived_archived_ids()?;
  let roots = store.subtree_roots_shared()?;
  let sidebar_generation = charliebot_store::sidebar_state::derived_generation();
  let cached = {
    let memo = PAGE_CORPUS_MEMO.lock().expect("the page corpus memo mutex is not poisoned");
    memo
      .as_ref()
      .filter(|entry| entry.holds(store, registry, generation, &archived, &roots, sidebar_generation))
      .map(|entry| (Arc::clone(&entry.rows), Arc::clone(&entry.index), Arc::clone(&entry.derived)))
  };
  if let Some((rows, index, derived)) = cached {
    // A memo hit still polls, because the periodic self-heal sweep is what catches an edit from outside the server.
    sidebar::count_memoized_read(store, registry, &rows)?;
    return Ok(PageCorpus { rows, index, derived });
  }
  let ListingCorpus { mut rows, derived, .. } = active_listing_corpus(store, registry)?;
  rows.retain(|row| outside_owned_subtrees(row, &roots));
  let rows = Arc::new(rows);
  let index = page_row_index(&rows);
  // Resolving the overlays probes the sessions that moved, and one probe bumps the sidebar generation, so the build
  // itself can move an input. A build that moved one stores nothing: its overlays were read under the older
  // generation, and the next request rebuilds them under the newer one and stores them.
  if charliebot_store::sidebar_state::derived_generation() == sidebar_generation {
    *PAGE_CORPUS_MEMO.lock().expect("the page corpus memo mutex is not poisoned") = Some(PageCorpusEntry {
      store: Arc::downgrade(store),
      registry: Arc::downgrade(registry),
      generation,
      archived,
      sequence_roots: roots.sequence,
      view_roots: roots.views,
      sidebar_generation,
      rows: Arc::clone(&rows),
      index: Arc::clone(&index),
      derived: Arc::clone(&derived),
    });
  }
  Ok(PageCorpus { rows, index, derived })
}

/// The row ids a status poll names by filter, or `None` when the sidebar has no filter of that name.
///
/// `all` is the Workspace's page; `starred` is every starred session, archived ones included; any other name is a
/// sidebar view the registry knows, answered with the active rows that belong to it.
pub fn status_filter_ids(
  store: &Arc<Store>,
  registry: &Arc<Registry>,
  filter: &str,
) -> StoreResult<Option<Vec<String>>> {
  let ids = match filter {
    "all" => active_page_rows(store, registry)?.rows().iter().map(|row| row.id.clone()).collect(),
    "starred" => store.list_sessions(None, Some(true), None)?.rows.iter().map(|row| row.id.clone()).collect(),
    view => {
      let views = store.view_subtree_roots()?;
      let Some(members) = views.get(view) else { return Ok(None) };
      active_listing_corpus(store, registry)?
        .rows
        .iter()
        .filter(|row| members.contains_key(&row.id))
        .map(|row| row.id.clone())
        .collect()
    }
  };
  Ok(Some(ids))
}

/// The id of the row that a page opened without a session redirects to.
///
/// The redirect reads only the row order, so it skips the derived overlays and the controller row fields that the
/// rendered page needs.
pub fn first_page_row_id(store: &Arc<Store>) -> StoreResult<Option<String>> {
  let rows = store.list_session_rows(Some(SessionStatus::Active), None, Some(false))?;
  let roots = store.subtree_roots_shared()?;
  Ok(rows.iter().find(|row| outside_owned_subtrees(row, &roots)).map(|row| row.id.clone()))
}

/// Whether a row stands outside every sequence subtree and every view subtree.
fn outside_owned_subtrees(row: &SessionMetadata, roots: &SharedSubtreeRoots) -> bool {
  !roots.sequence.contains_key(&row.id) && !roots.views.values().any(|members| members.contains_key(&row.id))
}

/// The chat page's row projection: running and trigger state, the run backend, and no plan badge.
const PAGE_PROJECTION: SidebarProjection = SidebarProjection {
  include_running: true,
  include_trigger: true,
  include_plan: false,
  display_backend: true,
  include_run_backend: true,
};

/// A sidebar listing's row projection: running, trigger and plan state, and no run backend.
pub const LISTING_PROJECTION: SidebarProjection = SidebarProjection {
  include_running: true,
  include_trigger: true,
  include_plan: true,
  display_backend: true,
  include_run_backend: false,
};

/// The starred view's row projection: its route renders the run backend and no plan badge.
pub const STARRED_PROJECTION: SidebarProjection = SidebarProjection {
  include_running: true,
  include_trigger: true,
  include_plan: false,
  display_backend: false,
  include_run_backend: true,
};

/// The session value a sidebar row starts from.
///
/// The page renders the stored metadata, and its client draws the goal and the origin fields it reads. A listing
/// route renders the response model's form, [`metadata_value`], which names the chat-thread flag and leaves the goal
/// out.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowSchema {
  Page,
  Listing,
}

impl RowSchema {
  fn value(self, store: &Store, meta: &SessionMetadata) -> StoreResult<Value> {
    match self {
      RowSchema::Page => Ok(store.session_metadata_value(meta)),
      RowSchema::Listing => metadata_value(store, meta),
    }
  }
}

/// The page's whole sidebar text and the inputs it rendered from.
struct WholePageText {
  source: PageTextSource,
  inputs: Vec<RowInputs>,
  clear_unread: Option<String>,
  /// The session the text's first screen kept visible, which picks the rows the text carries.
  active: Option<String>,
  text: Arc<String>,
}

/// The corpus one page text rendered from, compared before the per-row inputs are rebuilt.
///
/// The rows compare by pointer. The store hands out one shared, immutable row while its content holds, so another
/// pointer means other content while the same pointer means an equal row. The derived overlays compare by pointer
/// too. Their builder keeps one map until a sidebar input moves, and every move bumps the generation that keys the
/// map, so the same map means the same overlays. The controllers build a fresh field map per request, so their
/// fields compare by value. Every other input a row reads from the store (busy state, run backend) moves through a
/// sidebar dirty mark, which also rebuilds the derived map.
struct PageTextSource {
  rows: Arc<Vec<Arc<SessionMetadata>>>,
  derived: Arc<DerivedSidebarMap>,
  sequence_fields: HashMap<String, Map<String, Value>>,
}

impl PageTextSource {
  fn of(page: &PageRows) -> Self {
    Self {
      rows: Arc::clone(&page.rows),
      derived: Arc::clone(&page.derived),
      sequence_fields: page.sequence_fields.clone(),
    }
  }

  fn holds(&mut self, page: &PageRows) -> bool {
    // The corpus arrives as one shared allocation: `active_page_rows` hands out the memoized list until an input
    // that shapes it moves, so the same pointer is the same rows and the same order. Comparing the rows one by one
    // walked all of them on every request. A rebuilt corpus that holds equal rows in a new allocation still matches,
    // and the source takes that allocation so the next request pays one comparison.
    let rows_hold = if Arc::ptr_eq(&self.rows, &page.rows) {
      true
    } else {
      let equal =
        self.rows.len() == page.rows.len() && self.rows.iter().zip(page.rows.iter()).all(|(a, b)| Arc::ptr_eq(a, b));
      if equal {
        self.rows = Arc::clone(&page.rows);
      }
      equal
    };
    rows_hold && Arc::ptr_eq(&self.derived, &page.derived) && self.sequence_fields == page.sequence_fields
  }
}

/// The chat page's rendered sidebar texts.
///
/// Every page request renders the same rows from the same inputs, so the memo serves the whole sidebar text until one
/// of the inputs [`page_client_value`] reads moves. A row whose inputs did not move keeps its fields across a rebuild,
/// so one changed row costs one row render rather than the whole sidebar. The memo holds the page's whole sidebar
/// text, about as much memory as one response body.
#[derive(Default)]
pub struct PageRowsMemo {
  state: Mutex<PageRowsMemoState>,
  #[cfg(test)]
  renders: std::sync::atomic::AtomicUsize,
}

#[derive(Default)]
struct PageRowsMemoState {
  rows: HashMap<String, Arc<PageRowText>>,
  whole: Option<WholePageText>,
}

impl PageRowsMemo {
  #[cfg(test)]
  fn render_calls(&self) -> usize {
    self.renders.load(std::sync::atomic::Ordering::Relaxed)
  }
}

/// Render the chat page's sidebar rows as their column text.
///
/// `clear_unread` names the active session. Its row renders with `has_unread` false and is left out of the memo, so
/// a later request that names another active session does not serve the marked row.
///
/// `active` names the session the page shows; the text carries the rows the sidebar paints before the reader expands
/// a group ([`PageFirstScreen`]), and `active`'s row and subtree are always among them.
pub fn active_page_json_text(
  store: &Store,
  page: &PageRows,
  clear_unread: Option<&str>,
  active: Option<&str>,
  memo: &PageRowsMemo,
) -> StoreResult<Arc<String>> {
  page_json_text(
    store,
    page,
    clear_unread,
    PageRowsScope::FirstScreen { active },
    memo,
    PAGE_PROJECTION,
    RowSchema::Page,
  )
}

/// Which rows a page's column text carries.
#[derive(Clone, Copy)]
pub(crate) enum PageRowsScope<'a> {
  /// Every row of the corpus, as the listing routes answer.
  EveryRow,
  /// The rows the chat page's sidebar paints before the reader expands a group ([`PageFirstScreen`]).
  FirstScreen { active: Option<&'a str> },
}

impl<'a> PageRowsScope<'a> {
  /// The open session whose row the first screen keeps visible.
  fn active(&self) -> Option<&'a str> {
    match self {
      Self::EveryRow => None,
      Self::FirstScreen { active } => *active,
    }
  }
}

/// Render one row projection of the sidebar corpus as its column text.
///
/// The text carries the fields of `projection` over the session value of `schema`, so each pair needs a memo of its
/// own: a memo serves the pair it filled from, and another projection or schema renders other values.
pub(crate) fn page_json_text(
  store: &Store,
  page: &PageRows,
  clear_unread: Option<&str>,
  scope: PageRowsScope<'_>,
  memo: &PageRowsMemo,
  projection: SidebarProjection,
  schema: RowSchema,
) -> StoreResult<Arc<String>> {
  // The corpus check answers the common request before it walks every row's inputs. It proves every input the text
  // reads still holds, so the text it returns is the text a rebuild would render.
  let active = scope.active();
  {
    let mut state = memo.state.lock().expect("the page row memo mutex is not poisoned");
    if let Some(whole) = &mut state.whole
      && whole.clear_unread.as_deref() == clear_unread
      && whole.active.as_deref() == active
      && whole.source.holds(page)
    {
      return Ok(Arc::clone(&whole.text));
    }
  }
  let screen = match scope {
    PageRowsScope::EveryRow => None,
    PageRowsScope::FirstScreen { active } => Some(PageFirstScreen::of(page, active)),
  };
  let kept_rows: Option<Vec<Arc<SessionMetadata>>> =
    screen.as_ref().map(|screen| screen.kept.iter().map(|&index| Arc::clone(&page.rows[index])).collect());
  let selected_rows = kept_rows.as_deref().unwrap_or(page.rows.as_slice());
  let inputs = row_inputs(store, selected_rows, &page.derived, &page.sequence_fields, projection)?;
  let mut state = memo.state.lock().expect("the page row memo mutex is not poisoned");
  if let Some(whole) = &state.whole
    && whole.clear_unread.as_deref() == clear_unread
    && whole.active.as_deref() == active
    && whole.inputs == inputs
  {
    return Ok(Arc::clone(&whole.text));
  }
  let previous: HashMap<String, RowInputs> = state
    .whole
    .take()
    .map_or_else(HashMap::new, |whole| whole.inputs.into_iter().map(|row| (row.meta.id.clone(), row)).collect());
  let current: HashSet<&str> = inputs.iter().map(|row| row.meta.id.as_str()).collect();
  state.rows.retain(|session_id, _| current.contains(session_id.as_str()));
  let mut rows = Vec::with_capacity(inputs.len());
  for row in &inputs {
    let clear = clear_unread == Some(row.meta.id.as_str());
    let inputs_moved = previous.get(row.meta.id.as_str()) != Some(row);
    if !clear
      && !inputs_moved
      && let Some(row_text) = state.rows.get(row.meta.id.as_str())
    {
      rows.push(Arc::clone(row_text));
      continue;
    }
    #[cfg(test)]
    memo.renders.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let row_text = Arc::new(PageRowText::of(page_client_value(store, row, clear, projection, schema)?));
    if clear {
      // The active row renders without its unread mark, so its fields cannot answer an ordinary request. A row whose
      // inputs moved drops its older fields here, which keeps them from serving the next request with them.
      if inputs_moved {
        state.rows.remove(row.meta.id.as_str());
      }
    } else {
      state.rows.insert(row.meta.id.clone(), Arc::clone(&row_text));
    }
    rows.push(row_text);
  }
  let text = Arc::new(page_columns_text(&rows, screen.as_ref()));
  state.whole = Some(WholePageText {
    source: PageTextSource::of(page),
    inputs,
    clear_unread: clear_unread.map(str::to_string),
    active: active.map(str::to_string),
    text: Arc::clone(&text),
  });
  Ok(text)
}

/// One page row's fields as the JSON text each field carries.
///
/// The page's text is written by field, so a row splits into its fields once and the column writer reads the texts
/// back. The fragments hold about the bytes the row's object text held, so the memo's memory stays at one sidebar
/// text rather than a value tree of it.
struct PageRowText {
  fields: Vec<(String, String)>,
}

impl PageRowText {
  /// The fragments of one page row's value.
  fn of(value: Value) -> Self {
    let fields = value
      .as_object()
      .expect("a page row is a JSON object")
      .iter()
      .map(|(field, cell)| (field.clone(), charliebot_compat::jinja::tojson_text(cell)))
      .collect();
    Self { fields }
  }

  /// The JSON text of one field, or `None` when the row leaves the field out.
  fn cell(&self, field: &str) -> Option<&str> {
    self.fields.iter().find(|(name, _)| name == field).map(|(_, text)| text.as_str())
  }
}

/// The key under which the page declares the encoding of each column it encoded.
const COLUMN_ENCODING_KEY: &str = "$encoded";

/// The encoding of the page's `id` column: the ids' 16-byte UUID values travel in row order as one base64url string.
/// A home whose ids are not all canonical UUIDs carries one cell per row instead: the canonical UUID's 16 bytes in
/// base64url, any other id as `~` and its text.
const UUID_ENCODING: &str = "uuid-b64";

/// The encoding of the page's `updated_at` column: the first row carries its epoch minutes, and every later row the
/// difference from the row before it.
const DELTA_ENCODING: &str = "delta";

/// The page's `id` field, whose UUIDs travel in [`UUID_ENCODING`].
const ID_FIELD: &str = "id";

/// The key under which the page declares which rows its listing covers.
const FIRST_SCREEN_KEY: &str = "$screen";

/// The roots the sidebar shows per group before the reader asks for the rest.
///
/// The sidebar that renders these rows holds the same limit as `GROUP_SESSION_PREVIEW_LIMIT` in
/// `web/static/js/sidebar/groups.js`. The server picks the rows the page carries, so both sides hold the number: the
/// sidebar hides the roots past it, and the page carries no others. A drift costs a row of preview, never a row.
const SIDEBAR_PREVIEW_LIMIT: usize = 5;

/// The rows the chat page's sidebar paints before the reader expands a group, and the rows each group holds.
///
/// The sidebar groups its rows by the session's `group` and paints the first [`SIDEBAR_PREVIEW_LIMIT`] roots of
/// each group with their descendants. A root with a bound schedule task always shows, and the open session's root
/// and subtree always show. Every other row is hidden behind the group's "Show all" toggle, so the page does not
/// carry it: the reader's expansion fetches the full listing.
///
/// A row's group is its root's `group`, so a group's count is the sum of its roots' subtree sizes.
pub(crate) struct PageFirstScreen {
  /// Indices into the page's rows of the rows the first screen paints, in listing order.
  kept: Vec<usize>,
  /// The row count of each group's trees, keyed by the group key (the empty string for the keyless bucket).
  counts: std::collections::BTreeMap<String, usize>,
  /// Whether the page leaves rows out, which the sidebar reads as "Show all fetches the rest".
  partial: bool,
}

impl PageFirstScreen {
  fn of(page: &PageRows, active: Option<&str>) -> Self {
    let rows = page.rows();
    let index_of: HashMap<&str, usize> = rows.iter().enumerate().map(|(index, row)| (row.id.as_str(), index)).collect();
    // A row nests under its `task_parent_id` when the page lists that parent, as the sidebar's tree does.
    let parent: Vec<Option<usize>> =
      rows.iter().map(|row| row.task_parent_id.as_deref().and_then(|id| index_of.get(id).copied())).collect();
    let group: Vec<&str> = rows.iter().map(|row| row.group.as_deref().unwrap_or("")).collect();
    let has_task: Vec<bool> = rows
      .iter()
      .map(|row| {
        page
          .sequence_fields()
          .get(row.id.as_str())
          .and_then(|fields| fields.get(SCHEDULE_TASK))
          .is_some_and(|value| !is_falsy(value))
      })
      .collect();
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); rows.len()];
    for (index, parent) in parent.iter().enumerate() {
      if let Some(parent) = parent {
        children[*parent].push(index);
      }
    }
    let roots: Vec<usize> = (0..rows.len()).filter(|&index| parent[index].is_none()).collect();
    // Each row belongs to the root its parent chain reaches. A row no root reaches stays unowned, and the sidebar
    // does not paint it either: its renderer walks out from the roots.
    let mut root_of: Vec<Option<usize>> = vec![None; rows.len()];
    let mut root_sizes: HashMap<usize, usize> = HashMap::new();
    for &root in &roots {
      let mut size = 0;
      let mut stack = vec![root];
      while let Some(node) = stack.pop() {
        if root_of[node].is_some() {
          continue;
        }
        root_of[node] = Some(root);
        size += 1;
        stack.extend(children[node].iter().copied());
      }
      root_sizes.insert(root, size);
    }
    let active_root = active.and_then(|id| index_of.get(id).copied()).and_then(|node| root_of[node]);
    let mut group_roots: std::collections::BTreeMap<String, Vec<usize>> = std::collections::BTreeMap::new();
    for &root in &roots {
      group_roots.entry(group[root].to_string()).or_default().push(root);
    }
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    let mut kept_roots: Vec<bool> = vec![false; rows.len()];
    for (key, roots) in &group_roots {
      let mut rank = 0;
      let mut total = 0;
      for &root in roots {
        total += root_sizes.get(&root).copied().unwrap_or(0);
        let in_preview = !has_task[root] && rank < SIDEBAR_PREVIEW_LIMIT;
        if !has_task[root] {
          rank += 1;
        }
        if in_preview || has_task[root] || Some(root) == active_root {
          kept_roots[root] = true;
        }
      }
      counts.insert(key.clone(), total);
    }
    let kept: Vec<usize> =
      (0..rows.len()).filter(|&index| root_of[index].is_some_and(|root| kept_roots[root])).collect();
    Self { partial: kept.len() < rows.len(), kept, counts }
  }
}

/// The page's row text: one array per field, in row order, holding `null` where a row leaves the field out.
///
/// The page's rows repeat the same field names and value shapes on every row. A gzip deflator finds the repetition
/// inside one field's values before it finds any across interleaved rows, so grouping the values by field shrinks
/// the page: on the 7,160-session mock home the sidebar costs 453 KB of the page's compressed bytes as columns and
/// 506 KB as row objects.
///
/// Two columns carry an encoding the page declares under [`COLUMN_ENCODING_KEY`], because their plain text costs
/// bytes a client-side expansion removes. A session id is a canonical UUID, so [`UUID_ENCODING`] carries the ids' 16
/// bytes in row order as one base64url string: the 36-character text costs a gzip deflator's 22 bytes per row, one
/// base64url cell per row costs the same, and the string of them costs the 16 bytes' entropy. An `updated_at` is
/// epoch minutes in newest-first order, so [`DELTA_ENCODING`] carries the differences, which are small numbers the
/// deflator packs tightly.
///
/// A page row never carries a null value: the falsy fields it keeps are a name and a schedule flag, and
/// [`drop_falsy_fields`] leaves every other falsy field out before this text is built.
fn page_columns_text(rows: &[Arc<PageRowText>], screen: Option<&PageFirstScreen>) -> String {
  let mut fields: Vec<&str> = Vec::new();
  let mut seen: HashSet<&str> = HashSet::new();
  for row in rows {
    for (field, _) in &row.fields {
      if seen.insert(field.as_str()) {
        fields.push(field.as_str());
      }
    }
  }
  fields.sort_unstable();
  let mut text = String::new();
  text.push('{');
  text.push_str(&charliebot_compat::jinja::tojson_text(&Value::String(COLUMN_ENCODING_KEY.to_string())));
  text.push(':');
  text.push_str(&charliebot_compat::jinja::tojson_text(&Value::Object(Map::from_iter([
    (ID_FIELD.to_string(), Value::String(UUID_ENCODING.to_string())),
    (UPDATED_AT.to_string(), Value::String(DELTA_ENCODING.to_string())),
  ]))));
  if let Some(screen) = screen {
    text.push(',');
    text.push_str(&charliebot_compat::jinja::tojson_text(&Value::String(FIRST_SCREEN_KEY.to_string())));
    text.push(':');
    text.push_str(&charliebot_compat::jinja::tojson_text(&Value::Object(Map::from_iter([
      ("partial".to_string(), Value::Bool(screen.partial)),
      (
        "counts".to_string(),
        Value::Object(screen.counts.iter().map(|(key, count)| (key.clone(), Value::from(*count))).collect()),
      ),
    ]))));
  }
  for field in fields {
    text.push(',');
    text.push_str(&charliebot_compat::jinja::tojson_text(&Value::String(field.to_string())));
    text.push(':');
    match field {
      ID_FIELD => write_id_column(&mut text, rows),
      UPDATED_AT => {
        text.push('[');
        write_updated_at_column(&mut text, rows);
        text.push(']');
      }
      _ => {
        text.push('[');
        write_plain_column(&mut text, rows, field);
        text.push(']');
      }
    }
  }
  text.push('}');
  text
}

/// Write one column's cells as their field's JSON text, with `null` where a row leaves the field out.
fn write_plain_column(text: &mut String, rows: &[Arc<PageRowText>], field: &str) {
  for (index, row) in rows.iter().enumerate() {
    if index > 0 {
      text.push(',');
    }
    text.push_str(row.cell(field).unwrap_or("null"));
  }
}

/// Write the `id` column in [`UUID_ENCODING`]: every id's 16 bytes, in row order, as one base64url string. A home
/// whose rows are not all canonical UUIDs carries one cell per row instead, so a row's exact id text survives.
fn write_id_column(text: &mut String, rows: &[Arc<PageRowText>]) {
  let mut ids = Vec::with_capacity(rows.len() * 16);
  for row in rows {
    let Some(cell) = row.cell(ID_FIELD) else {
      write_id_cells(text, rows);
      return;
    };
    let id: String = serde_json::from_str(cell).expect("an id cell is JSON text");
    match canonical_uuid(&id) {
      Some(bytes) => ids.extend_from_slice(&bytes),
      None => {
        write_id_cells(text, rows);
        return;
      }
    }
  }
  text.push('"');
  text.push_str(&URL_SAFE_NO_PAD.encode(&ids));
  text.push('"');
}

/// Write the `id` column as one cell per row: a canonical UUID as its 16 bytes in base64url, any other id as `~` and
/// its text, and `null` where the row leaves the field out.
fn write_id_cells(text: &mut String, rows: &[Arc<PageRowText>]) {
  text.push('[');
  for (index, row) in rows.iter().enumerate() {
    if index > 0 {
      text.push(',');
    }
    let Some(cell) = row.cell(ID_FIELD) else {
      text.push_str("null");
      continue;
    };
    let id: String = serde_json::from_str(cell).expect("an id cell is JSON text");
    match canonical_uuid(&id) {
      Some(bytes) => {
        text.push('"');
        text.push_str(&URL_SAFE_NO_PAD.encode(bytes));
        text.push('"');
      }
      None => text.push_str(&charliebot_compat::jinja::tojson_text(&Value::String(format!("~{id}")))),
    }
  }
  text.push(']');
}

/// The 16 bytes one id carries, when its text is the canonical UUID spelling.
///
/// The store holds and the routes name an id by its exact text, so only the canonical spelling may travel encoded:
/// `Uuid::parse_str` also accepts uppercase, simple, braced and URN spellings, and decoding those would hand the page
/// a different id string.
fn canonical_uuid(id: &str) -> Option<[u8; 16]> {
  match uuid::Uuid::parse_str(id) {
    Ok(uuid) if uuid.to_string() == id => Some(*uuid.as_bytes()),
    _ => None,
  }
}

/// Write the `updated_at` column in [`DELTA_ENCODING`]: the first value as it stands, then each difference.
fn write_updated_at_column(text: &mut String, rows: &[Arc<PageRowText>]) {
  let mut previous: Option<i64> = None;
  for (index, row) in rows.iter().enumerate() {
    if index > 0 {
      text.push(',');
    }
    let Some(cell) = row.cell(UPDATED_AT) else {
      text.push_str("null");
      continue;
    };
    let value: i64 = cell.parse().expect("an updated_at cell is an integer");
    text.push_str(&previous.map_or(value, |previous| value - previous).to_string());
    previous = Some(value);
  }
}

/// One page row's value as its client reads it: every falsy field left out, then the fields it never reads, then the
/// values in the form the client uses.
///
/// The session value the row starts from is the caller's [`RowSchema`]. The test's expected-value helper builds its
/// compare values through this function, so it compares a text against the construction the route serves.
fn page_client_value(
  store: &Store,
  row: &RowInputs,
  clear_unread: bool,
  projection: SidebarProjection,
  schema: RowSchema,
) -> StoreResult<Value> {
  let mut value = page_row_value(store, row, clear_unread, projection, schema)?;
  drop_falsy_fields(&mut value);
  drop_unread_fields(&mut value);
  compact_page_fields(&mut value, row.meta.updated_at);
  Ok(value)
}

/// One row's value: the session's metadata with its sidebar and controller fields.
fn page_row_value(
  store: &Store,
  row: &RowInputs,
  clear_unread: bool,
  projection: SidebarProjection,
  schema: RowSchema,
) -> StoreResult<Value> {
  let mut value = schema.value(store, &row.meta)?;
  apply_sidebar_fields(&mut value, row, projection);
  apply_listing_fields(&mut value, row.listing.clone().unwrap_or_default());
  if clear_unread {
    value
      .as_object_mut()
      .expect("a session metadata value is an object")
      .insert(HAS_UNREAD.to_string(), Value::Bool(false));
  }
  Ok(value)
}

/// Fields the page keeps even when their value is falsy.
///
/// The client reads these values themselves rather than their truth: it renders `name` literally, so an absent name
/// prints "undefined" in the sidebar, and it draws a schedule as disabled only for `schedule_enabled` false, so an
/// absent value would draw the schedule as enabled.
const KEPT_FALSY_FIELDS: [&str; 2] = ["name", SCHEDULE_ENABLED];

/// Leave out every field the page's client reads as falsy: null, false, zero and the empty string.
///
/// The page carries each row's whole metadata, and most sessions leave most of its fields at their default: on the
/// 7,160-session mock home a row keeps 10 of its 49 fields. The client reads a missing field as undefined, which
/// every falsy test (a condition, `||`, `!!`, `|| 0`) treats like null, false or zero, so an omitted field changes no
/// decision it takes. The sidebar text falls from 9.2 MB to 2.0 MB, and the page body from 695 KB to 577 KB
/// compressed.
fn drop_falsy_fields(value: &mut Value) {
  let object = value.as_object_mut().expect("a session metadata value is an object");
  object.retain(|field, value| KEPT_FALSY_FIELDS.contains(&field.as_str()) || !is_falsy(value));
}

/// The fields of a page row that no page client reads: the session's creation time and its stored metadata schema
/// version.
///
/// The page's rows feed the sidebar and the session views under `web/static/js`, and none of them reads either
/// field: the tree listing orders and pages by `created_at` on the server, and `schema_version` is the store's own
/// compatibility marker. On the 7,160-session mock home the two fields are 365,160 bytes of the page's 2,347,241-byte
/// sidebar text.
const UNREAD_PAGE_FIELDS: [&str; 2] = ["created_at", SCHEMA_VERSION_FIELD];

/// Leave out the fields no page client reads, and an empty `round_ratings`.
///
/// A row's `round_ratings` is an object of rounds the user rated, so a session with ratings keeps it. The page's
/// client reads the field as `session.round_ratings || {}`, which draws an absent field exactly like an empty map;
/// on the mock home every session's map is empty, and the field's 121,720 bytes of the sidebar text carry no value.
fn drop_unread_fields(value: &mut Value) {
  let object = value.as_object_mut().expect("a session metadata value is an object");
  object.retain(|field, value| {
    if UNREAD_PAGE_FIELDS.contains(&field.as_str()) {
      return false;
    }
    !(field.as_str() == ROUND_RATINGS && value.as_object().is_some_and(|ratings| ratings.is_empty()))
  });
}

/// The page's own presence and form of the row values the client fixes.
///
/// The page lists active sessions only ([`active_page_rows`] reads [`SessionStatus::Active`]), so every page row's
/// `status` is `active`; the sidebar reads an absent status as active. A task-tree row's `work_state` paints an
/// indicator only for `waiting`, and `idle` is the client's own default, so an idle row drops it while any other
/// state stays.
///
/// The client formats a row's time with `relativeTime` at minute precision and sorts by the same instant, so epoch
/// minutes carry everything it reads. On the 7,160-session mock home the ISO text costs 54 KB of the sidebar's
/// compressed bytes and epoch minutes cost 25 KB.
fn compact_page_fields(value: &mut Value, updated_at: UtcDateTime) {
  let object = value.as_object_mut().expect("a session metadata value is an object");
  object.remove("status");
  if object.get(WORK_STATE).and_then(Value::as_str) == Some("idle") {
    object.remove(WORK_STATE);
  }
  object.insert(UPDATED_AT.to_string(), Value::from(updated_at.into_inner().unix_timestamp().div_euclid(60)));
}

/// The session-metadata field holding the session's last update, in the page's epoch-minute form.
const UPDATED_AT: &str = "updated_at";

/// Whether *value* is a value the page's client reads as falsy: null, false, zero or the empty string.
///
/// An empty array or object stays, because JavaScript reads it as truthy while a missing field is undefined.
fn is_falsy(value: &Value) -> bool {
  match value {
    Value::Null => true,
    Value::Bool(flag) => !*flag,
    Value::Number(number) => number.as_f64().is_some_and(|number| number == 0.0),
    Value::String(text) => text.is_empty(),
    Value::Array(_) | Value::Object(_) => false,
  }
}

/// Render a page's sidebar rows with their controller fields and route-local body and gzip memos.
///
/// The page hands out the corpus as the listing memo's own allocation, so its row and overlay identities are the
/// memo's, and a repeat request proves the corpus unchanged with one pointer comparison.
pub fn sessions_list_page_response(
  request: &Request<Body>,
  store: &Store,
  page: &PageRows,
  memos: &SessionsListMemos,
) -> Response {
  sessions_list_response(request, store, page.rows(), Some(&page.rows), page.derived(), page.sequence_fields(), memos)
}

/// Render sidebar rows with their controller fields and route-local body and gzip memos.
///
/// `shared` names the allocation a memoized corpus was handed out as, when the caller holds one; a route that filters
/// the corpus into a list of its own passes `None` and the source compares row by row.
pub fn sessions_list_response(
  request: &Request<Body>,
  store: &Store,
  rows: &[Arc<SessionMetadata>],
  shared: Option<&Arc<Vec<Arc<SessionMetadata>>>>,
  derived: &Arc<DerivedSidebarMap>,
  sequence_fields: &HashMap<String, Map<String, Value>>,
  memos: &SessionsListMemos,
) -> Response {
  let projection = LISTING_PROJECTION;
  let wants_gzip = request
    .headers()
    .get(header::ACCEPT_ENCODING)
    .and_then(|value| value.to_str().ok())
    .is_some_and(|value| value.contains("gzip"));
  let mut state = memos.state.lock().expect("sessions list memo mutex is not poisoned");
  // The corpus check answers the common request before it walks every row's inputs. It proves every input the body
  // reads still holds, so the body it returns is the body a rebuild would render.
  let source_holds =
    state.whole_body.as_mut().is_some_and(|whole| whole.source.holds(rows, shared, derived, sequence_fields));
  if !source_holds {
    let inputs = match row_inputs(store, rows, derived, sequence_fields, projection) {
      Ok(inputs) => inputs,
      Err(error) => return charliebot_compat::responses::internal_error("RuntimeError", error.to_string()),
    };
    // As `_sessions_list_response`: an unchanged corpus with unchanged overlays serves the rendered body again and
    // renders no row.
    let unchanged = state.whole_body.as_ref().is_some_and(|whole| whole.inputs == inputs);
    if unchanged {
      let whole = state.whole_body.as_mut().expect("the body is rendered when the inputs are unchanged");
      whole.source = ListingSource::of(rows, shared, derived, sequence_fields);
    } else {
      #[cfg(test)]
      {
        state.render_calls += inputs.len();
      }
      let values = match inputs.iter().map(|row| render_row(store, row, projection)).collect::<StoreResult<Vec<_>>>() {
        Ok(values) => values,
        Err(error) => return charliebot_compat::responses::internal_error("RuntimeError", error.to_string()),
      };
      let body = Bytes::from(charliebot_compat::json::fast_json_bytes(&Value::Array(values)));
      state.whole_body = Some(WholeListingBody {
        source: ListingSource::of(rows, shared, derived, sequence_fields),
        inputs,
        body,
        gzip: None,
      });
    }
  }
  let whole = state.whole_body.as_ref().expect("the body is rendered when the inputs changed");
  if !wants_gzip {
    return json_bytes_response(whole.body.clone(), false);
  }
  if let Some(compressed) = &whole.gzip {
    return json_bytes_response(compressed.clone(), true);
  }
  let compressed = Bytes::from(charliebot_compat::compression::gzip_memoized_body(&whole.body));
  state.whole_body.as_mut().expect("the body is rendered when the inputs changed").gzip = Some(compressed.clone());
  json_bytes_response(compressed, true)
}

/// A sidebar listing's column text and its gzip body.
///
/// The Workspace filter refetches the listing on every session or task-tree change, and the page already carries the
/// same rows in the column form [`active_page_json_text`] writes. The compact response serves that text in place of a
/// row array: the row texts it joins are the page's own fields, so the sidebar decodes it with the same
/// `expandSessionColumns` the page's inline listing takes. Its gzip memo reuses the previous body while the text
/// keeps its allocation, and the row memo re-renders only the rows a corpus change moved.
#[derive(Default)]
pub struct CompactListingMemo {
  rows: PageRowsMemo,
  body: Mutex<Option<CompactListingBody>>,
}

struct CompactListingBody {
  text: Arc<String>,
  gzip: Bytes,
}

impl CompactListingMemo {
  /// The compact listing for this corpus, gzipped when the request accepts gzip.
  pub fn response(&self, request: &Request<Body>, store: &Store, page: &PageRows) -> Response {
    let text = match page_json_text(
      store,
      page,
      None,
      PageRowsScope::EveryRow,
      &self.rows,
      LISTING_PROJECTION,
      RowSchema::Listing,
    ) {
      Ok(text) => text,
      Err(error) => return charliebot_compat::responses::internal_error("RuntimeError", error.to_string()),
    };
    let wants_gzip = request
      .headers()
      .get(header::ACCEPT_ENCODING)
      .and_then(|value| value.to_str().ok())
      .is_some_and(|value| value.contains("gzip"));
    if !wants_gzip {
      return json_bytes_response(Bytes::copy_from_slice(text.as_bytes()), false);
    }
    let mut body = self.body.lock().expect("the compact listing memo mutex is not poisoned");
    if let Some(cached) = body.as_ref()
      && Arc::ptr_eq(&cached.text, &text)
    {
      return json_bytes_response(cached.gzip.clone(), true);
    }
    let gzip = Bytes::from(charliebot_compat::compression::gzip_memoized_body(text.as_bytes()));
    *body = Some(CompactListingBody { text: Arc::clone(&text), gzip: gzip.clone() });
    json_bytes_response(gzip, true)
  }
}

/// The ids a row-refresh request names, or `None` for a full-list request.
pub fn row_refresh_ids(request: &Request<Body>) -> Option<Vec<String>> {
  crate::route_support::Query::new(request).get("ids").map(crate::api::parse_session_ids)
}

/// The row refresh body: the compact column text of the rows of `page` that `ids` name, and the ids the page no longer
/// holds, as `{"sessions": <column text>, "gone": [...]}`.
///
/// The rows take the full listing's compact form, so the sidebar merges them into its painted list with the same
/// decoding and the same epoch-minute times. The row memo is local to this call: a refresh names a few rows, and the
/// whole-listing memo keys on the full page.
pub fn row_refresh_text(store: &Store, page: &PageRows, ids: &[String]) -> StoreResult<String> {
  // The named rows come from the page's id index, so a refresh of a full Workspace does not walk the corpus; the ids
  // the page no longer holds come back in `gone`.
  let named = page.rows_named(ids);
  row_refresh_text_for(store, &named, page.derived(), &page.sequence_fields, ids, LISTING_PROJECTION)
}

/// The row refresh body of one sidebar view: the named rows the view lists, in the compact column form, plus the ids it
/// no longer lists.
///
/// Each view passes the projection its full list renders, so a refreshed row replaces a painted one field for field.
/// The row memo is local to this call: a refresh names a few rows, and the whole-listing memo keys on the full page.
pub fn row_refresh_text_for(
  store: &Store,
  rows: &[Arc<SessionMetadata>],
  derived: &Arc<DerivedSidebarMap>,
  sequence_fields: &HashMap<String, Map<String, Value>>,
  ids: &[String],
  projection: SidebarProjection,
) -> StoreResult<String> {
  let wanted: HashSet<&str> = ids.iter().map(String::as_str).collect();
  let rows: Vec<Arc<SessionMetadata>> = rows.iter().filter(|row| wanted.contains(row.id.as_str())).cloned().collect();
  let present: HashSet<String> = rows.iter().map(|row| row.id.clone()).collect();
  let gone: Vec<&str> = ids.iter().map(String::as_str).filter(|id| !present.contains(*id)).collect();
  let subset = PageRows {
    index: page_row_index(&rows),
    rows: Arc::new(rows),
    derived: Arc::clone(derived),
    sequence_fields: sequence_fields
      .iter()
      .filter(|(id, _)| present.contains(id.as_str()))
      .map(|(id, fields)| (id.clone(), fields.clone()))
      .collect(),
  };
  let columns = page_json_text(
    store,
    &subset,
    None,
    PageRowsScope::EveryRow,
    &PageRowsMemo::default(),
    projection,
    RowSchema::Listing,
  )?;
  let gone = serde_json::to_string(&gone).expect("a list of session ids serializes");
  Ok(format!(r#"{{"sessions":{columns},"gone":{gone}}}"#))
}

/// The response key of the chat-thread flag: true for a session whose metadata names a chat thread.
pub const THREAD_SESSION: &str = "thread_session";

/// Render metadata as the response model does, excluding `task.goal`. The `thread_session` flag names a chat thread
/// session, so the web client never reads the platform origin fields itself.
pub(crate) fn metadata_value(store: &Store, meta: &SessionMetadata) -> StoreResult<Value> {
  let mut value = store.session_metadata_value(meta);
  let object = value.as_object_mut().expect("session metadata serializes to an object");
  if let Some(task) = object.get_mut("task").and_then(Value::as_object_mut) {
    task.remove("goal");
  }
  object.insert(THREAD_SESSION.to_string(), Value::Bool(is_thread_session(meta)));
  Ok(value)
}

/// Remove schedule model fields and apply the controller's row fields.
pub fn apply_listing_fields(dump: &mut Value, fields: Map<String, Value>) {
  let object = dump.as_object_mut().expect("session metadata serializes to an object");
  for field in SCHEDULE_FIELDS {
    object.remove(field);
  }
  object.insert(SCHEDULE_TASK.to_string(), Value::Null);
  object.extend(fields);
}

pub(crate) fn run_backend(store: &Store, meta: &SessionMetadata) -> StoreResult<Option<String>> {
  Ok(store.run_backend(&meta.id))
}

fn display_backend(store: &Store, meta: &SessionMetadata) -> StoreResult<Option<String>> {
  // Matches displayed_backend_id. The serialized run_backend field remains the
  // thinking_state-only value that the listing reads.
  if meta.profile == TaskProfile::Worker {
    return Ok(Some(store.run_backend(&meta.id).unwrap_or_else(|| meta.backend.clone())));
  }
  Ok(Some(meta.backend.clone()))
}

pub(crate) fn listed_values(
  store: &Store,
  rows: &[Arc<SessionMetadata>],
  derived: &DerivedSidebarMap,
  sequence_fields: &HashMap<String, Map<String, Value>>,
  projection: SidebarProjection,
) -> StoreResult<Vec<Value>> {
  row_inputs(store, rows, derived, sequence_fields, projection)?
    .iter()
    .map(|row| render_row(store, row, projection))
    .collect()
}

/// Read every input that the rows of one listing render from.
fn row_inputs(
  store: &Store,
  rows: &[Arc<SessionMetadata>],
  derived: &DerivedSidebarMap,
  sequence_fields: &HashMap<String, Map<String, Value>>,
  projection: SidebarProjection,
) -> StoreResult<Vec<RowInputs>> {
  rows
    .iter()
    .map(|row| {
      Ok(RowInputs {
        meta: Arc::clone(row),
        derived: derived.get(&row.id).cloned(),
        thinking_since: store.busy_since(&row.id).map_or(Value::Null, |value| json!(value.to_str())),
        run_backend: if projection.include_run_backend { run_backend(store, row)? } else { None },
        // Only a worker row renders a display backend; another row keeps the backend its metadata carries.
        display_backend: if projection.display_backend && row.profile == TaskProfile::Worker {
          display_backend(store, row)?
        } else {
          None
        },
        listing: sequence_fields.get(&row.id).cloned(),
      })
    })
    .collect()
}

/// Render one listing row from its inputs.
fn render_row(store: &Store, row: &RowInputs, projection: SidebarProjection) -> StoreResult<Value> {
  let mut value = metadata_value(store, &row.meta)?;
  apply_sidebar_fields(&mut value, row, projection);
  apply_listing_fields(&mut value, row.listing.clone().unwrap_or_default());
  Ok(value)
}

fn apply_sidebar_fields(value: &mut Value, row: &RowInputs, projection: SidebarProjection) {
  let object = value.as_object_mut().expect("session metadata serializes to an object");
  let derived = row.derived.as_ref();
  if projection.include_running {
    object.insert(
      HAS_RUNNING_TASKS.to_string(),
      derived.and_then(|entry| entry.get(HAS_RUNNING_TASKS)).cloned().unwrap_or(Value::Bool(false)),
    );
    if let Some(work_state) = derived.and_then(|entry| entry.get(WORK_STATE)) {
      object.insert(WORK_STATE.to_string(), work_state.clone());
    }
  }
  if projection.include_trigger {
    object.insert(
      HAS_PENDING_TRIGGER.to_string(),
      derived.and_then(|entry| entry.get(HAS_PENDING_TRIGGER)).cloned().unwrap_or(Value::Bool(false)),
    );
    object.insert(
      PENDING_TRIGGER_COUNT.to_string(),
      derived.and_then(|entry| entry.get(PENDING_TRIGGER_COUNT)).cloned().unwrap_or(Value::from(0)),
    );
    object.insert(
      NEXT_TRIGGER_AT.to_string(),
      derived.and_then(|entry| entry.get(NEXT_TRIGGER_AT)).cloned().unwrap_or(Value::Null),
    );
  }
  if projection.include_plan {
    object.insert(
      HAS_PENDING_PLAN_APPROVAL.to_string(),
      derived.and_then(|entry| entry.get(HAS_PENDING_PLAN_APPROVAL)).cloned().unwrap_or(Value::Bool(false)),
    );
  }
  object.insert("thinking_since".to_string(), row.thinking_since.clone());
  if projection.include_run_backend {
    object.insert("run_backend".to_string(), row.run_backend.clone().map_or(Value::Null, Value::String));
  }
  if projection.display_backend
    && row.meta.profile == TaskProfile::Worker
    && let Some(backend) = &row.display_backend
  {
    object.insert("backend".to_string(), Value::String(backend.clone()));
  }
}

fn json_bytes_response(body: Bytes, gzip: bool) -> Response {
  let mut response = Response::builder()
    .status(StatusCode::OK)
    .header(header::CONTENT_TYPE, "application/json")
    .header(header::CONTENT_LENGTH, body.len());
  if gzip {
    response = response
      .header(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"))
      .header(header::VARY, HeaderValue::from_static("Accept-Encoding"));
  }
  response.body(Body::from(body)).expect("a JSON response builds")
}

#[cfg(test)]
mod tests {
  use super::*;
  use charliebot_store::metadata::{RoundRating, RoundRatings, SessionMetadata, TaskProfile};
  use serde_json::json;

  async fn list_body(
    features: &charliebot_registry::FeatureContext,
    rows: &[Arc<SessionMetadata>],
    derived: &Arc<DerivedSidebarMap>,
    sequence_fields: &HashMap<String, Map<String, Value>>,
    memos: &SessionsListMemos,
  ) -> Value {
    let request = Request::builder().uri("/api/sessions/").body(Body::empty()).unwrap();
    let response = sessions_list_response(&request, &features.store, rows, None, derived, sequence_fields, memos);
    serde_json::from_slice(&axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap()
  }

  #[tokio::test]
  async fn the_list_body_renders_again_when_a_row_input_moves() {
    let (_home, features) = crate::test_support::feature_context();
    let node = crate::test_support::create_node(&features.store, "memo-row", None, TaskProfile::Manager).await;
    let mut rows = vec![Arc::new(features.store.get_session(&node.id).await.unwrap().unwrap())];
    let memos = SessionsListMemos::default();
    let mut derived = Arc::new(DerivedSidebarMap::new());
    Arc::make_mut(&mut derived).insert(
      node.id.clone(),
      Arc::new(Map::from_iter([
        (HAS_RUNNING_TASKS.to_string(), Value::Bool(false)),
        (WORK_STATE.to_string(), json!("idle")),
      ])),
    );
    let sequence_fields = HashMap::new();

    let first = list_body(&features, &rows, &derived, &sequence_fields, &memos).await;
    assert_eq!(memos.state.lock().unwrap().render_calls, 1);
    assert_eq!(list_body(&features, &rows, &derived, &sequence_fields, &memos).await, first);
    assert_eq!(memos.state.lock().unwrap().render_calls, 1);
    assert_eq!(first[0]["work_state"], "idle");

    // A sidebar move rebuilds the derived map, so the memo follows the rows to their new overlays.
    let mut moved = (*derived).clone();
    moved.insert(
      node.id.clone(),
      Arc::new(Map::from_iter([
        (HAS_RUNNING_TASKS.to_string(), Value::Bool(false)),
        (WORK_STATE.to_string(), json!("waiting")),
      ])),
    );
    derived = Arc::new(moved);
    assert_eq!(list_body(&features, &rows, &derived, &sequence_fields, &memos).await[0]["work_state"], "waiting");
    assert_eq!(memos.state.lock().unwrap().render_calls, 2);

    let renamed = (*rows[0]).clone();
    rows[0] = Arc::new(SessionMetadata { name: "renamed".to_string(), ..renamed });
    assert_eq!(list_body(&features, &rows, &derived, &sequence_fields, &memos).await[0]["name"], "renamed");
    assert_eq!(memos.state.lock().unwrap().render_calls, 3);
  }

  /// The page's text expands to the row values the client reads, and a row keeps its value until an input moves.
  #[tokio::test]
  async fn page_text_expands_to_the_row_values_and_reuses_unchanged_rows() {
    let (_home, features) = crate::test_support::feature_context();
    let first = crate::test_support::create_node(&features.store, "page-text-a", None, TaskProfile::Manager).await;
    crate::test_support::create_node(&features.store, "page-text-b", None, TaskProfile::Manager).await;
    let memo = PageRowsMemo::default();

    let page = active_page_rows(&features.store, &features.registry).unwrap();
    let text = active_page_json_text(&features.store, &page, None, None, &memo).unwrap();
    assert_eq!(page_rows_of(text.as_str()), page_row_values(&features.store, &page, None));
    let rendered = memo.render_calls();
    assert_eq!(rendered, page.rows.len());

    let page = active_page_rows(&features.store, &features.registry).unwrap();
    assert_eq!(active_page_json_text(&features.store, &page, None, None, &memo).unwrap().as_str(), text.as_str());
    assert_eq!(memo.render_calls(), rendered);

    let page = active_page_rows(&features.store, &features.registry).unwrap();
    let active = active_page_json_text(&features.store, &page, Some(&first.id), Some(&first.id), &memo).unwrap();
    assert_eq!(page_rows_of(active.as_str()), page_row_values(&features.store, &page, Some(&first.id)));
    assert_eq!(memo.render_calls(), rendered + 1);
  }

  /// A row refresh answers the rows it names in listing order, once each, and names the ids the page lost.
  #[tokio::test]
  async fn a_row_refresh_answers_the_named_rows_in_listing_order() {
    let (_home, features) = crate::test_support::feature_context();
    for name in ["refresh-a", "refresh-b", "refresh-c"] {
      crate::test_support::create_node(&features.store, name, None, TaskProfile::Manager).await;
    }
    let page = active_page_rows(&features.store, &features.registry).unwrap();
    let listed: Vec<String> = page.rows().iter().map(|row| row.id.clone()).collect();
    // The ids arrive in the order the sidebar queued them, which need not be the page's order.
    let named = vec![listed[2].clone(), listed[0].clone(), listed[2].clone(), "no-such-session".to_string()];

    let text = row_refresh_text(&features.store, &page, &named).unwrap();
    let body: Value = serde_json::from_str(&text).expect("the refresh body is JSON");
    let sessions = serde_json::to_string(&body["sessions"]).expect("the refresh's rows serialize");
    let answered: Vec<String> = page_rows_of(&sessions)
      .into_iter()
      .map(|row| row["id"].as_str().expect("a row names its id").to_string())
      .collect();
    assert_eq!(answered, vec![listed[0].clone(), listed[2].clone()]);
    assert_eq!(body["gone"], json!(["no-such-session"]));

    // The route reads the controller fields of the rows it names alone.
    let page = active_page_rows_for(&features.store, &features.registry, &named).unwrap();
    assert!(page.sequence_fields().keys().all(|id| named.contains(id)));
  }

  /// One synthetic page: the rows in listing order, with the controller row fields the selection reads.
  fn synthetic_page(rows: Vec<SessionMetadata>, sequence_fields: HashMap<String, Map<String, Value>>) -> PageRows {
    let rows: Arc<Vec<Arc<SessionMetadata>>> = Arc::new(rows.into_iter().map(Arc::new).collect());
    PageRows { index: page_row_index(&rows), rows, derived: Arc::new(DerivedSidebarMap::new()), sequence_fields }
  }

  /// One session's metadata, carrying the fields the page's tree and group selection read.
  fn synthetic_session(id: &str, group: Option<&str>, parent: Option<&str>) -> SessionMetadata {
    serde_json::from_value(json!({
      "id": id,
      "name": id,
      "profile": "manager",
      "group": group,
      "task_parent_id": parent,
    }))
    .expect("synthetic session metadata")
  }

  fn kept_ids<'a>(screen: &PageFirstScreen, page: &'a PageRows) -> Vec<&'a str> {
    screen.kept.iter().map(|&index| page.rows[index].id.as_str()).collect()
  }

  /// The page carries the five roots each group paints with their descendants, and each group's whole row count.
  #[test]
  fn the_page_carries_the_roots_each_group_paints_and_each_groups_count() {
    let mut rows: Vec<SessionMetadata> =
      (1..=6).map(|n| synthetic_session(&format!("grouped-{n}"), Some("g1"), None)).collect();
    rows.push(synthetic_session("grouped-1-child", Some("g1"), Some("grouped-1")));
    rows.push(synthetic_session("keyless-1", None, None));
    let page = synthetic_page(rows, HashMap::new());

    let screen = PageFirstScreen::of(&page, None);

    assert_eq!(
      kept_ids(&screen, &page),
      vec!["grouped-1", "grouped-2", "grouped-3", "grouped-4", "grouped-5", "grouped-1-child", "keyless-1"]
    );
    assert_eq!(screen.counts.get("g1"), Some(&7));
    assert_eq!(screen.counts.get(""), Some(&1));
    assert!(screen.partial);
  }

  /// A root with a bound schedule task always shows, and so do the open session's root and subtree.
  #[test]
  fn a_scheduled_root_and_the_open_sessions_tree_stay_past_the_preview() {
    let mut rows: Vec<SessionMetadata> =
      (1..=8).map(|n| synthetic_session(&format!("plain-{n}"), None, None)).collect();
    rows.push(synthetic_session("plain-8-child", None, Some("plain-8")));
    let mut scheduled = Map::new();
    scheduled.insert(SCHEDULE_TASK.to_string(), json!("nightly"));
    let sequence_fields = HashMap::from([("plain-7".to_string(), scheduled)]);
    let page = synthetic_page(rows, sequence_fields);

    let screen = PageFirstScreen::of(&page, Some("plain-8"));

    assert_eq!(
      kept_ids(&screen, &page),
      vec!["plain-1", "plain-2", "plain-3", "plain-4", "plain-5", "plain-7", "plain-8", "plain-8-child"]
    );
    assert_eq!(screen.counts.get(""), Some(&9));
    assert!(screen.partial);
  }

  /// A sidebar with nothing hidden carries every row, and its text says the listing is complete.
  #[test]
  fn a_sidebar_with_nothing_hidden_carries_every_row_and_says_so() {
    let rows: Vec<SessionMetadata> = (1..=3).map(|n| synthetic_session(&format!("small-{n}"), None, None)).collect();
    let page = synthetic_page(rows, HashMap::new());

    let screen = PageFirstScreen::of(&page, None);
    assert_eq!(kept_ids(&screen, &page), vec!["small-1", "small-2", "small-3"]);
    assert_eq!(screen.counts.get(""), Some(&3));
    assert!(!screen.partial);

    let row = Arc::new(PageRowText::of(json!({"id": "small-1", "name": "small-1", "updated_at": 1})));
    let text = page_columns_text(&[row], Some(&screen));
    let columns: Value = serde_json::from_str(&text).expect("the page text is JSON");
    assert_eq!(columns[FIRST_SCREEN_KEY]["partial"], false);
    assert_eq!(columns[FIRST_SCREEN_KEY]["counts"][""], 3);
  }

  /// The compact listing carries the listing projection's columns, so the sidebar's expander restores the rows the
  /// full listing sends, and a repeat gzip request serves the compressed body the first one built.
  #[tokio::test]
  async fn the_compact_listing_carries_the_listing_columns_and_reuses_its_gzip_body() {
    let (_home, features) = crate::test_support::feature_context();
    crate::test_support::create_node(&features.store, "compact-a", None, TaskProfile::Manager).await;
    crate::test_support::create_node(&features.store, "compact-b", None, TaskProfile::Worker).await;
    let memo = CompactListingMemo::default();
    let page = active_page_rows(&features.store, &features.registry).unwrap();

    let identity = Request::builder().uri("/api/sessions/?compact=1").body(Body::empty()).unwrap();
    let response = memo.response(&identity, &features.store, &page);
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let text = std::str::from_utf8(&body).expect("the identity body is UTF-8");
    assert_eq!(
      page_rows_of(text),
      projection_row_values(&features.store, &page, None, LISTING_PROJECTION, RowSchema::Listing)
    );

    let gzip_request = Request::builder()
      .uri("/api/sessions/?compact=1")
      .header(header::ACCEPT_ENCODING, "gzip")
      .body(Body::empty())
      .unwrap();
    let first =
      axum::body::to_bytes(memo.response(&gzip_request, &features.store, &page).into_body(), usize::MAX).await.unwrap();
    let second =
      axum::body::to_bytes(memo.response(&gzip_request, &features.store, &page).into_body(), usize::MAX).await.unwrap();
    assert_eq!(first, second, "a repeat request serves the compressed body the memo holds");
    assert_eq!(memo.body.lock().unwrap().as_ref().unwrap().gzip, first);
  }

  /// A repeat page request serves the corpus the previous one built, and a session created after it joins the next
  /// page. The gate every sidebar-moving fixture holds (`test_support`) keeps the other tests of this binary out of
  /// the window the pointer assertions read. The test build never spawns the self-heal sweep.
  #[tokio::test]
  async fn a_repeat_page_read_serves_the_corpus_it_built_and_a_new_session_joins_it() {
    let (_home, features) = crate::test_support::feature_context();
    crate::test_support::create_node(&features.store, "corpus-first", None, TaskProfile::Manager).await;
    // The first read probes the new session and moves the sidebar generation, so it stores nothing; the read after it
    // resolves the overlays with every probe settled.
    let _ = active_page_rows(&features.store, &features.registry).unwrap();
    let page = active_page_rows(&features.store, &features.registry).unwrap();
    assert_eq!(page.rows.len(), 1);

    let again = active_page_rows(&features.store, &features.registry).unwrap();
    assert!(Arc::ptr_eq(&page.rows, &again.rows), "a repeat read serves the row vector it already built");
    assert!(Arc::ptr_eq(&page.derived, &again.derived), "a repeat read serves the overlays it already resolved");

    // A rebuilt index holds an equal archive list in a new allocation; the page still serves the corpus it built.
    features.store.invalidate_tree_index();
    let rebuilt = active_page_rows(&features.store, &features.registry).unwrap();
    assert!(
      Arc::ptr_eq(&page.rows, &rebuilt.rows),
      "a rebuilt index with the same archive ids serves the corpus the previous read built"
    );

    let late = crate::test_support::create_node(&features.store, "corpus-late", None, TaskProfile::Manager).await;

    let page = active_page_rows(&features.store, &features.registry).unwrap();
    assert!(
      page.rows.iter().any(|row| row.id == late.id),
      "the page corpus lists the session created after it was read"
    );
  }

  /// A repeat listing read serves the corpus the previous one built, and a session created after it joins the next
  /// listing. The gate every sidebar-moving fixture holds (`test_support`) keeps the other tests of this binary out
  /// of the window the pointer assertions read. The test build never spawns the self-heal sweep.
  #[tokio::test]
  async fn a_repeat_listing_read_serves_the_corpus_it_built_and_a_new_session_joins_it() {
    let (_home, features) = crate::test_support::feature_context();
    crate::test_support::create_node(&features.store, "active-first", None, TaskProfile::Manager).await;
    // The first read probes the new session and moves the sidebar generation, so it stores nothing; the read after it
    // resolves the overlays with every probe settled.
    let _ = active_listing_corpus(&features.store, &features.registry).unwrap();
    let corpus = active_listing_corpus(&features.store, &features.registry).unwrap();
    assert_eq!(corpus.rows.len(), 1);

    let again = active_listing_corpus(&features.store, &features.registry).unwrap();
    assert!(Arc::ptr_eq(&corpus.derived, &again.derived), "a repeat read serves the overlays it already resolved");
    assert!(Arc::ptr_eq(&corpus.rows[0], &again.rows[0]), "a repeat read serves the rows it already built");

    let late = crate::test_support::create_node(&features.store, "active-late", None, TaskProfile::Manager).await;

    let corpus = active_listing_corpus(&features.store, &features.registry).unwrap();
    assert!(
      corpus.rows.iter().any(|row| row.id == late.id),
      "the listing corpus lists the session created after it was read"
    );
  }

  /// The page corpus over given rows, with the controller fields that a listing supplies for each row.
  fn page_rows_over(rows: Vec<Arc<SessionMetadata>>) -> PageRows {
    let sequence_fields = rows.iter().map(|row| (row.id.clone(), Map::new())).collect();
    let rows: Arc<Vec<Arc<SessionMetadata>>> = Arc::new(rows);
    PageRows { index: page_row_index(&rows), rows, derived: Arc::new(DerivedSidebarMap::new()), sequence_fields }
  }

  /// A page row keeps its values and leaves out every field the client reads as falsy.
  #[tokio::test]
  async fn a_page_row_omits_the_falsy_fields_and_keeps_the_rest() {
    let (_home, features) = crate::test_support::feature_context();
    let meta = crate::test_support::create_node(&features.store, "falsy-row", None, TaskProfile::Manager).await;
    let memo = PageRowsMemo::default();
    let rows: Arc<Vec<Arc<SessionMetadata>>> = Arc::new(vec![Arc::new(meta.clone())]);
    let page = PageRows {
      index: page_row_index(&rows),
      rows,
      derived: Arc::new(DerivedSidebarMap::new()),
      sequence_fields: HashMap::new(),
    };

    let text = active_page_json_text(&features.store, &page, None, None, &memo).unwrap();
    let rows = page_rows_of(text.as_str());
    let row = rows[0].as_object().expect("a page row is an object");
    // The row writer fills these with null, false or zero, so the client would read them as falsy either way.
    for omitted in [SCHEDULE_TASK, "cc_session_id", "has_unread", "pending_trigger_count"] {
      assert!(!row.contains_key(omitted), "{omitted} is falsy and stays out of the page row");
    }
    // No page client reads these: the sidebar and session views never touch a row's creation time or schema
    // version, and an absent rating map reads like the empty one this row holds.
    for omitted in UNREAD_PAGE_FIELDS.iter().copied().chain([ROUND_RATINGS]) {
      assert!(!row.contains_key(omitted), "{omitted} is not a field the page's client reads");
    }
    assert_eq!(row["id"], json!(meta.id));
    assert_eq!(row["name"], json!("falsy-row"));
    assert_eq!(row["profile"], json!("manager"));
    // The page lists active sessions only, and the client reads an absent status as active.
    assert!(!row.contains_key("status"), "the page's rows carry the status its listing fixes");
    assert_eq!(
      row[UPDATED_AT],
      json!(meta.updated_at.into_inner().unix_timestamp().div_euclid(60)),
      "the page carries a row's time as epoch minutes"
    );
  }

  /// The page's own form of a row's time and defaults.
  #[test]
  fn a_page_row_time_is_epoch_minutes_and_an_idle_work_state_stays_out() {
    let updated_at = UtcDateTime::parse("2026-03-19T23:54:31Z").unwrap();
    let mut row = json!({"id": "synthetic", "status": "active", "work_state": "idle"});

    compact_page_fields(&mut row, updated_at);

    assert_eq!(row, json!({"id": "synthetic", "updated_at": 29_566_074_i64}));

    let mut moving = json!({"id": "synthetic", "status": "active", "work_state": "waiting"});
    compact_page_fields(&mut moving, updated_at);
    assert_eq!(moving["work_state"], json!("waiting"), "a moving row keeps the state its indicator paints");
  }

  /// A session the user rated keeps its ratings: only an empty map is left out of the page row.
  #[tokio::test]
  async fn a_page_row_keeps_the_round_ratings_it_holds() {
    let (_home, features) = crate::test_support::feature_context();
    let meta = crate::test_support::create_node(&features.store, "rated-row", None, TaskProfile::Manager).await;
    let mut round_ratings = RoundRatings::default();
    round_ratings.set("round-a".to_string(), Some(RoundRating::ThumbsUp));
    let page = page_rows_over(vec![Arc::new(SessionMetadata { round_ratings, ..meta })]);
    let memo = PageRowsMemo::default();

    let text = active_page_json_text(&features.store, &page, None, None, &memo).unwrap();
    let rows = page_rows_of(text.as_str());
    assert_eq!(rows[0][ROUND_RATINGS], json!({"round-a": "thumbs_up"}));
  }

  /// The fields the client reads by value stay when their value is falsy.
  #[test]
  fn the_page_keeps_a_falsy_name_and_a_disabled_schedule() {
    let mut row = json!({
      "id": "synthetic",
      "name": "",
      "schedule_enabled": false,
      "has_unread": false,
      "pending_trigger_count": 0,
      "next_trigger_at": null,
    });

    drop_falsy_fields(&mut row);

    assert_eq!(row, json!({"id": "synthetic", "name": "", "schedule_enabled": false}));
  }

  /// A row whose inputs move while it is the active session does not serve its older text to a later request.
  #[tokio::test]
  async fn a_moved_active_row_does_not_serve_its_older_text() {
    let (_home, features) = crate::test_support::feature_context();
    let meta = crate::test_support::create_node(&features.store, "moved-active-row", None, TaskProfile::Manager).await;
    let memo = PageRowsMemo::default();

    let page = page_rows_over(vec![Arc::new(meta.clone())]);
    active_page_json_text(&features.store, &page, None, None, &memo).unwrap();

    let renamed = SessionMetadata { name: "renamed while active".to_string(), has_unread: true, ..meta.clone() };
    let changed = page_rows_over(vec![Arc::new(renamed)]);
    let active_text = active_page_json_text(&features.store, &changed, Some(&meta.id), Some(&meta.id), &memo).unwrap();
    let active = page_rows_of(active_text.as_str());
    assert_eq!(active[0]["name"], "renamed while active");
    assert!(active[0].get("has_unread").is_none(), "the active row renders without its unread mark");

    let ordinary_text = active_page_json_text(&features.store, &changed, None, None, &memo).unwrap();
    let ordinary = page_rows_of(ordinary_text.as_str());
    assert_eq!(ordinary[0]["name"], "renamed while active");
    assert_eq!(ordinary[0]["has_unread"], true);
  }

  /// A run backend that lands after the page text was memoized still reaches the row, though the corpus pointers do
  /// not move. The backend change bumps the derived generation, which rebuilds the overlay map the memo compares.
  #[tokio::test]
  async fn a_changed_run_backend_renders_into_the_page_text() {
    let (_home, features) = crate::test_support::feature_context();
    let meta = crate::test_support::create_node(&features.store, "run-backend-row", None, TaskProfile::Manager).await;
    let memo = PageRowsMemo::default();

    let page = active_page_rows(&features.store, &features.registry).unwrap();
    let text = active_page_json_text(&features.store, &page, None, None, &memo).unwrap();
    let row_of = |text: &str| -> Value {
      page_rows_of(text).into_iter().find(|row| row["id"] == json!(meta.id)).expect("the created row is in the page")
    };
    assert_eq!(row_of(text.as_str())["run_backend"], Value::Null);

    features.store.note_run_backend(&meta.id, "synthetic-backend");

    let page = active_page_rows(&features.store, &features.registry).unwrap();
    let text = active_page_json_text(&features.store, &page, None, None, &memo).unwrap();
    assert_eq!(row_of(text.as_str())["run_backend"], "synthetic-backend");
  }

  /// A page opened without a session redirects to the row that the rendered page puts first.
  #[tokio::test]
  async fn the_redirect_row_is_the_rendered_page_first_row() {
    let (_home, features) = crate::test_support::feature_context();
    crate::test_support::create_node(&features.store, "redirect-first", None, TaskProfile::Manager).await;
    crate::test_support::create_node(&features.store, "redirect-second", None, TaskProfile::Manager).await;

    let page = active_page_rows(&features.store, &features.registry).unwrap();
    let first_row = page.rows.first().map(|row| row.id.as_str());
    assert_eq!(first_page_row_id(&features.store).unwrap().as_deref(), first_row);
  }

  /// The row values the page's client reads, as the page's columns expand to them.
  fn page_row_values(store: &Store, page: &PageRows, clear_unread: Option<&str>) -> Vec<Value> {
    projection_row_values(store, page, clear_unread, PAGE_PROJECTION, RowSchema::Page)
  }

  /// The row values one projection over one schema renders, as its columns expand to them.
  fn projection_row_values(
    store: &Store,
    page: &PageRows,
    clear_unread: Option<&str>,
    projection: SidebarProjection,
    schema: RowSchema,
  ) -> Vec<Value> {
    let inputs = row_inputs(store, page.rows.as_slice(), &page.derived, &page.sequence_fields, projection).unwrap();
    inputs
      .iter()
      .map(|row| page_client_value(store, row, clear_unread == Some(row.meta.id.as_str()), projection, schema).unwrap())
      .collect()
  }

  /// The row values the page's column text carries, with each declared encoding expanded.
  fn page_rows_of(text: &str) -> Vec<Value> {
    let columns: Map<String, Value> = serde_json::from_str(text).expect("the page text is a column object");
    let encodings = columns.get(COLUMN_ENCODING_KEY).and_then(Value::as_object).cloned().unwrap_or_default();
    let count = columns
      .iter()
      .filter(|(field, _)| !field.starts_with('$'))
      .map(|(_, cells)| match cells {
        Value::Array(cells) => cells.len(),
        // The `id` column's string form carries one 16-byte id per row.
        Value::String(blob) => URL_SAFE_NO_PAD.decode(blob).expect("the id blob is base64url").len() / 16,
        _ => 0,
      })
      .max()
      .unwrap_or(0);
    let mut rows = vec![Map::new(); count];
    for (field, cells) in columns {
      if field.starts_with('$') {
        continue;
      }
      let encoding = encodings.get(&field).and_then(Value::as_str);
      if encoding == Some(UUID_ENCODING)
        && let Some(blob) = cells.as_str()
      {
        let bytes = URL_SAFE_NO_PAD.decode(blob).expect("the id blob is base64url");
        for index in 0..count {
          let id =
            uuid::Uuid::from_slice(&bytes[index * 16..(index + 1) * 16]).expect("the id blob holds 16 bytes per row");
          rows[index].insert(field.clone(), Value::String(id.to_string()));
        }
        continue;
      }
      let mut running = 0i64;
      for (index, cell) in cells.as_array().expect("a column is an array").iter().enumerate() {
        if cell.is_null() {
          continue;
        }
        let value = match encoding {
          Some(UUID_ENCODING) => Value::String(expand_session_id(cell.as_str().expect("an id cell is a string"))),
          Some(DELTA_ENCODING) => {
            running += cell.as_i64().expect("a delta cell is an integer");
            Value::from(running)
          }
          _ => cell.clone(),
        };
        rows[index].insert(field.clone(), value);
      }
    }
    rows.into_iter().map(Value::Object).collect()
  }

  /// The session id a [`UUID_ENCODING`] cell carries: a UUID decoded from its base64url bytes, or a `~` text id.
  fn expand_session_id(cell: &str) -> String {
    let Some(encoded) = cell.strip_prefix('~') else {
      let bytes = URL_SAFE_NO_PAD.decode(cell).expect("a UUID id cell is base64url");
      return uuid::Uuid::from_slice(&bytes).expect("a UUID id cell holds 16 bytes").to_string();
    };
    encoded.to_string()
  }

  /// A home whose ids are all canonical UUIDs carries them as one base64url string of the 16-byte values, and the
  /// expansion restores each id's text.
  #[test]
  fn canonical_uuid_ids_travel_as_one_base64url_string() {
    let ids = ["e870f19c-96c6-40e7-8da7-71f02b2622e3", "994a3ec8-c930-4c2c-9009-5b75d17b823d"];
    let rows: Vec<Arc<PageRowText>> = ids
      .into_iter()
      .enumerate()
      .map(|(index, id)| Arc::new(PageRowText::of(json!({"id": id, "updated_at": 29_566_074 + index as i64}))))
      .collect();

    let text = page_columns_text(&rows, None);
    let bytes: Vec<u8> = ids.iter().flat_map(|id| *uuid::Uuid::parse_str(id).unwrap().as_bytes()).collect();
    assert_eq!(
      text,
      format!(
        r#"{{"$encoded":{{"id": "{UUID_ENCODING}", "updated_at": "{DELTA_ENCODING}"}},"id":"{}","updated_at":[29566074,1]}}"#,
        URL_SAFE_NO_PAD.encode(&bytes)
      )
    );
    assert_eq!(
      page_rows_of(&text),
      vec![json!({"id": ids[0], "updated_at": 29_566_074}), json!({"id": ids[1], "updated_at": 29_566_075})]
    );
  }

  /// A home whose ids are not all canonical UUIDs carries one cell per row: a canonical UUID as 22 base64url
  /// characters, any other id as `~` and its text, and the expansion restores both.
  #[test]
  fn a_canonical_uuid_id_travels_as_its_bytes_and_any_other_id_as_text() {
    let uuid = "e870f19c-96c6-40e7-8da7-71f02b2622e3";
    let rows: Vec<Arc<PageRowText>> = [uuid, "imported-chat", "E870F19C-96C6-40E7-8DA7-71F02B2622E3"]
      .into_iter()
      .enumerate()
      .map(|(index, id)| Arc::new(PageRowText::of(json!({"id": id, "updated_at": 29_566_074 + index as i64}))))
      .collect();

    let text = page_columns_text(&rows, None);
    assert_eq!(
      text,
      format!(
        r#"{{"$encoded":{{"id": "{UUID_ENCODING}", "updated_at": "{DELTA_ENCODING}"}},"id":["{}","~imported-chat","~E870F19C-96C6-40E7-8DA7-71F02B2622E3"],"updated_at":[29566074,1,1]}}"#,
        URL_SAFE_NO_PAD.encode(uuid::Uuid::parse_str(uuid).unwrap().as_bytes())
      )
    );
    assert_eq!(
      page_rows_of(&text),
      vec![
        json!({"id": uuid, "updated_at": 29_566_074}),
        json!({"id": "imported-chat", "updated_at": 29_566_075}),
        json!({"id": "E870F19C-96C6-40E7-8DA7-71F02B2622E3", "updated_at": 29_566_076})
      ]
    );
  }

  #[tokio::test]
  async fn backend_display_falls_back_to_metadata_without_reading_run_records() {
    let (_home, features) = crate::test_support::feature_context();
    let meta =
      crate::test_support::create_node(&features.store, "backend-read-count", None, TaskProfile::Manager).await;
    let record = features.store.run_metadata_path(&meta.id, "synthetic-run");
    std::fs::create_dir_all(record.parent().unwrap()).unwrap();
    std::fs::write(&record, b"this metadata must not be read").unwrap();

    let before = features.store.run_record_read_count();
    assert_eq!(run_backend(&features.store, &meta).unwrap(), None);
    assert_eq!(display_backend(&features.store, &meta).unwrap(), Some(meta.backend.clone()));
    assert_eq!(features.store.run_record_read_count() - before, 0);
  }

  #[test]
  fn listing_fields_remove_schedule_values_and_append_controller_fields() {
    let mut value = json!({"schedule_cron": null, "name": "Synthetic", "schedule_enabled": null});
    let mut fields = Map::new();
    fields.insert("schedule_cron".to_string(), json!("0 8 * * *"));
    fields.insert("sequence_state".to_string(), json!("ready"));
    apply_listing_fields(&mut value, fields);
    let mut expected = json!({
      "name": "Synthetic",
      "schedule_cron": "0 8 * * *",
      "sequence_state": "ready"
    });
    expected[SCHEDULE_TASK] = Value::Null;
    assert_eq!(value, expected);
  }

  #[test]
  fn response_projection_removes_only_task_goal() {
    let (_home, features) = crate::test_support::feature_context();
    let mut meta = SessionMetadata::new("synthetic", "Synthetic", TaskProfile::Manager);
    meta.task = Some(charliebot_store::metadata::TaskSpec {
      task_type: Some(charliebot_store::metadata::TaskType::Implement),
      goal: "private goal".to_string(),
      ..Default::default()
    });
    let value = metadata_value(&features.store, &meta).unwrap();
    assert!(value["task"].get("goal").is_none());
    assert_eq!(value["task"]["task_type"], "implement");
  }
}
