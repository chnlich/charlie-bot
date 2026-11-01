//! Session listing projections shared by sidebar routes.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex, Weak};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use charliebot_registry::Registry;
use charliebot_store::metadata::{SCHEDULE_FIELDS, SessionMetadata, SessionStatus, TaskProfile, WORK_STATE};
use charliebot_store::{SharedSubtreeRoots, Store, StoreResult};
use http::HeaderValue;
use serde_json::{Map, Value, json};

use crate::sidebar;
pub use crate::sidebar::DerivedSidebarMap;

#[derive(Clone, Copy)]
pub(crate) struct SidebarProjection {
  pub(crate) include_running: bool,
  pub(crate) include_trigger: bool,
  pub(crate) include_plan: bool,
  pub(crate) display_backend: bool,
  pub(crate) include_run_backend: bool,
}

#[derive(Default)]
struct SessionsListMemoState {
  whole_body: Option<WholeListingBody>,
  #[cfg(test)]
  render_calls: usize,
}

/// One rendered listing body, its corpus, its inputs, and the gzip form a request asked for.
///
/// The gzip form lives beside the body it compressed: a request that matches the inputs serves both without hashing or
/// comparing the body, which the memo would otherwise do to find the compressed bytes it already holds.
struct WholeListingBody {
  source: ListingSource,
  inputs: Vec<RowInputs>,
  body: Vec<u8>,
  gzip: Option<Vec<u8>>,
}

/// The corpus one listing body rendered from, compared before the per-row inputs are rebuilt.
///
/// The rows compare by pointer: the store hands out one shared, immutable row while its content holds, so another
/// pointer means other content while the same pointer means an equal row. The derived overlays compare by pointer
/// too: their builder keeps one map until a sidebar input moves, and every move bumps the generation that keys the
/// map, so the same map means the same overlays. The controllers build a fresh field map per request, so their
/// fields compare by value. Every other input a row reads from the store (busy state, run backend) moves through a
/// sidebar dirty mark, which also rebuilds the derived map.
struct ListingSource {
  rows: Vec<Arc<SessionMetadata>>,
  derived: Arc<DerivedSidebarMap>,
  sequence_fields: HashMap<String, Map<String, Value>>,
}

impl ListingSource {
  fn of(
    rows: &[Arc<SessionMetadata>],
    derived: &Arc<DerivedSidebarMap>,
    sequence_fields: &HashMap<String, Map<String, Value>>,
  ) -> Self {
    Self { rows: rows.to_vec(), derived: Arc::clone(derived), sequence_fields: sequence_fields.clone() }
  }

