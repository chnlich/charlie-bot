//! The `GET /ncu` page: `src/features/ncu/api.py`.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, Uri, request::Parts};
use axum::response::Response;
use charliebot_compat::{responses, routing::get_only};
use charliebot_core::RepoRoot;
use charliebot_registry::FeatureContext;
use charliebot_registry::templating::Templating;
use nix::errno::Errno;
use serde_json::{Value, json};

use crate::error::ParseError;
use crate::pycompat;
use crate::viewer::{NSIGHT_ROOT, Viewer};

/// The viewer route (`api.py:24`). The auth whitelist does not admit it: it reads local report files.
const VIEWER_PATH: &str = "/ncu";
/// The file server's mount: the download URL is this prefix and the absolute path (`file_urls.FILE_SERVER_MOUNTS`).
const FILE_SERVER_MOUNT: &str = "/absolute_filepath";
const NO_REPORT: &str = "No report specified. Provide a 'file' query param with an absolute path to a .ncu-rep file.";
/// The `Path.is_file()` errors that mean "not a file" rather than a failure (Python's `_ignore_error`).
const NOT_A_FILE_ERRNOS: [i32; 4] =
  [Errno::ENOENT as i32, Errno::ENOTDIR as i32, Errno::EBADF as i32, Errno::ELOOP as i32];

/// What the page reads: the feature context for the templates and the startup state, and the viewer's caches.
struct NcuRoute {
  features: Arc<FeatureContext>,
  viewer: Viewer,
}

pub(super) fn router(features: &Arc<FeatureContext>) -> Router {
  router_over(Arc::clone(features), Viewer::new(env::var_os("PATH"), PathBuf::from(NSIGHT_ROOT)))
}

/// The page over a given viewer. The server uses the search path and install root of its environment.
pub(crate) fn router_over(features: Arc<FeatureContext>, viewer: Viewer) -> Router {
  Router::new().route(VIEWER_PATH, get_only(ncu_viewer)).with_state(Arc::new(NcuRoute { features, viewer }))
}

pub(super) fn matches(path: &str) -> bool {
  path == VIEWER_PATH
}

async fn ncu_viewer(State(route): State<Arc<NcuRoute>>, uri: Uri, request: Parts) -> Response {
  view(route, &uri, &request).await.unwrap_or_else(|response| response)
}

/// The page's answer. Every answer other than a parsed report is an `Err`, so the steps can use `?`.
#[allow(clippy::result_large_err)]
async fn view(route: Arc<NcuRoute>, uri: &Uri, request: &Parts) -> Result<Response, Response> {
  let files = query_values(uri.query(), "file");
  let Some(target) = files.first() else {
    return Err(error_page(&route.features, request, NO_REPORT.to_string(), StatusCode::BAD_REQUEST));
  };
  if !target.starts_with('/') {
    let message = format!("Report path must be absolute: {target}");
    return Err(error_page(&route.features, request, message, StatusCode::BAD_REQUEST));
  }
  let path = pycompat::pathlib_str(target);

  let checked = path.clone();
  let is_file = on_blocking(move || is_regular_file(Path::new(&checked))).await?;
  if !is_file.map_err(|error| responses::internal_error("OSError", error.to_string()))? {
    let message = format!("Report not found: {target}");
    return Err(error_page(&route.features, request, message, StatusCode::NOT_FOUND));
  }

  let root = route
    .features
    .startup
    .require::<RepoRoot>()
    .map_err(|error| responses::internal_error("RuntimeError", error.to_string()))?;
  let parsing = Arc::clone(&route);
  let parsed_path = path.clone();
  let report = on_blocking(move || parsing.viewer.parse(root.path(), &parsed_path))
    .await?
    .map_err(|error| parse_failure(&route.features, request, error))?;

  let context = json!({
    "error": null,
    "report": &*report,
    "report_path": &path,
    "filename": pycompat::basename(&path),
    "download_url": format!("{FILE_SERVER_MOUNT}{path}"),
    "extra_count": files.len() - 1,
    "ncu_ui_cmd": format!("ncu-ui {path}"),
    "ncu_details_cmd": format!("ncu --import {path} --page details"),
    "ncu_source_cmd": format!("ncu --import {path} --page source --print-source sass"),
    "ncu_session_cmd": format!("ncu --import {path} --page session"),
  });
  Ok(render(&route.features, request, &context, StatusCode::OK))
}

