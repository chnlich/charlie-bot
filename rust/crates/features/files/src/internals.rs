//! The file server's routes and answers: the file views, the directory listing, the file and run-log answers, and the
//! 403 page that refuses a path the server process cannot read, or an entry that is not a regular file.
//!
//! The public surface is `api.rs`, which hands the routes and the matcher to the registry.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, LazyLock};

use axum::Router;
use axum::extract::{Path, Request, State};
use axum::http::request::Parts;
use axum::response::Response;
use charliebot_compat::mimetypes::{self, MimeTypes};
use charliebot_compat::mimetypes_table::TEXT_PLAIN;
use charliebot_compat::responses;
use charliebot_compat::routing::get_and_head;
use charliebot_compat::static_files::{self, StaticDirectory};
use charliebot_core::paths;
use charliebot_registry::icons::Icons;
use charliebot_registry::templating::Templating;
use charliebot_registry::{FeatureContext, StartupState};
use charliebot_store::run_logs;
use http::{HeaderMap, Method, StatusCode, header};
use serde_json::json;

use crate::error_page;
use crate::listing::{self, FailureKind, ListingError};
use crate::mount::MOUNT;

/// The `mimetypes` table of this process, read once at first use.
static MIME_TYPES: LazyLock<MimeTypes> = LazyLock::new(MimeTypes::load);

/// The file server's routes, with the feature context as their state. The route paths repeat `MOUNT` as literals,
/// because `Router::route` takes its path as written.
pub(crate) fn routes(features: &Arc<FeatureContext>) -> Router {
  Router::new()
    .route("/absolute_filepath/", get_and_head(serve_root))
    .route("/absolute_filepath/{*path}", get_and_head(serve))
    .with_state(Arc::clone(features))
}

/// Whether a request path falls under the mount, whatever the method. For a request to the bare prefix, the server's
/// trailing-slash redirect asks this of the slashed path, which turns the 404 into a 307.
pub(crate) fn matches(path: &str) -> bool {
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
  let startup = features.startup.clone();
  let (path, method, headers) = (path.to_string(), parts.method.clone(), parts.headers.clone());
  tokio::task::spawn_blocking(move || answer_from_disk(&path, &method, &headers, || listing_icons(&startup)))
    .await
    .expect("the file server's blocking work does not panic")
}

/// The icon table the directory listing draws its glyphs from, for the current static-tree revision. The listing
/// asks for it only when the path is a directory, so a file answer does not need the templating.
fn listing_icons(startup: &StartupState) -> Result<Arc<Icons>, String> {
  let templating = startup.require::<Templating>().map_err(|error| error.to_string())?;
  templating.icons().map_err(|error| error.to_string())
}

/// The answer below the file views, on the blocking pool: a directory listing, a file, the 403 page that refuses the
/// path, or the 404 or 500 that the route raises for it.
fn answer_from_disk(
  path: &str,
  method: &Method,
  headers: &HeaderMap,
  load_icons: impl FnOnce() -> Result<Arc<Icons>, String>,
) -> Response {
  // The path joined under the root, then resolved to its real path.
  let fs_path = paths::realpath(&FsPath::new("/").join(path));
  let url_prefix = if path.is_empty() { MOUNT.to_string() } else { format!("{MOUNT}/{path}") };
  let display = fs_path.display().to_string();
  match listing::dir_listing_page(&fs_path, &url_prefix, load_icons) {
    Ok(Some(page)) => return responses::html(page),
    Ok(None) => {}
    // A missing path is 404 unless the compaction stored a run log here as `<name>.zst`.
    Err(ListingError::Missing) => return answer_compressed_run_log(&fs_path, method).unwrap_or_else(not_found),
    Err(ListingError::PermissionDenied) => return denied(&fs_path, &display, &url_prefix),
    Err(ListingError::Failed { kind, detail }) => return responses::internal_error(kind.as_str(), detail),
  }
  // Not a directory. The metadata read decides between 404, the refusal of a path that is not a regular file, and the
  // file answer.
  match fs::metadata(&fs_path) {
    Err(_) => not_found(),
    Ok(metadata) if !metadata.is_file() => {
      error_page::refusal(error_page::Refusal::NotRegularFile, &display, &url_prefix)
    }
    // The open tells whether the server process may read the file, before the static answer opens it for its own read.
    Ok(_) => match fs::File::open(&fs_path) {
      Ok(_) => answer_file(&fs_path, method, headers),
      Err(error) if error.kind() == ErrorKind::PermissionDenied => denied(&fs_path, &display, &url_prefix),
      Err(error) => responses::internal_error(FailureKind::OSError.as_str(), format!("{display}: {error}")),
    },
  }
}