  fn holds(
    &self,
    rows: &[Arc<SessionMetadata>],
    derived: &Arc<DerivedSidebarMap>,
    sequence_fields: &HashMap<String, Map<String, Value>>,
  ) -> bool {
    Arc::ptr_eq(&self.derived, derived)
      && self.rows.len() == rows.len()
      && self.rows.iter().zip(rows.iter()).all(|(before, now)| Arc::ptr_eq(before, now))
      && self.sequence_fields == *sequence_fields
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
  /// The metadata compares by identity: the store hands out one shared, immutable row for as long as its content
  /// holds, so another pointer means other content while the same pointer means an equal row. Reading it by content
  /// instead compares every field of every row's metadata on each request of a listing or a page.
  ///
  /// The derived overlay compares by content, because a rebuilt overlay hands out new maps for rows whose derived
  /// state did not move, and the memo should then still serve the row it rendered before.
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

/// The chat page's sidebar corpus: active, unscheduled sessions outside every owned subtree, with their overlays.
pub struct PageRows {
  rows: Arc<Vec<Arc<SessionMetadata>>>,
  derived: Arc<DerivedSidebarMap>,
  sequence_fields: HashMap<String, Map<String, Value>>,
}

/// The page corpus one request built, reused while every input that shapes it still holds.
///
/// Building the corpus walks every active session: the store filters the corpus, the sidebar resolves each row's
/// overlay, and the page drops the rows that an owned subtree holds. A repeat page request reads the same corpus, so
/// the memo answers it from the identities that would move if any of it changed. The listing generation names the
/// metadata corpus (the store numbers every rebuild, so the same number is the same rows in the same order), the
/// derived archive ids and the subtree roots name the two filters, the sidebar generation names the overlays, and the
/// repository identities name the store and registry the corpus came from. Both are held weakly: a dropped store's
/// corpus never answers another store's request.
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

/// Read the chat page's sidebar corpus without rendering a row.
pub fn active_page_rows(store: &Arc<Store>, registry: &Arc<Registry>) -> StoreResult<PageRows> {
  let (_, generation) = store.load_session_metas_snapshot_versioned(Some(SessionStatus::Active))?;
  let archived = store.derived_archived_ids()?;
  let roots = store.subtree_roots_shared()?;
  let sidebar_generation = charliebot_store::sidebar_state::derived_generation();
  let cached = {
    let memo = PAGE_CORPUS_MEMO.lock().expect("the page corpus memo mutex is not poisoned");
    memo
      .as_ref()
      .filter(|entry| entry.holds(store, registry, generation, &archived, &roots, sidebar_generation))
      .map(|entry| (Arc::clone(&entry.rows), Arc::clone(&entry.derived)))
  };
  if let Some((rows, derived)) = cached {
    let sequence_fields = store.sequence_listing_fields(&rows)?;
    return Ok(PageRows { rows, derived, sequence_fields });
  }
  let ListingCorpus { mut rows, derived, sequence_fields } = active_listing_corpus(store, registry)?;
  rows.retain(|row| outside_owned_subtrees(row, &roots));
  let rows = Arc::new(rows);
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
      derived: Arc::clone(&derived),
    });
  }
  Ok(PageRows { rows, derived, sequence_fields })
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

/// The page's whole sidebar text and the inputs it rendered from.
struct WholePageText {
  source: PageTextSource,
  inputs: Vec<RowInputs>,
  clear_unread: Option<String>,
  text: Arc<String>,
}

/// The corpus one page text rendered from, compared before the per-row inputs are rebuilt.
///
/// The rows compare by pointer: the store hands out one shared, immutable row while its content holds, so another
/// pointer means other content while the same pointer means an equal row. The derived overlays compare by pointer
/// too: their builder keeps one map until a sidebar input moves, and every move bumps the generation that keys the
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

  fn holds(&self, page: &PageRows) -> bool {
    Arc::ptr_eq(&self.derived, &page.derived)
      && self.rows.len() == page.rows.len()
      && self.rows.iter().zip(page.rows.iter()).all(|(before, now)| Arc::ptr_eq(before, now))
      && self.sequence_fields == page.sequence_fields
  }
}

/// The chat page's rendered sidebar texts.
///
/// Every page request renders the same rows from the same inputs, so the memo serves the whole sidebar text until one
/// of the inputs [`render_row_text`] reads moves. A row whose inputs did not move keeps its text across a rebuild, so
/// one changed row costs one row render rather than the whole sidebar. The memo holds the page's whole sidebar text,
/// about as much memory as one response body.
#[derive(Default)]
pub struct PageRowsMemo {
  state: Mutex<PageRowsMemoState>,
  #[cfg(test)]
  renders: std::sync::atomic::AtomicUsize,
}

#[derive(Default)]
struct PageRowsMemoState {
  rows: HashMap<String, String>,
  whole: Option<WholePageText>,
}

impl PageRowsMemo {
  #[cfg(test)]
  fn render_calls(&self) -> usize {
    self.renders.load(std::sync::atomic::Ordering::Relaxed)
  }
}