/// `NcuParseError` answers 422 with its text. The helper's own failure answers 500 with its text. Any other exception
/// is a plain 500, as the server answers a Python exception the viewer does not catch.
fn parse_failure(features: &FeatureContext, request: &Parts, error: ParseError) -> Response {
  match error {
    ParseError::Report(message) => error_page(features, request, message, StatusCode::UNPROCESSABLE_ENTITY),
    ParseError::Helper(message) => error_page(features, request, message, StatusCode::INTERNAL_SERVER_ERROR),
    ParseError::Internal { kind, detail } => responses::internal_error(kind, detail),
  }
}

/// `ncu.html` with `error` set and `report` null.
fn error_page(features: &FeatureContext, request: &Parts, message: String, status: StatusCode) -> Response {
  render(features, request, &json!({"error": message, "report": null}), status)
}

fn render(features: &FeatureContext, request: &Parts, context: &Value, status: StatusCode) -> Response {
  let templating = match features.startup.require::<Templating>() {
    Ok(templating) => templating,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  match templating.render_page(features, "ncu.html", context, request) {
    Ok(mut response) => {
      *response.status_mut() = status;
      response
    }
    Err(error) => responses::internal_error(error.kind(), error.to_string()),
  }
}

/// Run `work` on the blocking pool. A task that does not finish answers as a RuntimeError.
#[allow(clippy::result_large_err)]
async fn on_blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Result<T, Response> {
  tokio::task::spawn_blocking(work).await.map_err(|error| responses::internal_error("RuntimeError", error.to_string()))
}

/// `Path.is_file()`: whether the path follows to a regular file. A missing path, a path through a non-directory, a
/// symlink loop or a NUL byte is not a file. Any other failure is an error.
fn is_regular_file(path: &Path) -> io::Result<bool> {
  match fs::metadata(path) {
    Ok(metadata) => Ok(metadata.is_file()),
    Err(error) if error.kind() == io::ErrorKind::InvalidInput => Ok(false),
    Err(error) if error.raw_os_error().is_some_and(|number| NOT_A_FILE_ERRNOS.contains(&number)) => Ok(false),
    Err(error) => Err(error),
  }
}

/// `request.query_params.getlist(name)`: every value of a repeated name, decoded as Starlette decodes it. `+` is a
/// space, and a pair without `=` has an empty value.
fn query_values(query: Option<&str>, name: &str) -> Vec<String> {
  form_urlencoded::parse(query.unwrap_or_default().as_bytes())
    .filter(|(key, _)| key == name)
    .map(|(_, value)| value.into_owned())
    .collect()
}

#[cfg(test)]
mod tests {
  use std::ffi::OsString;
  use std::fs;
  use std::os::unix::fs::PermissionsExt;
  use std::path::Path;

  use axum::body::Body;
  use axum::http::{HeaderValue, Request, header};
  use tower::ServiceExt;

  use super::*;
  use crate::test_support::{Install, fake_install, feature_context, module_report, repo_root, write_report};

  #[test]
  fn repeated_file_values_match_starlette_query_decoding() {
    assert_eq!(
      query_values(Some("file=%2Ftmp%2Fa+b&file=%2Ftmp%2Fsecond%20file&file&other=x"), "file"),
      ["/tmp/a b", "/tmp/second file", ""]
    );
  }

  /// A checkout whose interpreter is a file that cannot run: a helper start fails with a 500.
  fn checkout_without_a_runnable_interpreter() -> tempfile::TempDir {
    let checkout = tempfile::tempdir().unwrap();
    let interpreter = checkout.path().join(".venv").join("bin").join("python");
    fs::create_dir_all(interpreter.parent().unwrap()).unwrap();
    fs::write(&interpreter, "not executable\n").unwrap();
    fs::set_permissions(&interpreter, fs::Permissions::from_mode(0o644)).unwrap();
    checkout
  }

  /// The page's answer to `uri`: the status, the content type, and the body.
  async fn get(features: &Arc<FeatureContext>, viewer: Viewer, uri: &str) -> (StatusCode, Option<HeaderValue>, String) {
    let request = Request::get(uri).body(Body::empty()).unwrap();
    let response = router_over(Arc::clone(features), viewer).oneshot(request).await.unwrap();
    let status = response.status();
    let content_type = response.headers().get(header::CONTENT_TYPE).cloned();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, content_type, String::from_utf8(body.to_vec()).unwrap())
  }

  fn module_viewer(install: &Path) -> Viewer {
    let search_path = OsString::from(format!("{}/bin:/usr/bin:/bin", install.display()));
    Viewer::new(Some(search_path), tempfile::tempdir().unwrap().keep())
  }

  #[tokio::test]
  async fn no_file_answers_400_with_the_error_page() {
    let (_home, features) = feature_context(&repo_root(), &repo_root());

    let (status, content_type, body) = get(&features, module_viewer(&fake_install()), "/ncu").await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(content_type.unwrap(), "text/html; charset=utf-8");
    assert!(
      body.contains(
        "No report specified. Provide a &#39;file&#39; query param with an absolute path to a .ncu-rep file."
      )
    );
  }

  #[tokio::test]
  async fn a_relative_path_answers_400_and_a_missing_file_404() {
    let (_home, features) = feature_context(&repo_root(), &repo_root());
    let viewer = module_viewer(&fake_install());

    let (status, _, body) = get(&features, viewer, "/ncu?file=relative/a.ncu-rep").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("Report path must be absolute: relative/a.ncu-rep"));

    let missing = "/ncu?file=/nonexistent-synthetic-dir/a.ncu-rep";
    let (status, _, body) = get(&features, module_viewer(&fake_install()), missing).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("Report not found: /nonexistent-synthetic-dir/a.ncu-rep"));
  }

  #[tokio::test]
  async fn a_parsed_report_renders_the_page_with_its_report_and_commands() {
    let (_home, features) = feature_context(&repo_root(), &repo_root());
    let reports = tempfile::tempdir().unwrap();
    let path = write_report(reports.path(), "a.ncu-rep", &module_report("synthetic_gemm"));
    let other = write_report(reports.path(), "b.ncu-rep", "");
    let uri = format!("/ncu?file={path}&file={other}");

    let (status, _, body) = get(&features, module_viewer(&fake_install()), &uri).await;

    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("const REPORT = {"));
    assert!(body.contains("\"parser\": \"ncu_report\"") || body.contains("\"parser\":\"ncu_report\""));
    assert!(body.contains(&format!("href=\"/absolute_filepath{path}\"")));
    assert!(body.contains(&format!("ncu --import {path} --page details")));
    assert!(body.contains("1 more report(s) ignored"));
  }

  #[tokio::test]
  async fn a_refused_report_answers_422_with_the_module_error() {
    let (_home, features) = feature_context(&repo_root(), &repo_root());
    let reports = tempfile::tempdir().unwrap();
    let path = write_report(reports.path(), "a.ncu-rep", r#"{"refuse": "synthetic refusal", "kernels": []}"#);

    let (status, _, body) = get(&features, module_viewer(&fake_install()), &format!("/ncu?file={path}")).await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body.contains("Unable to load report: synthetic refusal"));
  }

  #[tokio::test]
  async fn a_helper_that_cannot_start_answers_500_with_the_error_page() {
    let checkout = checkout_without_a_runnable_interpreter();
    let (_home, features) = feature_context(checkout.path(), &repo_root());
    let reports = tempfile::tempdir().unwrap();
    let path = write_report(reports.path(), "a.ncu-rep", "");
    let install = Install::with_module(&module_report("unused"));

    let (status, content_type, body) = get(&features, module_viewer(install.dir()), &format!("/ncu?file={path}")).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(content_type.unwrap(), "text/html; charset=utf-8");
    assert!(body.contains("did not start"), "{body}");
  }
}
