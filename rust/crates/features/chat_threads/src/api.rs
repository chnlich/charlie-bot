//! `GET /api/sessions/chat-threads`: the sidebar Threads view's list.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use charliebot_compat::responses;
use charliebot_compat::routing::get_only;
use charliebot_registry::FeatureContext;
use charliebot_session_api::listing::{self, SessionsListMemos};
use charliebot_store::sidebar_state::DerivedSidebarMap;
use serde_json::{Map, Value, json};

use crate::sidebar::THREADS_VIEW;

/// The render memos of this route alone, held for the server's lifetime, so the Workspace list's memos never serve
/// the Threads body.
static CHAT_THREADS_MEMOS: LazyLock<SessionsListMemos> = LazyLock::new(SessionsListMemos::default);

/// The overlay an empty Threads view answers with. No row reads it, and one shared allocation keeps the listing
/// memo's source comparison a hit across the view's polls.
static EMPTY_DERIVED: LazyLock<Arc<DerivedSidebarMap>> = LazyLock::new(|| Arc::new(DerivedSidebarMap::new()));

/// The controller fields an empty Threads view answers with, shared as [`EMPTY_DERIVED`] is.
static EMPTY_SEQUENCE_FIELDS: LazyLock<HashMap<String, Map<String, Value>>> = LazyLock::new(HashMap::new);

/// The route table, built with the shared feature context.
pub(crate) fn routes(features: &Arc<FeatureContext>) -> Router {
  Router::new().route("/api/sessions/chat-threads", get_only(list_chat_threads)).with_state(Arc::clone(features))
}

/// Match the route family for the server's trailing-slash redirect.
pub(crate) fn matches(path: &str) -> bool {
  path == "/api/sessions/chat-threads"
}

/// The active corpus's rows that the `threads` view holds, newest first. The Workspace list is the complement over the
/// same corpus. A view that holds no row answers without building that corpus.
///
/// With `ids`, the route answers those named rows alone, as the sidebar's row refresh: the ids the view no longer holds
/// come back in `gone`, and the rest in the compact column form the full list's rows take.
async fn list_chat_threads(State(features): State<Arc<FeatureContext>>, request: Request<Body>) -> Response {
  let views = match features.store.view_subtree_roots() {
    Ok(views) => views,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  let threads = views.get(THREADS_VIEW);
  let query_ids = listing::row_refresh_ids(&request);
  // An empty Threads view holds no row, so no row's state feeds the answer. The Workspace corpus would be built,
  // polled and copied only for the view to discard every row, so the empty view answers from its own emptiness.
  if threads.is_none_or(HashMap::is_empty) {
    if let Some(ids) = query_ids {
      if ids.is_empty() {
        return responses::json(
          StatusCode::UNPROCESSABLE_ENTITY,
          &json!({"detail": "ids must name at least one session"}),
        );
      }
      let text = match listing::row_refresh_text_for(
        &features.store,
        &[],
        &EMPTY_DERIVED,
        &EMPTY_SEQUENCE_FIELDS,
        &ids,
        listing::LISTING_PROJECTION,
      ) {
        Ok(text) => text,
        Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
      };
      return responses::fast_json_rendered(StatusCode::OK, text.into_bytes());
    }
    return listing::sessions_list_response(
      &request,
      &features.store,
      &[],
      None,
      &EMPTY_DERIVED,
      &EMPTY_SEQUENCE_FIELDS,
      &CHAT_THREADS_MEMOS,
    );
  }
  let listing::ListingCorpus { mut rows, derived, sequence_fields } =
    match listing::active_listing_corpus(&features.store, &features.registry) {
      Ok(corpus) => corpus,
      Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
    };
  rows.retain(|row| threads.is_some_and(|members| members.contains_key(&row.id)));
  if let Some(ids) = query_ids {
    if ids.is_empty() {
      return responses::json(
        StatusCode::UNPROCESSABLE_ENTITY,
        &json!({"detail": "ids must name at least one session"}),
      );
    }
    let text = match listing::row_refresh_text_for(
      &features.store,
      &rows,
      &derived,
      &sequence_fields,
      &ids,
      listing::LISTING_PROJECTION,
    ) {
      Ok(text) => text,
      Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
    };
    return responses::fast_json_rendered(StatusCode::OK, text.into_bytes());
  }
  // The view filters the corpus into a list of its own, so the source keeps the row-by-row comparison.
  listing::sessions_list_response(
    &request,
    &features.store,
    &rows,
    None,
    &derived,
    &sequence_fields,
    &CHAT_THREADS_MEMOS,
  )
}
