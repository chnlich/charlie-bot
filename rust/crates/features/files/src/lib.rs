//! `src/features/files`: the file server at `/absolute_filepath/{path:path}`.
//!
//! The route answers `GET` and `HEAD` for any path the server process can read. The registered file views are asked
//! first. The path then resolves the way `pathlib.Path.resolve()` does, and the answer is a directory listing, a file,
//! or the error that the Python route raises for that path.

mod human_size;
mod listing;

use std::fs;
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, LazyLock};

use axum::Router;
use axum::extract::{Path, Request, State};
use axum::http::request::Parts;
use axum::response::Response;
use charliebot_compat::mimetypes::MimeTypes;
use charliebot_compat::responses;
use charliebot_compat::routing::get_and_head;
use charliebot_compat::static_files::{self, StaticDirectory};
use charliebot_core::paths;
use charliebot_registry::pages::HomeCard;
use charliebot_registry::{FeatureContext, Registry, RegistryError};
use http::{HeaderMap, Method, StatusCode, header};
use listing::ListingError;
use serde_json::json;

/// `file_urls.FILE_SERVER_MOUNTS[0]`: the prefix the route is mounted under, without its trailing slash.
pub const MOUNT: &str = charliebot_compat::file_urls::FILE_SERVER_MOUNT;

/// The `mimetypes` table of this process, read once as Python reads its table at first use.
static MIME_TYPES: LazyLock<MimeTypes> = LazyLock::new(MimeTypes::load);

/// `register` (`src/features/files/__init__.py`): the router, then the home card.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router(routes, matches)?;
  registry.home_card(HomeCard {
    name: "File browser",
    url: "/absolute_filepath/",
    description: "Browse any file on this host's filesystem.",
  })
}

/// The file server's routes, with the feature context as their state. The route paths repeat `MOUNT` as literals,
/// because `Router::route` takes its path as written.
fn routes(features: &Arc<FeatureContext>) -> Router {
  Router::new()
    .route("/absolute_filepath/", get_and_head(serve_root))
    .route("/absolute_filepath/{*path}", get_and_head(serve))
    .with_state(Arc::clone(features))
}

/// Whether a request path falls under the mount, whatever the method. For a request to the bare prefix, the server's
/// trailing-slash redirect asks this of the slashed path, which turns the 404 into a 307.
fn matches(path: &str) -> bool {
  path.strip_prefix(MOUNT).is_some_and(|rest| rest.starts_with('/'))
}

/// `GET /absolute_filepath/`: the listing of the root.
async fn serve_root(State(features): State<Arc<FeatureContext>>, request: Request) -> Response {
  let (parts, _) = request.into_parts();
  answer(&features, &parts, "").await
}

/// `GET /absolute_filepath/{path:path}`, where `path` is the decoded text below the mount.
async fn serve(State(features): State<Arc<FeatureContext>>, Path(path): Path<String>, request: Request) -> Response {
  let (parts, _) = request.into_parts();
  answer(&features, &parts, &path).await
}

/// The answer to `path`: the first registered file view that answers, else the file system.
async fn answer(features: &FeatureContext, parts: &Parts, path: &str) -> Response {
  for (_, view) in features.registry.file_views() {
    match view.view(features, parts, path).await {
      Ok(Some(response)) => return response,
      Ok(None) => {}
      Err(response) => return response,
    }
  }
  let (path, method, headers) = (path.to_string(), parts.method.clone(), parts.headers.clone());
  tokio::task::spawn_blocking(move || answer_from_disk(&path, &method, &headers))
    .await
    .expect("the file server's blocking work does not panic")
}

/// The answer below the file views, on the blocking pool: a directory listing, a file, or the error the Python route
/// raises for the path.
fn answer_from_disk(path: &str, method: &Method, headers: &HeaderMap) -> Response {
  // `(pathlib.Path("/") / path).resolve()`
  let fs_path = paths::realpath(&FsPath::new("/").join(path));
  let url_prefix = if path.is_empty() { MOUNT.to_string() } else { format!("{MOUNT}/{path}") };
  match listing::dir_listing_page(&fs_path, &url_prefix) {
    Ok(Some(page)) => return responses::html(page),
    Ok(None) => {}
    Err(ListingError::Missing) => return not_found(),
    Err(ListingError::PermissionDenied) => return permission_denied(),
    Err(ListingError::Failed { kind, detail }) => return responses::internal_error(kind, detail),
  }
  // Not a directory. `os.path.exists` decides between 404 and the file answer.
  match fs::metadata(&fs_path) {
    Err(_) => not_found(),
    Ok(metadata) if !metadata.is_file() => {
      responses::internal_error("RuntimeError", format!("File at path {} is not a file.", fs_path.display()))
    }
    Ok(_) => answer_file(&fs_path, method, headers),
  }
}