/// Render the chat page's sidebar rows as the Jinja `tojson` text of their array.
///
/// `clear_unread` names the active session: its row renders with `has_unread` false and is left out of the memo, so a
/// later request that names another active session does not serve the marked row.
pub fn active_page_json_text(
  store: &Store,
  page: &PageRows,
  clear_unread: Option<&str>,
  memo: &PageRowsMemo,
) -> StoreResult<Arc<String>> {
  // The corpus check answers the common request before it walks every row's inputs. It proves every input the text
  // reads still holds, so the text it returns is the text a rebuild would render.
  {
    let state = memo.state.lock().expect("the page row memo mutex is not poisoned");
    if let Some(whole) = &state.whole
      && whole.clear_unread.as_deref() == clear_unread
      && whole.source.holds(page)
    {
      return Ok(Arc::clone(&whole.text));
    }
  }
  let inputs = row_inputs(store, page.rows.as_slice(), &page.derived, &page.sequence_fields, PAGE_PROJECTION)?;
  let mut state = memo.state.lock().expect("the page row memo mutex is not poisoned");
  if let Some(whole) = &state.whole
    && whole.clear_unread.as_deref() == clear_unread
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
  let cached_bytes: usize = state.rows.values().map(String::len).sum();
  let mut text = String::with_capacity(2 + cached_bytes + 256 * inputs.len());
  text.push('[');
  for (index, row) in inputs.iter().enumerate() {
    if index > 0 {
      text.push_str(", ");
    }
    let clear = clear_unread == Some(row.meta.id.as_str());
    let inputs_moved = previous.get(row.meta.id.as_str()) != Some(row);
    if !clear
      && !inputs_moved
      && let Some(row_text) = state.rows.get(row.meta.id.as_str())
    {
      text.push_str(row_text);
      continue;
    }
    #[cfg(test)]
    memo.renders.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let row_text = render_row_text(store, row, clear)?;
    text.push_str(&row_text);
    if clear {
      // The active row renders without its unread mark, so its text cannot answer an ordinary request. A row whose
      // inputs moved drops its older text here, which keeps that text from serving the next request with them.
      if inputs_moved {
        state.rows.remove(row.meta.id.as_str());
      }
    } else {
      state.rows.insert(row.meta.id.clone(), row_text);
    }
  }
  text.push(']');
  let text = Arc::new(text);
  state.whole = Some(WholePageText {
    source: PageTextSource::of(page),
    inputs,
    clear_unread: clear_unread.map(str::to_string),
    text: Arc::clone(&text),
  });
  Ok(text)
}

/// One row as the page's JSON text, with its `has_unread` cleared for the active session.
///
/// Unlike [`render_row`], the page's rows keep `task.goal` (see `docs/contract/shapes.md`, "Listing rows").
fn render_row_text(store: &Store, row: &RowInputs, clear_unread: bool) -> StoreResult<String> {
  let mut value = store.session_metadata_value(&row.meta);
  apply_sidebar_fields(&mut value, row, PAGE_PROJECTION);
  apply_listing_fields(&mut value, row.listing.clone().unwrap_or_default());
  if clear_unread {
    value
      .as_object_mut()
      .expect("a session metadata value is an object")
      .insert("has_unread".to_string(), Value::Bool(false));
  }
  Ok(charliebot_compat::jinja::tojson_text(&value))
}