/// The refusal of a path whose permissions deny the server process. A directory whose metadata the server process can
/// read is refused as unlistable. Any other entry is refused as unreadable, including an entry under a directory that
/// the server process cannot search, because its metadata is denied too.
fn denied(fs_path: &FsPath, display: &str, url_prefix: &str) -> Response {
  let is_dir = fs::metadata(fs_path).is_ok_and(|metadata| metadata.is_dir());
  let refusal = if is_dir { error_page::Refusal::DirectoryDenied } else { error_page::Refusal::EntryDenied };
  error_page::refusal(refusal, display, url_prefix)
}

/// The file answer for a regular file. It honors only the `Range` headers, so the conditional
/// headers the static views read are removed first. The static answer would otherwise send a 304 for them.
/// The static root is `/`, so the answer serves the resolved path wherever it lies.
fn answer_file(fs_path: &FsPath, method: &Method, headers: &HeaderMap) -> Response {
  let Some(route_path) = fs_path.to_str() else {
    return responses::internal_error(FailureKind::UnicodeEncodeError.as_str(), format!("{fs_path:?} is not UTF-8"));
  };
  let mut headers = headers.clone();
  headers.remove(header::IF_NONE_MATCH);
  headers.remove(header::IF_MODIFIED_SINCE);
  static_files::serve(&StaticDirectory::new(PathBuf::from("/")), &MIME_TYPES, route_path, method, &headers)
}

/// A run log whose plain file the compaction replaced with `<name>.zst`: the decompressed bytes answer the plain
/// path an evidence link names. Any other missing path, and an unreadable or empty log, answers `None` for the 404.
fn answer_compressed_run_log(fs_path: &FsPath, method: &Method) -> Option<Response> {
  if !run_logs::is_run_log_name(fs_path.file_name()?.to_str()?) {
    return None;
  }
  let bytes = run_logs::read_bytes(fs_path).ok().flatten()?;
  let guessed = MIME_TYPES.guess_type(&paths::display(fs_path)).unwrap_or(TEXT_PLAIN);
  let content_type = mimetypes::with_utf8_charset(guessed);
  let size = bytes.len() as u64;
  let body = if method == Method::HEAD { axum::body::Body::empty() } else { axum::body::Body::from(bytes.to_vec()) };
  Response::builder()
    .status(StatusCode::OK)
    .header(header::CONTENT_TYPE, content_type)
    .header(header::CONTENT_LENGTH, size)
    .body(body)
    .ok()
}