/// `FileResponse` for a regular file. Starlette's `FileResponse` answers only the `Range` headers, so the conditional
/// headers that `StaticFiles` reads are removed first; the static answer would otherwise send a 304 for them. The
/// static root is `/`, so the answer serves the resolved path wherever it lies.
fn answer_file(fs_path: &FsPath, method: &Method, headers: &HeaderMap) -> Response {
  let Some(route_path) = fs_path.to_str() else {
    return responses::internal_error("UnicodeEncodeError", format!("{fs_path:?} is not UTF-8"));
  };
  let mut headers = headers.clone();
  headers.remove(header::IF_NONE_MATCH);
  headers.remove(header::IF_MODIFIED_SINCE);
  static_files::serve(&StaticDirectory::new(PathBuf::from("/")), &MIME_TYPES, route_path, method, &headers)
}

fn not_found() -> Response {
  responses::json(StatusCode::NOT_FOUND, &json!({"detail": "Not found"}))
}

fn permission_denied() -> Response {
  responses::json(StatusCode::FORBIDDEN, &json!({"detail": "Permission denied"}))
}

#[cfg(test)]
mod tests {
  use std::os::unix::fs::PermissionsExt;

  use axum::body::to_bytes;
  use axum::http::Request as HttpRequest;
  use charliebot_core::config::CREDENTIALS_FILENAME;
  use charliebot_core::credentials::load_credentials;
  use charliebot_core::hot_reload::HotReload;
  use charliebot_registry::context::StartupState;
  use charliebot_registry::store_hooks;
  use charliebot_registry::wiring::FileView;
  use charliebot_store::Store;
  use futures_util::future::BoxFuture;
  use nix::sys::stat::Mode;

  use super::*;

  fn parts(method: Method, headers: &[(&str, &str)]) -> Parts {
    let mut builder = HttpRequest::builder().method(method).uri("/");
    for (name, value) in headers {
      builder = builder.header(*name, *value);
    }
    builder.body(()).expect("the request builds").into_parts().0
  }

  fn test_features(registry: Registry) -> (tempfile::TempDir, Arc<FeatureContext>) {
    let home = tempfile::tempdir().expect("a temporary home is available");
    let registry = Arc::new(registry);
    let config_value = registry.load_config(home.path()).expect("the default config loads");
    let store = Arc::new(
      Store::new(config_value.clone(), registry.metadata_slots(), store_hooks::store_hooks(Arc::clone(&registry)))
        .expect("the store opens over the temporary home"),
    );
    let config_registry = Arc::clone(&registry);
    let config_home = home.path().to_path_buf();
    let config = Arc::new(HotReload::new(
      config_value.config_file(),
      "files_test_config_reload_failed",
      Box::new(move || config_registry.load_config(&config_home)),
    ));
    let credentials_home = home.path().to_path_buf();
    let credentials = Arc::new(HotReload::new(
      home.path().join(CREDENTIALS_FILENAME),
      "files_test_credentials_reload_failed",
      Box::new(move || load_credentials(&credentials_home)),
    ));
    let features =
      Arc::new(FeatureContext { config, credentials, store, registry, startup: Arc::new(StartupState::default()) });
    (home, features)
  }

  async fn answer_text(features: &FeatureContext, parts: &Parts, path: &str) -> (StatusCode, String) {
    let response = answer(features, parts, path).await;
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.expect("the body reads");
    (status, String::from_utf8_lossy(&body).into_owned())
  }

  /// A path below a temp dir, as the decoded text the route receives after the mount.
  fn below(dir: &tempfile::TempDir, name: &str) -> String {
    dir.path().join(name).to_str().expect("the temp path is UTF-8").trim_start_matches('/').to_string()
  }

  #[test]
  fn the_mount_is_the_one_prefix_below_which_a_path_is_routed() {
    assert_eq!(MOUNT, "/absolute_filepath");
    assert!(matches("/absolute_filepath/"));
    assert!(matches("/absolute_filepath/tmp/x.txt"));
    assert!(!matches("/absolute_filepathx/"));
  }

  #[test]
  fn the_legacy_spellings_are_not_routed() {
    assert!(!matches("/files/x"));
    assert!(!matches("/file/x"));
  }

  #[test]
  fn the_bare_prefix_is_not_routed_but_its_slashed_form_is() {
    // The server's trailing-slash redirect asks `matches` of `/absolute_filepath/`, so the bare prefix gets a 307.
    assert!(!matches("/absolute_filepath"));
    assert!(matches("/absolute_filepath/"));
  }

