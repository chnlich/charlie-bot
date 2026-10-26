//! The file server's session artifact view: comment UI injection and the `?diff=` plan comparison.

use std::fs;
use std::path::{Path, PathBuf};

use axum::http::request::Parts;
use axum::response::Response;
use charliebot_compat::responses;
use charliebot_core::paths;
use charliebot_core::pyfmt;
use charliebot_registry::templating::Templating;
use charliebot_registry::wiring::FileView;
use charliebot_registry::{FeatureContext, StateError};
use http::{HeaderValue, StatusCode, header};
use serde_json::json;

use crate::plan_diff;

const DIFF_TARGET_DETAIL: &str = "diff target is not a session artifact page: ";
const DIFF_BASE_NOT_FOUND_DETAIL: &str = "diff base not found: ";
const DIFF_BASE_INVALID_DETAIL: &str = "diff base is not a session artifact page: ";
type ViewError = Box<Response>;
type ViewResult<T> = Result<T, ViewError>;

pub(crate) struct ArtifactView;

impl FileView for ArtifactView {
  fn view<'a>(
    &'a self,
    features: &'a FeatureContext,
    request: &'a Parts,
    path: &'a str,
  ) -> futures_util::future::BoxFuture<'a, Result<Option<Response>, Response>> {
    Box::pin(async move { serve(features, request, path).await.map_err(|response| *response) })
  }
}

async fn serve(features: &FeatureContext, request: &Parts, path: &str) -> ViewResult<Option<Response>> {
  let diff_param = request.uri.query().and_then(|query| query_value(query, "diff"));
  if diff_param.is_none() && !path.to_lowercase().ends_with(".html") {
    return Ok(None);
  }

  let config = features.config.get().map_err(|error| internal_error("ConfigError", error.to_string()))?;
  let sessions_dir = config.sessions_dir();
  let target_path = path.to_owned();
  let target_diff = diff_param.clone();
  let resolved =
    tokio::task::spawn_blocking(move || resolve_target(&sessions_dir, &target_path, target_diff.as_deref()))
      .await
      .map_err(|error| internal_error("RuntimeError", error.to_string()))??;
  let Some((target_path, session_id)) = resolved else {
    return Ok(None);
  };

  let html = if let Some(diff_param) = diff_param {
    let base_sessions_dir = config.sessions_dir();
    let base_session = session_id.clone();
    let base_param = diff_param.clone();
    let base_text = tokio::task::spawn_blocking(move || read_diff_base(&base_sessions_dir, &base_session, &base_param))
      .await
      .map_err(|error| internal_error("RuntimeError", error.to_string()))??;

    let page_text = read_target_text(target_path.clone()).await?;
    plan_diff::annotate(&base_text, &page_text).map_err(|error| internal_error("ValueError", error))?
  } else {
    read_target_text(target_path).await?
  };
  let (comment_version, artifact_version) = asset_versions(features)?;
  Ok(Some(html_response(inject_artifact_ui(&html, &session_id, &comment_version, &artifact_version))))
}

fn query_value(query: &str, key: &str) -> Option<String> {
  url::form_urlencoded::parse(query.as_bytes())
    .filter(|(name, _)| name == key)
    .map(|(_, value)| value.into_owned())
    .last()
}

fn resolve_target(sessions_dir: &Path, path: &str, diff_param: Option<&str>) -> ViewResult<Option<(PathBuf, String)>> {
  let fs_path = paths::realpath(&Path::new("/").join(path));
  let session_id = if is_html_path(&fs_path) { artifact_session_id(sessions_dir, &fs_path) } else { None };

  let Some(_diff_param) = diff_param else {
    let Some(session_id) = session_id else { return Ok(None) };
    return Ok(fs::metadata(&fs_path).ok().filter(|metadata| metadata.is_file()).map(|_| (fs_path, session_id)));
  };

  let metadata = match fs::metadata(&fs_path) {
    Ok(metadata) => Some(metadata),
    Err(error) if matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => {
      return Ok(None);
    }
    Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => None,
    Err(error) => {
      return Err(internal_error("OSError", format!("{}: {error}", fs_path.display())));
    }
  };
  let Some(session_id) = session_id.filter(|_| metadata.as_ref().is_some_and(|value| !value.is_dir())) else {
    return Err(http_error(StatusCode::BAD_REQUEST, format!("{DIFF_TARGET_DETAIL}{}", fs_path.display())));
  };
  Ok(Some((fs_path, session_id)))
}

fn artifact_session_id(sessions_dir: &Path, fs_path: &Path) -> Option<String> {
  let root = paths::realpath(sessions_dir);
  let relative = fs_path.strip_prefix(root).ok()?;
  if fs_path.parent()?.file_name()?.to_str()? != "artifacts" {
    return None;
  }
  let mut parts = relative.components();
  let session_id = parts.next()?.as_os_str().to_str()?;
  if parts.count() < 2 {
    return None;
  }
  Some(session_id.to_owned())
}