fn not_found() -> Response {
  responses::json(StatusCode::NOT_FOUND, &json!({"detail": "Not found"}))
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
  use charliebot_registry::{Registry, RegistryError};
  use charliebot_store::Store;
  use futures_util::future::BoxFuture;
  use nix::sys::stat::Mode;
  use percent_encoding::percent_decode_str;

  use super::*;
  use crate::api::register;

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
    registry.register_package("features.files", register).expect("the package registers");
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
    registry.register_package("features.passes", passing_package).expect("registers");
    registry.register_package("features.answers", answering_package).expect("registers");
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

  /// A run log the compaction stored as `<name>.zst` answers the plain path an evidence link names, with the
  /// decompressed content.
  #[tokio::test]
  async fn a_compressed_run_log_answers_its_plain_path_with_the_decompressed_content() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let plain = dir.path().join("events.jsonl");
    let content = "{\"type\": \"result\"}\n{\"type\": \"assistant\"}\n";
    let compressed = run_logs::compressed_path(&plain);
    fs::write(&compressed, zstd::stream::encode_all(content.as_bytes(), 3).expect("the content compresses"))
      .expect("the compressed log writes");
    let (_home, features) = test_features(Registry::new());
    let path = below(&dir, "events.jsonl");
    let (status, body) = answer_text(&features, &parts(Method::GET, &[]), &path).await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, content));
    let head = answer(&features, &parts(Method::HEAD, &[]), &path).await;
    assert_eq!(head.status(), StatusCode::OK);
  }

  /// A compressed file that is not a run log keeps the 404 its missing plain path answers.
  #[tokio::test]
  async fn a_compressed_file_outside_the_run_logs_keeps_its_plain_path_a_404() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let plain = dir.path().join("notes.txt");
    let content = "notes";
    fs::write(run_logs::compressed_path(&plain), zstd::stream::encode_all(content.as_bytes(), 3).unwrap()).unwrap();
    let (_home, features) = test_features(Registry::new());
    let (status, _) = answer_text(&features, &parts(Method::GET, &[]), &below(&dir, "notes.txt")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
  }

  #[tokio::test]
  async fn an_unreadable_file_is_a_403_page_on_get_and_on_head() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let locked = dir.path().join("locked.bin");
    fs::write(&locked, b"secret").expect("the file writes");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("the mode sets");
    let (_home, features) = test_features(Registry::new());
    let path = below(&dir, "locked.bin");
    let (status, body) = answer_text(&features, &parts(Method::GET, &[]), &path).await;
    let head = answer(&features, &parts(Method::HEAD, &[]), &path).await;
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o644)).expect("the mode restores");
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body.contains(error_page::Refusal::EntryDenied.reason()), "{body}");
    assert_eq!(head.status(), StatusCode::FORBIDDEN);
  }

  #[tokio::test]
  async fn an_unlistable_directory_and_an_entry_under_it_are_403_pages() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let locked = dir.path().join("locked");
    fs::create_dir(&locked).expect("the dir creates");
    fs::write(locked.join("child.txt"), "x").expect("the child writes");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("the mode sets");
    let (_home, features) = test_features(Registry::new());
    let get = parts(Method::GET, &[]);
    let listing = answer_text(&features, &get, &below(&dir, "locked")).await;
    let entry = answer_text(&features, &get, &below(&dir, "locked/child.txt")).await;
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("the mode restores");
    assert_eq!(listing.0, StatusCode::FORBIDDEN);
    assert!(listing.1.contains(error_page::Refusal::DirectoryDenied.reason()), "{}", listing.1);
    assert_eq!(entry.0, StatusCode::FORBIDDEN);
    assert!(entry.1.contains(error_page::Refusal::EntryDenied.reason()), "{}", entry.1);
  }

  /// The parent link of a refusal below a directory named with `#`, `?`, a space and `%` decodes to that directory,
  /// and its href holds no raw `#` or `?`.
  #[tokio::test]
  async fn the_parent_link_of_a_refusal_decodes_to_the_special_named_parent() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let parent = dir.path().join("a#b?c d%e");
    let locked = parent.join("locked");
    fs::create_dir_all(&locked).expect("the dirs create");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("the mode sets");
    let (_home, features) = test_features(Registry::new());
    let (status, body) = answer_text(&features, &parts(Method::GET, &[]), &below(&dir, "a#b?c d%e/locked")).await;
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("the mode restores");
    assert_eq!(status, StatusCode::FORBIDDEN);
    let href = body
      .split(r#"<a class="parent" href=""#)
      .nth(1)
      .and_then(|rest| rest.split('"').next())
      .unwrap_or_else(|| panic!("the refusal links its parent: {body}"));
    assert!(!href.contains(['#', '?']), "a raw # or ? in {href}");
    let decoded = percent_decode_str(href).decode_utf8().expect("the href decodes to UTF-8");
    assert_eq!(decoded, format!("{MOUNT}{}", parent.display()));
  }

  #[tokio::test]
  async fn a_fifo_is_a_403_page_with_a_parent_link_on_get_and_on_head() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let fifo = dir.path().join("pipe");
    nix::unistd::mkfifo(&fifo, Mode::S_IRWXU).expect("the fifo makes");
    let (_home, features) = test_features(Registry::new());
    let path = below(&dir, "pipe");
    let (status, body) = answer_text(&features, &parts(Method::GET, &[]), &path).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body.contains(error_page::Refusal::NotRegularFile.reason()), "{body}");
    assert!(body.contains(r#"class="parent""#), "{body}");
    let head = answer(&features, &parts(Method::HEAD, &[]), &path).await;
    assert_eq!(head.status(), StatusCode::FORBIDDEN);
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