/// Render sidebar rows with their controller fields and route-local body and gzip memos.
pub fn sessions_list_response(
  request: &Request<Body>,
  store: &Store,
  rows: &[Arc<SessionMetadata>],
  derived: &Arc<DerivedSidebarMap>,
  sequence_fields: &HashMap<String, Map<String, Value>>,
  memos: &SessionsListMemos,
) -> Response {
  let projection = SidebarProjection {
    include_running: true,
    include_trigger: true,
    include_plan: true,
    display_backend: true,
    include_run_backend: false,
  };
  let wants_gzip = request
    .headers()
    .get(header::ACCEPT_ENCODING)
    .and_then(|value| value.to_str().ok())
    .is_some_and(|value| value.contains("gzip"));
  let mut state = memos.state.lock().expect("sessions list memo mutex is not poisoned");
  // The corpus check answers the common request before it walks every row's inputs. It proves every input the body
  // reads still holds, so the body it returns is the body a rebuild would render.
  let source_holds = state.whole_body.as_ref().is_some_and(|whole| whole.source.holds(rows, derived, sequence_fields));
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
      whole.source = ListingSource::of(rows, derived, sequence_fields);
    } else {
      #[cfg(test)]
      {
        state.render_calls += inputs.len();
      }
      let values = match inputs.iter().map(|row| render_row(store, row, projection)).collect::<StoreResult<Vec<_>>>() {
        Ok(values) => values,
        Err(error) => return charliebot_compat::responses::internal_error("RuntimeError", error.to_string()),
      };
      let body = charliebot_compat::json::fast_json_bytes(&Value::Array(values));
      state.whole_body =
        Some(WholeListingBody { source: ListingSource::of(rows, derived, sequence_fields), inputs, body, gzip: None });
    }
  }
  let whole = state.whole_body.as_ref().expect("the body is rendered when the inputs changed");
  if !wants_gzip {
    return json_bytes_response(whole.body.clone(), false);
  }
  if let Some(compressed) = &whole.gzip {
    return json_bytes_response(compressed.clone(), true);
  }
  let compressed = charliebot_compat::compression::gzip_memoized_body(&whole.body);
  state.whole_body.as_mut().expect("the body is rendered when the inputs changed").gzip = Some(compressed.clone());
  json_bytes_response(compressed, true)
}

/// Render metadata as the Python response model does, excluding `task.goal`.
pub(crate) fn metadata_value(store: &Store, meta: &SessionMetadata) -> StoreResult<Value> {
  let mut value = store.session_metadata_value(meta);
  if let Some(task) = value.get_mut("task").and_then(Value::as_object_mut) {
    task.remove("goal");
  }
  Ok(value)
}

/// Remove schedule model fields and apply the controller's row fields.
pub fn apply_listing_fields(dump: &mut Value, fields: Map<String, Value>) {
  let object = dump.as_object_mut().expect("session metadata serializes to an object");
  for field in SCHEDULE_FIELDS {
    object.remove(field);
  }
  object.insert("schedule_task".to_string(), Value::Null);
  object.extend(fields);
}

pub(crate) fn run_backend(store: &Store, meta: &SessionMetadata) -> StoreResult<Option<String>> {
  Ok(store.run_backend(&meta.id))
}

