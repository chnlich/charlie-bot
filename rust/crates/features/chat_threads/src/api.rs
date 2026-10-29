//! `GET /api/sessions/chat-threads`: the sidebar Threads view's list (`src/features/chat_threads/api.py`).

use std::sync::{Arc, LazyLock};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::Request;
use axum::response::Response;
use charliebot_compat::responses;
use charliebot_compat::routing::get_only;
use charliebot_registry::FeatureContext;
use charliebot_session_api::listing::{self, SessionsListMemos};

use crate::sidebar::THREADS_VIEW;

/// The render memos of this route alone, held for the server's lifetime, so the Workspace list's memos never serve
/// the Threads body.
static CHAT_THREADS_MEMOS: LazyLock<SessionsListMemos> = LazyLock::new(SessionsListMemos::default);

/// The route table, built with the shared feature context.
pub(crate) fn routes(features: &Arc<FeatureContext>) -> Router {
  Router::new().route("/api/sessions/chat-threads", get_only(list_chat_threads)).with_state(Arc::clone(features))
}

/// Match the route family for the server's trailing-slash redirect.
pub(crate) fn matches(path: &str) -> bool {
  path == "/api/sessions/chat-threads"
}

/// The active corpus's rows that the `threads` view holds, newest first. The Workspace list is the complement over the
/// same corpus.
async fn list_chat_threads(State(features): State<Arc<FeatureContext>>, request: Request<Body>) -> Response {
  let listing::ListingCorpus { mut rows, derived, sequence_fields } =
    match listing::active_listing_corpus(&features.store, &features.registry) {
      Ok(corpus) => corpus,
      Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
    };
  let views = match features.store.view_subtree_roots() {
    Ok(views) => views,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  let threads = views.get(THREADS_VIEW);
  rows.retain(|row| threads.is_some_and(|members| members.contains_key(&row.id)));
  listing::sessions_list_response(&request, &features.store, &rows, &derived, &sequence_fields, &CHAT_THREADS_MEMOS)
}

#[cfg(test)]
mod tests {
  use std::collections::HashSet;
  use std::fs;
  use std::sync::Arc;

  use axum::Router;
  use axum::body::{Body, to_bytes};
  use axum::http::{Method, Request, StatusCode, header};
  use axum::response::Response;
  use charliebot_core::config::{CONFIG_FILENAME, CREDENTIALS_FILENAME};
  use charliebot_core::credentials::load_credentials;
  use charliebot_core::hot_reload::HotReload;
  use charliebot_core::metadata_slot::MetadataSlot;
  use charliebot_registry::context::{FeatureContext, StartupState};
  use charliebot_registry::{RegisterFn, Registry};
  use charliebot_store::Store;
  use charliebot_store::metadata::{SessionMetadata, SessionStatus, TaskProfile};
  use serde_json::{Value, json};
  use tempfile::TempDir;
  use tower::ServiceExt;

  const LISTING: (&str, RegisterFn) = ("runtime.session_api", charliebot_session_api::register);
  const CHAT_THREADS: (&str, RegisterFn) = ("src.features.chat_threads", crate::register);
  const SLACK: (&str, RegisterFn) = ("src.features.slack", charliebot_slack::register);
  const DISCORD: (&str, RegisterFn) = ("src.features.discord", charliebot_discord::register);
  /// The server's order: the session-api's listing and per-session routes, this package, then the Slack and Discord
  /// packages that own the origin slots (`src/app/registrations.py` lists them after the chat-threads package).
  const SERVER: [(&str, RegisterFn); 4] = [LISTING, CHAT_THREADS, SLACK, DISCORD];

  /// The server's composition in miniature: the packages register in the given order, and the app merges their
  /// routers in that order, as `charliebot-server`'s `app::build` does.
  struct Server {
    _home: TempDir,
    features: Arc<FeatureContext>,
  }

  impl Server {
    fn new(packages: &[(&'static str, RegisterFn)]) -> Server {
      let home = tempfile::tempdir().expect("a temporary home is available");
      let mut registry = Registry::new();
      for &(package, register) in packages {
        registry.register_package(package, register).expect("the package registers");
      }
      let registry = Arc::new(registry);
      let cfg = registry.load_config(home.path()).expect("the synthetic config loads");
      let store = Store::new(
        cfg,
        registry.metadata_slots(),
        charliebot_registry::store_hooks::store_hooks(Arc::clone(&registry)),
      )
      .expect("the store opens over the temporary home");
      let config_registry = Arc::clone(&registry);
      let config_home = home.path().to_path_buf();
      let credentials_home = config_home.clone();
      let features = Arc::new(FeatureContext {
        config: Arc::new(HotReload::new(
          home.path().join(CONFIG_FILENAME),
          "config_reload_failed",
          Box::new(move || config_registry.load_config(&config_home)),
        )),
        credentials: Arc::new(HotReload::new(
          home.path().join(CREDENTIALS_FILENAME),
          "credentials_reload_failed",
          Box::new(move || load_credentials(&credentials_home)),
        )),
        store: Arc::new(store),
        registry,
        startup: Arc::new(StartupState::default()),
      });
      Server { _home: home, features }
    }

    fn app(&self) -> Router {
      self
        .features
        .registry
        .routers()
        .iter()
        .fold(Router::new(), |router, route| router.merge((route.build)(&self.features)))
    }
  }

  /// The chat-thread corpus: a Discord thread, a Slack thread, a task under the Discord thread, a worker under that
  /// task, an archived Discord thread and a plain root.
  struct Corpus {
    thread: String,
    slack_thread: String,
    child: String,
    grandchild: String,
    archived_thread: String,
    plain: String,
  }

  fn session(id: &str, profile: TaskProfile, parent: Option<&str>) -> SessionMetadata {
    let mut meta = SessionMetadata::new(id, id, profile);
    meta.task_parent_id = parent.map(str::to_string);
    meta
  }

  fn write_session(store: &Store, slots: &[MetadataSlot], meta: &SessionMetadata) {
    let path = store.metadata_path(&meta.id);
    fs::create_dir_all(path.parent().expect("a session file has a directory"))
      .expect("the session directory is created");
    fs::write(path, meta.to_json_string(slots, true)).expect("the metadata is written");
  }

  /// Writes the corpus through the registered slots, so every file carries the origin fields as the store writes them.
  fn seed(features: &FeatureContext) -> Corpus {
    let slots = features.registry.metadata_slots();
    let store = &features.store;
    let mut thread = session("thread", TaskProfile::Manager, None);
    thread.set_extra("discord_origin", json!({"guild_id": "900000000000000010", "thread_id": "900000000000000030"}));
    let mut slack_thread = session("slack-thread", TaskProfile::Manager, None);
    slack_thread.set_extra("slack_origin", json!({"channel_id": "C0SYNTHETIC", "thread_ts": "1767600000.000100"}));
    let child = session("child", TaskProfile::Manager, Some("thread"));
    let grandchild = session("grandchild", TaskProfile::Worker, Some("child"));
    let mut archived_thread = session("archived-thread", TaskProfile::Manager, None);
    archived_thread
      .set_extra("discord_origin", json!({"guild_id": "900000000000000010", "thread_id": "900000000000000040"}));
    archived_thread.status = SessionStatus::Archived;
    let plain = session("plain", TaskProfile::Manager, None);
    for meta in [&thread, &slack_thread, &child, &grandchild, &archived_thread, &plain] {
      write_session(store, &slots, meta);
    }
    Corpus {
      thread: thread.id,
      slack_thread: slack_thread.id,
      child: child.id,
      grandchild: grandchild.id,
      archived_thread: archived_thread.id,
      plain: plain.id,
    }
  }

  async fn send(app: &Router, request: Request<Body>) -> Response {
    app.clone().oneshot(request).await.expect("the router is infallible")
  }

  fn get_request(uri: &str, gzip: bool) -> Request<Body> {
    let mut builder = Request::builder().method(Method::GET).uri(uri);
    if gzip {
      builder = builder.header(header::ACCEPT_ENCODING, "gzip");
    }
    builder.body(Body::empty()).expect("the request builds")
  }

  async fn bytes(response: Response) -> Vec<u8> {
    to_bytes(response.into_body(), usize::MAX).await.expect("the body reads").to_vec()
  }

  async fn json_list(response: Response) -> Vec<Value> {
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_slice(&bytes(response).await).expect("the body is a JSON list")
  }

  fn row_ids(rows: &[Value]) -> HashSet<String> {
    rows.iter().map(|row| row["id"].as_str().expect("a row has an id").to_string()).collect()
  }

  fn row_for<'a>(rows: &'a [Value], id: &str) -> &'a Value {
    rows.iter().find(|row| row["id"].as_str() == Some(id)).expect("the row is listed")
  }

  fn keys(row: &Value) -> HashSet<String> {
    row.as_object().expect("a row is an object").keys().cloned().collect()
  }

  #[tokio::test]
  async fn the_threads_view_roots_each_thread_session_and_its_descendants() {
    let server = Server::new(&SERVER);
    let corpus = seed(&server.features);

    let views = server.features.store.view_subtree_roots().expect("the views resolve");
    let threads = &views["threads"];
    assert_eq!(threads[&corpus.thread], corpus.thread);
    assert_eq!(threads[&corpus.slack_thread], corpus.slack_thread);
    assert_eq!(threads[&corpus.child], corpus.thread);
    assert_eq!(threads[&corpus.grandchild], corpus.thread);
    assert_eq!(threads[&corpus.archived_thread], corpus.archived_thread);
    assert!(!threads.contains_key(&corpus.plain));

    // The route lists the active members only: the archived thread session stays out.
    let rows = json_list(send(&server.app(), get_request("/api/sessions/chat-threads", false)).await).await;
    let expected = HashSet::from([corpus.thread, corpus.slack_thread, corpus.child, corpus.grandchild]);
    assert_eq!(row_ids(&rows), expected);
  }

  #[tokio::test]
  async fn the_workspace_and_threads_lists_partition_the_active_rows() {
    let server = Server::new(&SERVER);
    let corpus = seed(&server.features);
    let app = server.app();
    let workspace = json_list(send(&app, get_request("/api/sessions/", false)).await).await;
    let threads = json_list(send(&app, get_request("/api/sessions/chat-threads", false)).await).await;
    let workspace_ids = row_ids(&workspace);
    let thread_ids = row_ids(&threads);

    let active: HashSet<String> = server
      .features
      .store
      .list_sessions(Some(SessionStatus::Active), None, Some(false))
      .expect("the active rows list")
      .rows
      .iter()
      .map(|row| row.id.clone())
      .collect();
    assert!(workspace_ids.contains(&corpus.plain));
    assert!(workspace_ids.is_disjoint(&thread_ids));
    assert_eq!(&workspace_ids | &thread_ids, active);
    assert!(!thread_ids.contains(&corpus.plain));

    // One row shape: every Threads row carries the Workspace row's keys.
    let shape = keys(row_for(&workspace, &corpus.plain));
    for row in &threads {
      assert_eq!(keys(row), shape);
    }
  }

  #[tokio::test]
  async fn the_threads_route_serves_its_own_body_on_every_repeat() {
    let server = Server::new(&SERVER);
    seed(&server.features);
    let app = server.app();

    let workspace = bytes(send(&app, get_request("/api/sessions/", false)).await).await;
    let threads = bytes(send(&app, get_request("/api/sessions/chat-threads", false)).await).await;
    assert_ne!(workspace, threads);
    assert_eq!(bytes(send(&app, get_request("/api/sessions/", false)).await).await, workspace);
    assert_eq!(bytes(send(&app, get_request("/api/sessions/chat-threads", false)).await).await, threads);

    let gzip = send(&app, get_request("/api/sessions/chat-threads", true)).await;
    assert_eq!(gzip.headers()[header::CONTENT_ENCODING], "gzip");
    assert_eq!(gzip.headers()[header::VARY], "Accept-Encoding");
    let gzip_threads = bytes(gzip).await;
    assert_eq!(bytes(send(&app, get_request("/api/sessions/chat-threads", true)).await).await, gzip_threads);
    assert_ne!(bytes(send(&app, get_request("/api/sessions/", true)).await).await, gzip_threads);
  }

  #[tokio::test]
  async fn the_threads_route_wins_over_the_per_session_route() {
    let server = Server::new(&SERVER);
    let corpus = seed(&server.features);
    let app = server.app();

    let rows = json_list(send(&app, get_request("/api/sessions/chat-threads", false)).await).await;
    let expected = HashSet::from([corpus.thread.clone(), corpus.slack_thread, corpus.child, corpus.grandchild]);
    assert_eq!(row_ids(&rows), expected);

    // The session-api's per-session route still answers the ids the static path does not claim.
    let per_session = send(&app, get_request(&format!("/api/sessions/{}", corpus.thread), false)).await;
    assert_eq!(per_session.status(), StatusCode::OK);
    let body = serde_json::from_slice::<Value>(&bytes(per_session).await).expect("the body is JSON");
    assert_eq!(body["id"], json!(corpus.thread));

    // An API GET route does not answer HEAD: 405 with `allow: GET`.
    let head = Request::builder()
      .method(Method::HEAD)
      .uri("/api/sessions/chat-threads")
      .body(Body::empty())
      .expect("the request builds");
    let head = send(&app, head).await;
    assert_eq!(head.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(head.headers()[header::ALLOW], "GET");
  }

  #[tokio::test]
  async fn the_threads_view_exists_only_with_this_package() {
    let server = Server::new(&[LISTING]);
    let corpus = seed(&server.features);

    assert!(server.features.store.view_subtree_roots().expect("the views resolve").is_empty());
    let workspace = json_list(send(&server.app(), get_request("/api/sessions/", false)).await).await;
    assert!(row_ids(&workspace).contains(&corpus.thread));
  }
}