  #[test]
  fn register_mounts_the_router_and_lists_the_home_card() {
    let mut registry = Registry::new();
    registry.register_package("src.features.files", register).expect("the package registers");
    assert_eq!(registry.routers().len(), 1);
    let cards = registry.home_cards();
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0].name, "File browser");
    assert_eq!(cards[0].url, "/absolute_filepath/");
    assert_eq!(cards[0].description, "Browse any file on this host's filesystem.");
  }

  struct Passes;

  impl FileView for Passes {
    fn view<'a>(
      &'a self,
      _features: &'a FeatureContext,
      _request: &'a Parts,
      _path: &'a str,
    ) -> BoxFuture<'a, Result<Option<Response>, Response>> {
      Box::pin(async { Ok(None) })
    }
  }

  struct Answers(&'static str);

  impl FileView for Answers {
    fn view<'a>(
      &'a self,
      _features: &'a FeatureContext,
      _request: &'a Parts,
      _path: &'a str,
    ) -> BoxFuture<'a, Result<Option<Response>, Response>> {
      Box::pin(async { Ok(Some(responses::plain_text(StatusCode::OK, self.0))) })
    }
  }

  fn passing_package(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.file_view("passes", Arc::new(Passes))
  }

  fn answering_package(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.file_view("answers", Arc::new(Answers("answered")))
  }

  #[tokio::test]
  async fn the_first_file_view_that_answers_ends_the_request() {
    let mut registry = Registry::new();
    registry.register_package("src.features.passes", passing_package).expect("registers");
    registry.register_package("src.features.answers", answering_package).expect("registers");
    let (_home, features) = test_features(registry);
    let (status, body) = answer_text(&features, &parts(Method::GET, &[]), "/no/such/file").await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, "answered"));
  }

  #[tokio::test]
  async fn a_missing_path_is_404_and_a_missing_path_under_a_file_is_404() {
    let dir = tempfile::tempdir().expect("a temp dir");
    fs::write(dir.path().join("plain.txt"), "x").expect("the file writes");
    let (_home, features) = test_features(Registry::new());
    let get = parts(Method::GET, &[]);
    let (status, body) = answer_text(&features, &get, &below(&dir, "gone.txt")).await;
    assert_eq!((status, body.as_str()), (StatusCode::NOT_FOUND, r#"{"detail":"Not found"}"#));
    let under_file = below(&dir, "plain.txt/gone.txt");
    let (status, body) = answer_text(&features, &get, &under_file).await;
    assert_eq!((status, body.as_str()), (StatusCode::NOT_FOUND, r#"{"detail":"Not found"}"#));
  }

  #[tokio::test]
  async fn an_unreadable_file_is_500_on_get_and_its_headers_answer_on_head() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let locked = dir.path().join("locked.bin");
    fs::write(&locked, b"secret").expect("the file writes");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("the mode sets");
    let (_home, features) = test_features(Registry::new());
    let path = below(&dir, "locked.bin");
    let (status, body) = answer_text(&features, &parts(Method::GET, &[]), &path).await;
    let head = answer(&features, &parts(Method::HEAD, &[]), &path).await;
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o644)).expect("the mode restores");
    assert_eq!((status, body.as_str()), (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error"));
    assert_eq!(head.status(), StatusCode::OK);
  }

  #[tokio::test]
  async fn an_unlistable_directory_is_403_with_the_permission_detail() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let locked = dir.path().join("locked");
    fs::create_dir(&locked).expect("the dir creates");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("the mode sets");
    let (_home, features) = test_features(Registry::new());
    let outcome = answer_text(&features, &parts(Method::GET, &[]), &below(&dir, "locked")).await;
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("the mode restores");
    assert_eq!(outcome, (StatusCode::FORBIDDEN, r#"{"detail":"Permission denied"}"#.to_string()));
  }

  #[tokio::test]
  async fn a_fifo_is_500_for_get_and_for_head() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let fifo = dir.path().join("pipe");
    nix::unistd::mkfifo(&fifo, Mode::S_IRWXU).expect("the fifo makes");
    let (_home, features) = test_features(Registry::new());
    let path = below(&dir, "pipe");
    let (status, body) = answer_text(&features, &parts(Method::GET, &[]), &path).await;
    assert_eq!((status, body.as_str()), (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error"));
    let head = answer(&features, &parts(Method::HEAD, &[]), &path).await;
    assert_eq!(head.status(), StatusCode::INTERNAL_SERVER_ERROR);
  }

  #[tokio::test]
  async fn a_conditional_header_that_would_send_304_for_a_static_file_gets_the_whole_file() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let file = dir.path().join("notes.txt");
    fs::write(&file, "plain bytes").expect("the file writes");
    let (_home, features) = test_features(Registry::new());
    let far_future = "Wed, 01 Jan 2031 00:00:00 GMT";
    let (status, body) =
      answer_text(&features, &parts(Method::GET, &[("if-modified-since", far_future)]), &below(&dir, "notes.txt"))
        .await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, "plain bytes"));
  }
}