fn is_html_path(path: &Path) -> bool {
  path
    .extension()
    .and_then(|extension| extension.to_str())
    .is_some_and(|extension| extension.eq_ignore_ascii_case("html"))
}

fn resolve_diff_base(sessions_dir: &Path, session_id: &str, diff_param: &str) -> ViewResult<PathBuf> {
  let candidate = paths::realpath(&sessions_dir.join(session_id).join(diff_param));
  if !is_html_path(&candidate) || artifact_session_id(sessions_dir, &candidate).is_none() {
    return Err(http_error(StatusCode::BAD_REQUEST, format!("{DIFF_BASE_INVALID_DETAIL}{diff_param}")));
  }
  Ok(candidate)
}

fn read_diff_base(sessions_dir: &Path, session_id: &str, diff_param: &str) -> ViewResult<String> {
  let candidate = resolve_diff_base(sessions_dir, session_id, diff_param)?;
  match fs::read_to_string(&candidate) {
    Ok(text) => Ok(text),
    Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
      Err(internal_error("UnicodeDecodeError", error.to_string()))
    }
    Err(_) => Err(http_error(StatusCode::NOT_FOUND, format!("{DIFF_BASE_NOT_FOUND_DETAIL}{}", candidate.display()))),
  }
}

async fn read_target_text(path: PathBuf) -> ViewResult<String> {
  tokio::task::spawn_blocking(move || match fs::read_to_string(&path) {
    Ok(text) => Ok(text),
    Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
      Err(internal_error("UnicodeDecodeError", error.to_string()))
    }
    Err(error) => Err(internal_error("OSError", format!("{}: {error}", path.display()))),
  })
  .await
  .map_err(|error| internal_error("RuntimeError", error.to_string()))?
}

fn asset_versions(features: &FeatureContext) -> ViewResult<(String, String)> {
  let templating = features
    .startup
    .require::<Templating>()
    .map_err(|error: StateError| internal_error("RuntimeError", error.to_string()))?;
  let comment_version =
    templating.static_asset_version().map_err(|error| internal_error("OSError", error.to_string()))?;
  let artifact_version =
    templating.static_asset_version().map_err(|error| internal_error("OSError", error.to_string()))?;
  Ok((comment_version, artifact_version))
}

fn inject_artifact_ui(html: &str, session_id: &str, comment_version: &str, artifact_version: &str) -> String {
  let mut encoded_session_id = String::with_capacity(session_id.len() + 2);
  pyfmt::write_ascii_json_str(&mut encoded_session_id, session_id);
  let tags = format!(
    "<script>window.__cbcServerSessionId={};</script>\n<script src=/static/js/comment_post.js?v={comment_version}></script>\n<script src=/static/features/artifacts/artifact-comments.js?v={artifact_version}></script>",
    encoded_session_id
  );
  match html.rfind("</body>") {
    Some(index) => format!("{}{}\n{}", &html[..index], tags, &html[index..]),
    None => format!("{html}\n{tags}\n"),
  }
}

fn internal_error(error_type: &str, detail: String) -> ViewError {
  Box::new(responses::internal_error(error_type, detail))
}

fn http_error(status: StatusCode, detail: String) -> ViewError {
  Box::new(responses::json(status, &json!({"detail": detail})))
}