fn display_backend(store: &Store, meta: &SessionMetadata) -> StoreResult<Option<String>> {
  // Matches displayed_backend_id at src/runtime/api/sessions.py:71-74. The serialized run_backend field remains the
  // thinking_state-only value that session_listing.py:93 uses.
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
      "has_running_tasks".to_string(),
      derived.and_then(|entry| entry.get("has_running_tasks")).cloned().unwrap_or(Value::Bool(false)),
    );
    if let Some(work_state) = derived.and_then(|entry| entry.get(WORK_STATE)) {
      object.insert(WORK_STATE.to_string(), work_state.clone());
    }
  }
  if projection.include_trigger {
    object.insert(
      "has_pending_trigger".to_string(),
      derived.and_then(|entry| entry.get("has_pending_trigger")).cloned().unwrap_or(Value::Bool(false)),
    );
    object.insert(
      "pending_trigger_count".to_string(),
      derived.and_then(|entry| entry.get("pending_trigger_count")).cloned().unwrap_or(Value::from(0)),
    );
    object.insert(
      "next_trigger_at".to_string(),
      derived.and_then(|entry| entry.get("next_trigger_at")).cloned().unwrap_or(Value::Null),
    );
  }
  if projection.include_plan {
    object.insert(
      "has_pending_plan_approval".to_string(),
      derived.and_then(|entry| entry.get("has_pending_plan_approval")).cloned().unwrap_or(Value::Bool(false)),
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

fn json_bytes_response(body: Vec<u8>, gzip: bool) -> Response {
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
  use charliebot_store::metadata::{SessionMetadata, TaskProfile};
  use serde_json::json;

  async fn list_body(
    features: &charliebot_registry::FeatureContext,
    rows: &[Arc<SessionMetadata>],
    derived: &Arc<DerivedSidebarMap>,
    sequence_fields: &HashMap<String, Map<String, Value>>,
    memos: &SessionsListMemos,
  ) -> Value {
    let request = Request::builder().uri("/api/sessions/").body(Body::empty()).unwrap();
    let response = sessions_list_response(&request, &features.store, rows, derived, sequence_fields, memos);
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
      Arc::new(json!({"has_running_tasks": false, "work_state": "idle"}).as_object().unwrap().clone()),
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
      Arc::new(json!({"has_running_tasks": false, "work_state": "waiting"}).as_object().unwrap().clone()),
    );
    derived = Arc::new(moved);
    assert_eq!(list_body(&features, &rows, &derived, &sequence_fields, &memos).await[0]["work_state"], "waiting");
    assert_eq!(memos.state.lock().unwrap().render_calls, 2);

    let renamed = (*rows[0]).clone();
    rows[0] = Arc::new(SessionMetadata { name: "renamed".to_string(), ..renamed });
    assert_eq!(list_body(&features, &rows, &derived, &sequence_fields, &memos).await[0]["name"], "renamed");
    assert_eq!(memos.state.lock().unwrap().render_calls, 3);
  }

  /// The page's text is the Jinja `tojson` text of the row array, and a row keeps that text until an input moves.
  #[tokio::test]
  async fn page_text_matches_the_array_json_and_reuses_unchanged_rows() {
    let (_home, features) = crate::test_support::feature_context();
    let first = crate::test_support::create_node(&features.store, "page-text-a", None, TaskProfile::Manager).await;
    crate::test_support::create_node(&features.store, "page-text-b", None, TaskProfile::Manager).await;
    let memo = PageRowsMemo::default();

    let page = active_page_rows(&features.store, &features.registry).unwrap();
    let text = active_page_json_text(&features.store, &page, None, &memo).unwrap();
    assert_eq!(text.as_str(), page_array_text(&features.store, &page, None));
    let rendered = memo.render_calls();
    assert_eq!(rendered, page.rows.len());

    let page = active_page_rows(&features.store, &features.registry).unwrap();
    assert_eq!(active_page_json_text(&features.store, &page, None, &memo).unwrap().as_str(), text.as_str());
    assert_eq!(memo.render_calls(), rendered);

    let page = active_page_rows(&features.store, &features.registry).unwrap();
    let active = active_page_json_text(&features.store, &page, Some(&first.id), &memo).unwrap();
    assert_eq!(active.as_str(), page_array_text(&features.store, &page, Some(&first.id)));
    assert_eq!(memo.render_calls(), rendered + 1);
  }

  /// A repeat page request serves the corpus the previous one built, and a session created after it joins the next
  /// page. The gate every sidebar-moving fixture holds (`test_support`) keeps the other tests of this binary out of
  /// the window the pointer assertions read, and the test build never spawns the self-heal sweep.
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
  /// of the window the pointer assertions read, and the test build never spawns the self-heal sweep.
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
    PageRows { rows: Arc::new(rows), derived: Arc::new(DerivedSidebarMap::new()), sequence_fields }
  }

  /// A row whose controller owns no fields renders the values the row writer fills for that row.
  #[tokio::test]
  async fn a_page_row_without_controller_fields_renders_the_defaults() {
    let (_home, features) = crate::test_support::feature_context();
    let meta = crate::test_support::create_node(&features.store, "no-fields-row", None, TaskProfile::Manager).await;
    let memo = PageRowsMemo::default();
    let page = PageRows {
      rows: Arc::new(vec![Arc::new(meta)]),
      derived: Arc::new(DerivedSidebarMap::new()),
      sequence_fields: HashMap::new(),
    };

    let text = active_page_json_text(&features.store, &page, None, &memo).unwrap();
    let rows: Vec<Value> = serde_json::from_str(text.as_str()).unwrap();
    assert_eq!(rows[0]["schedule_task"], Value::Null);
  }

  /// A row whose inputs move while it is the active session does not serve its older text to a later request.
  #[tokio::test]
  async fn a_moved_active_row_does_not_serve_its_older_text() {
    let (_home, features) = crate::test_support::feature_context();
    let meta = crate::test_support::create_node(&features.store, "moved-active-row", None, TaskProfile::Manager).await;
    let memo = PageRowsMemo::default();

    let page = page_rows_over(vec![Arc::new(meta.clone())]);
    active_page_json_text(&features.store, &page, None, &memo).unwrap();

    let renamed = SessionMetadata { name: "renamed while active".to_string(), ..meta.clone() };
    let changed = page_rows_over(vec![Arc::new(renamed)]);
    let active_text = active_page_json_text(&features.store, &changed, Some(&meta.id), &memo).unwrap();
    let active: Vec<Value> = serde_json::from_str(active_text.as_str()).unwrap();
    assert_eq!(active[0]["name"], "renamed while active");
    assert_eq!(active[0]["has_unread"], false);

    let ordinary_text = active_page_json_text(&features.store, &changed, None, &memo).unwrap();
    let ordinary: Vec<Value> = serde_json::from_str(ordinary_text.as_str()).unwrap();
    assert_eq!(ordinary[0]["name"], "renamed while active");
  }

  /// A run backend that lands after the page text was memoized still reaches the row, though the corpus pointers do
  /// not move: the backend change bumps the derived generation, which rebuilds the overlay map the memo compares.
  #[tokio::test]
  async fn a_changed_run_backend_renders_into_the_page_text() {
    let (_home, features) = crate::test_support::feature_context();
    let meta = crate::test_support::create_node(&features.store, "run-backend-row", None, TaskProfile::Manager).await;
    let memo = PageRowsMemo::default();

    let page = active_page_rows(&features.store, &features.registry).unwrap();
    let text = active_page_json_text(&features.store, &page, None, &memo).unwrap();
    let row_of = |text: &str| -> Value {
      serde_json::from_str::<Vec<Value>>(text)
        .unwrap()
        .into_iter()
        .find(|row| row["id"] == json!(meta.id))
        .expect("the created row is in the page")
    };
    assert_eq!(row_of(text.as_str())["run_backend"], Value::Null);

    features.store.note_run_backend(&meta.id, "synthetic-backend");

    let page = active_page_rows(&features.store, &features.registry).unwrap();
    let text = active_page_json_text(&features.store, &page, None, &memo).unwrap();
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

  /// The bytes the page must carry: `tojson` of the row values, with the active session's unread mark cleared.
  fn page_array_text(store: &Store, page: &PageRows, clear_unread: Option<&str>) -> String {
    let inputs =
      row_inputs(store, page.rows.as_slice(), &page.derived, &page.sequence_fields, PAGE_PROJECTION).unwrap();
    let values: Vec<Value> = inputs
      .iter()
      .map(|row| {
        let mut value = store.session_metadata_value(&row.meta);
        apply_sidebar_fields(&mut value, row, PAGE_PROJECTION);
        apply_listing_fields(&mut value, row.listing.clone().unwrap_or_default());
        if clear_unread == Some(row.meta.id.as_str()) {
          value.as_object_mut().unwrap().insert("has_unread".to_string(), Value::Bool(false));
        }
        value
      })
      .collect();
    charliebot_compat::jinja::tojson_text(&Value::Array(values))
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
    assert_eq!(
      value,
      json!({
        "name": "Synthetic",
        "schedule_task": null,
        "schedule_cron": "0 8 * * *",
        "sequence_state": "ready"
      })
    );
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