fn html_response(body: String) -> Response {
  let mut response = responses::html(body);
  response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
  response
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::path::Path;

  use axum::body::{Body, to_bytes};
  use axum::http::Request;

  use super::*;
  use crate::test_support::features;

  fn request(uri: &str) -> Parts {
    Request::builder().uri(uri).body(Body::empty()).expect("the request builds").into_parts().0
  }

  fn route_path(path: &Path) -> String {
    path.strip_prefix("/").expect("temporary paths are absolute").to_string_lossy().into_owned()
  }

  fn write_page(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().expect("a page has a parent")).expect("the page directory is created");
    fs::write(path, text).expect("the page writes");
  }

  async fn response_body(response: Response) -> String {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.expect("the response body reads");
    String::from_utf8(bytes.to_vec()).expect("the response body is UTF-8")
  }

  async fn invoke(features: &FeatureContext, path: &str, uri: &str) -> Result<Option<Response>, Box<Response>> {
    ArtifactView.view(features, &request(uri), path).await.map_err(Box::new)
  }

  #[test]
  fn ui_injection_matches_the_python_splice_and_append_rules() {
    let tags = "<script>window.__cbcServerSessionId=\"S\";</script>\n<script src=/static/js/comment_post.js?v=v1></script>\n<script src=/static/features/artifacts/artifact-comments.js?v=v2></script>";
    assert_eq!(
      inject_artifact_ui("<body>one</body>tail</body>", "S", "v1", "v2"),
      format!("<body>one</body>tail{tags}\n</body>")
    );
    assert_eq!(inject_artifact_ui("<p>page</p>", "S", "v1", "v2"), format!("<p>page</p>\n{tags}\n"));
  }

  #[test]
  fn injected_script_keeps_python_json_ascii_escapes_for_unicode_session_ids() {
    let html = inject_artifact_ui("<body>page</body>", "计划😀", "v1", "v2");

    assert!(html.contains(r#"window.__cbcServerSessionId="\u8ba1\u5212\ud83d\ude00";"#));
  }

  #[test]
  fn a_repeated_diff_parameter_takes_its_last_value_like_starlette() {
    let parts = request("/?diff=a&diff=b");
    let query = parts.uri.query().expect("the request has a query");
    assert_eq!(query_value(query, "diff").as_deref(), Some("b"));
  }

  #[tokio::test]
  async fn a_session_artifact_page_gets_ui_and_no_store_headers() {
    let (home, features) = features();
    let page = home.path().join("sessions/S/artifacts/plan.html");
    let source = "<html><body><h1>Plan</h1></body></html>";
    write_page(&page, source);
    let result = invoke(&features, &route_path(&page), "/").await.expect("the view answers");
    let response = result.expect("the artifact page is handled");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "text/html; charset=utf-8");
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert!(!response.headers().contains_key(header::LAST_MODIFIED));
    let (comment_version, artifact_version) = asset_versions(&features).expect("asset versions are available");
    assert_eq!(response_body(response).await, inject_artifact_ui(source, "S", &comment_version, &artifact_version));
  }

  #[tokio::test]
  async fn a_nested_artifact_uses_the_first_session_directory() {
    let (home, features) = features();
    let page = home.path().join("sessions/S/threads/T/sub/artifacts/plan.html");
    write_page(&page, "<body>Plan</body>");
    let response = invoke(&features, &route_path(&page), "/").await.expect("the view answers").unwrap();
    assert!(response_body(response).await.contains("window.__cbcServerSessionId=\"S\";"));
  }

  #[tokio::test]
  async fn paths_outside_session_artifacts_and_non_html_paths_pass_through() {
    let (home, features) = features();
    let outside = home.path().join("workspace/artifacts/outside.html");
    write_page(&outside, "<body>Outside</body>");
    let plain = home.path().join("sessions/S/artifacts/notes.txt");
    write_page(&plain, "plain text");
    assert!(invoke(&features, &route_path(&outside), "/").await.expect("the view checks the path").is_none());
    assert!(invoke(&features, &route_path(&plain), "/").await.expect("the view checks the path").is_none());
    assert!(invoke(&features, "", "/").await.expect("the view checks the path").is_none());
  }

  #[tokio::test]
  async fn diff_answers_annotated_html_with_the_comment_layer() {
    let (home, features) = features();
    let base = home.path().join("sessions/S/artifacts/plan_01.html");
    let target = home.path().join("sessions/S/artifacts/plan_02.html");
    write_page(&base, "<html><body><p>hello world</p></body></html>");
    let target_text = "<html><body><p>hello brave world</p></body></html>";
    write_page(&target, target_text);
    let response = invoke(&features, &route_path(&target), "/?diff=artifacts%2Fplan_01.html")
      .await
      .expect("the view answers")
      .unwrap();
    let body = response_body(response).await;
    assert!(body.contains("<ins class=\"cbd-ins\">brave</ins>"));
    assert!(body.contains("data-cbd-header=\"This version · vs previous\""));
    assert!(body.contains("window.__cbcServerSessionId=\"S\";"));
  }

  #[tokio::test]
  async fn missing_diff_target_passes_to_the_file_server_before_diff_validation() {
    let (home, features) = features();
    let missing = home.path().join("sessions/S/artifacts/missing.html");
    let result = invoke(&features, &route_path(&missing), "/?diff=notes.txt").await.expect("the view passes");
    assert!(result.is_none());
  }

  #[tokio::test]
  async fn diff_target_directories_and_outside_pages_are_bad_requests() {
    let (home, features) = features();
    let directory = home.path().join("sessions/S/artifacts/directory.html");
    fs::create_dir_all(&directory).expect("the target directory is created");
    let result = invoke(&features, &route_path(&directory), "/?diff=artifacts%2Fbase.html").await;
    let response = result.expect_err("a directory target is rejected");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(response_body(*response).await.contains("diff target is not a session artifact page:"));

    let outside = home.path().join("workspace/artifacts/outside.html");
    write_page(&outside, "<body>Outside</body>");
    let response = invoke(&features, &route_path(&outside), "/?diff=artifacts%2Fbase.html")
      .await
      .expect_err("an outside target is rejected");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
  }

  #[tokio::test]
  async fn invalid_and_missing_diff_bases_keep_their_python_status_and_details() {
    let (home, features) = features();
    let target = home.path().join("sessions/S/artifacts/plan.html");
    write_page(&target, "<body>Plan</body>");
    let invalid = invoke(&features, &route_path(&target), "/?diff=notes%2Fbase.html")
      .await
      .expect_err("a non-artifact base is rejected");
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    assert!(response_body(*invalid).await.contains("diff base is not a session artifact page: notes/base.html"));

    let missing = invoke(&features, &route_path(&target), "/?diff=artifacts%2Fmissing.html")
      .await
      .expect_err("a missing base is a 404");
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    assert!(response_body(*missing).await.contains("diff base not found:"));
  }
}
