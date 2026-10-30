//! Perfetto page and merged-trace routes.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header, request::Parts};
use axum::response::Response;
use charliebot_compat::mimetypes::MimeTypes;
use charliebot_compat::routing::get_only;
use charliebot_compat::{responses, static_files, validation};
use charliebot_registry::FeatureContext;
use charliebot_registry::templating::Templating;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::{Value, json};

use crate::merge::{self, BuildFailure, BuildMode};

const VIEWER_PATH: &str = "/perfetto";
const MERGED_PATH: &str = "/perfetto/merged";
const FILE_SERVER_MOUNT: &str = "/absolute_filepath";
const DEFAULT_PATTERN: &str = "*.json";
const QUOTE_SET: &AsciiSet = &NON_ALPHANUMERIC.remove(b'_').remove(b'.').remove(b'-').remove(b'~');

static MIME_TYPES: OnceLock<MimeTypes> = OnceLock::new();

pub(super) fn router(features: &std::sync::Arc<FeatureContext>) -> Router {
  Router::new()
    .route(VIEWER_PATH, get_only(perfetto_viewer))
    .route(MERGED_PATH, get_only(perfetto_merged))
    .with_state(std::sync::Arc::clone(features))
}

pub(super) fn matches(path: &str) -> bool {
  path == VIEWER_PATH || path == MERGED_PATH
}

async fn perfetto_viewer(State(features): State<std::sync::Arc<FeatureContext>>, uri: Uri, request: Parts) -> Response {
  let query = query_pairs(uri.query());
  let traces: Vec<String> = query.iter().filter(|(name, _)| name == "trace").map(|(_, value)| value.clone()).collect();
  let dir = last_value(&query, "dir").map(str::to_string);
  let pattern = last_value(&query, "pattern").unwrap_or(DEFAULT_PATTERN).to_string();
  let title = last_value(&query, "title").map(str::to_string);
  let slim = match last_value(&query, "slim") {
    Some(value) => match validation::parse_bool(validation::Source::Query, "slim", value) {
      Ok(value) => Some(value),
      Err(error) => return validation::response(&[*error]),
    },
    None => None,
  };

  let mut inputs: Vec<(String, Option<PathBuf>)> = traces.iter().map(|value| trace_input(value)).collect();
  if let Some(directory) = &dir {
    let directory = directory.clone();
    let pattern_for_scan = pattern.clone();
    match tokio::task::spawn_blocking(move || discover_trace_paths(&directory, &pattern_for_scan)).await {
      Ok(Ok(paths)) => {
        inputs.extend(paths.into_iter().map(|path| (format!("{FILE_SERVER_MOUNT}{}", path.display()), Some(path))))
      }
      Ok(Err(error)) => return responses::internal_error("OSError", error.message().to_string()),
      Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
    }
  }
  if inputs.is_empty() {
    return json_error(StatusCode::BAD_REQUEST, "No trace files specified. Provide 'trace' or 'dir' query params.");
  }

  let all_local = inputs.clone();
  let all_local = tokio::task::spawn_blocking(move || all_local_json_traces(&all_local)).await;
  let all_local = match all_local {
    Ok(Ok(all_local)) => all_local,
    Ok(Err(error)) => return responses::internal_error("OSError", error.message().to_string()),
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  let mut warn = None;
  let trace_url = if all_local {
    let mut pairs: Vec<(String, String)> = traces.iter().map(|trace| ("trace".to_string(), trace.clone())).collect();
    if let Some(dir) = &dir {
      pairs.push(("dir".to_string(), dir.clone()));
      pairs.push(("pattern".to_string(), pattern.clone()));
    }
    if let Some(slim) = slim {
      pairs.push(("slim".to_string(), if slim { "True" } else { "False" }.to_string()));
    }
    format!("{MERGED_PATH}?{}", urlencode(&pairs))
  } else {
    if inputs.len() > 1 {
      warn = Some("⚠ Remote or non-JSON traces cannot be merged, showing first trace only".to_string());
    }
    inputs[0].0.clone()
  };
  let first_url = &inputs[0].0;
  let display_title = title
    .filter(|value| !value.is_empty())
    .or_else(|| dir.clone().filter(|value| !value.is_empty()))
    .unwrap_or_else(|| first_url.rsplit('/').next().unwrap_or(first_url).to_string());
  let is_merge = trace_url.starts_with(MERGED_PATH) && (inputs.len() > 1 || slim.unwrap_or(false));
  let context = json!({
    "trace_url": trace_url,
    "title": display_title,
    "warn": warn,
    "merge_count": inputs.len(),
    "is_merge": is_merge,
    "is_direct_pass": trace_url.starts_with(MERGED_PATH) && !is_merge,
  });
  match render_perfetto(&features, &context, &request) {
    Ok(response) => response,
    Err((kind, detail)) => responses::internal_error(&kind, detail),
  }
}

async fn perfetto_merged(
  State(features): State<std::sync::Arc<FeatureContext>>,
  uri: Uri,
  headers: HeaderMap,
) -> Response {
  let query = query_pairs(uri.query());
  let traces: Vec<PathBuf> =
    query.iter().filter(|(name, _)| name == "trace").map(|(_, value)| PathBuf::from(value)).collect();
  let dir = last_value(&query, "dir").map(str::to_string);
  let pattern = last_value(&query, "pattern").unwrap_or(DEFAULT_PATTERN).to_string();
  let slim = match last_value(&query, "slim") {
    Some(value) => match validation::parse_bool(validation::Source::Query, "slim", value) {
      Ok(value) => value,
      Err(error) => return validation::response(&[*error]),
    },
    None => false,
  };
  if traces.is_empty() && dir.is_none() {
    return json_error(StatusCode::BAD_REQUEST, "Provide at least one trace path with 'trace' or 'dir'.");
  }

  // Python offloads discovery and each selected path check to `asyncio.to_thread` (`src/features/trace/api.py:354,
  // 361-365`); keep the same filesystem work off Tokio's worker here.
  let resolve_traces = tokio::task::spawn_blocking(move || resolve_trace_paths(traces, dir.as_deref(), &pattern)).await;
  let resolved_paths = match resolve_traces {
    Ok(Ok(paths)) => paths,
    Ok(Err(response)) => return *response,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };

  let mode = if resolved_paths.len() == 1 && !slim { BuildMode::DirectPass } else { BuildMode::Merge };
  match merge::cached_build(features, resolved_paths.clone(), slim, mode).await {
    Ok(cache_path) => serve_cached_trace(&cache_path, &headers),
    Err(error) => {
      tracing::error!(
        paths = ?resolved_paths.iter().map(|path| path.display().to_string()).collect::<Vec<_>>(),
        slim,
        detail = %error.message(),
        "perfetto_merge_failed"
      );
      let detail = if mode == BuildMode::Merge && resolved_paths.len() == 1 && slim && !error.is_not_a_trace() {
        format!("merged-trace build failed: {}", error.message())
      } else {
        error.message().to_string()
      };
      json_error(StatusCode::INTERNAL_SERVER_ERROR, &detail)
    }
  }
}

fn serve_cached_trace(cache_path: &Path, headers: &HeaderMap) -> Response {
  let directory = cache_path.parent().expect("the cache artifact has a parent directory");
  let name = cache_path.file_name().expect("the cache artifact has a file name").to_string_lossy();
  let served = static_files::StaticDirectory::new(directory.to_path_buf());
  let mut response =
    static_files::serve(&served, MIME_TYPES.get_or_init(MimeTypes::load), &format!("/{name}"), &Method::GET, headers);
  if response.status() == StatusCode::OK || response.status() == StatusCode::PARTIAL_CONTENT {
    response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/gzip"));
  }
  response
}

fn render_perfetto(features: &FeatureContext, context: &Value, request: &Parts) -> Result<Response, (String, String)> {
  let templating =
    features.startup.require::<Templating>().map_err(|error| ("RuntimeError".to_string(), error.to_string()))?;
  templating
    .render_page(features, "perfetto.html", context, request)
    .map_err(|error| (error.kind().to_string(), error.to_string()))
}

fn trace_input(value: &str) -> (String, Option<PathBuf>) {
  let prefix = format!("{FILE_SERVER_MOUNT}/");
  if value.starts_with(&prefix) {
    return (value.to_string(), Some(PathBuf::from(&value[FILE_SERVER_MOUNT.len()..])));
  }
  if value.starts_with('/') {
    return (format!("{FILE_SERVER_MOUNT}{value}"), Some(PathBuf::from(value)));
  }
  (value.to_string(), None)
}

fn discover_trace_paths(directory: &str, pattern: &str) -> Result<Vec<PathBuf>, BuildFailure> {
  let directory_path = Path::new(directory);
  if !directory_path.is_dir() {
    tracing::warn!(dir = directory, "perfetto_dir_not_found");
    return Ok(Vec::new());
  }
  let mut matcher = GlobMatcher::new(pattern);
  let mut paths = Vec::new();
  for entry in fs::read_dir(directory_path).map_err(|error| BuildFailure::Failed(error.to_string()))? {
    let entry = entry.map_err(|error| BuildFailure::Failed(error.to_string()))?;
    let path = entry.path();
    let is_file = fs::metadata(&path).is_ok_and(|metadata| metadata.is_file());
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
      continue;
    };
    if is_file && matcher.is_match(name) {
      paths.push(path);
    }
  }
  paths.sort_by(|left, right| left.file_name().cmp(&right.file_name()));
  Ok(paths)
}

fn resolve_trace_paths(
  mut paths: Vec<PathBuf>,
  directory: Option<&str>,
  pattern: &str,
) -> Result<Vec<PathBuf>, Box<Response>> {
  if let Some(directory) = directory {
    match discover_trace_paths(directory, pattern) {
      Ok(discovered) if discovered.is_empty() => {
        return Err(Box::new(json_error(
          StatusCode::BAD_REQUEST,
          "Provide at least one trace path; 'dir' matched no files.",
        )));
      }
      Ok(discovered) => paths.extend(discovered),
      Err(error) => return Err(Box::new(responses::internal_error("OSError", error.message().to_string()))),
    }
  }

  let mut resolved_paths = Vec::with_capacity(paths.len());
  for path in paths {
    match fs::metadata(&path) {
      Ok(metadata) if metadata.is_file() => {}
      _ => {
        return Err(Box::new(json_error(StatusCode::NOT_FOUND, &format!("Trace path not found: {}", path.display()))));
      }
    }
    let resolved = match fs::canonicalize(&path) {
      Ok(path) => path,
      Err(error) => {
        return Err(Box::new(responses::internal_error("OSError", format!("{}: {error}", path.display()))));
      }
    };
    match merge::is_json_trace(&resolved) {
      Ok(true) => resolved_paths.push(resolved),
      Ok(false) => {
        return Err(Box::new(json_error(StatusCode::BAD_REQUEST, &format!("Trace is not JSON: {}", path.display()))));
      }
      Err(error) => return Err(Box::new(responses::internal_error("OSError", error.message().to_string()))),
    }
  }
  Ok(resolved_paths)
}

fn all_local_json_traces(inputs: &[(String, Option<PathBuf>)]) -> Result<bool, BuildFailure> {
  for (_, path) in inputs {
    let Some(path) = path else {
      return Ok(false);
    };
    if !fs::metadata(path).is_ok_and(|metadata| metadata.is_file()) {
      return Ok(false);
    }
    if !merge::is_json_trace(path)? {
      return Ok(false);
    }
  }
  Ok(true)
}

fn query_pairs(query: Option<&str>) -> Vec<(String, String)> {
  form_urlencoded::parse(query.unwrap_or_default().as_bytes())
    .map(|(key, value)| (key.into_owned(), value.into_owned()))
    .collect()
}

fn last_value<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
  pairs.iter().rev().find(|(name, _)| name == key).map(|(_, value)| value.as_str())
}

fn urlencode(pairs: &[(String, String)]) -> String {
  pairs.iter().map(|(key, value)| format!("{}={}", quote_plus(key), quote_plus(value))).collect::<Vec<_>>().join("&")
}

fn quote_plus(value: &str) -> String {
  value.split(' ').map(|part| utf8_percent_encode(part, QUOTE_SET).to_string()).collect::<Vec<_>>().join("+")
}

enum GlobToken {
  Star,
  Any,
  Literal(char),
  Class(Vec<char>),
}

/// A reusable `fnmatch.fnmatch` pattern. Python compiles and caches each pattern, then runs the matcher per filename
/// (`src/features/trace/api.py:130-138`); compile once here before the directory loop and reuse the name buffer.
struct GlobMatcher {
  tokens: Vec<GlobToken>,
  name: Vec<char>,
}

impl GlobMatcher {
  fn new(pattern: &str) -> Self {
    let pattern: Vec<char> = pattern.chars().collect();
    let mut tokens = Vec::with_capacity(pattern.len());
    let mut index = 0;
    while index < pattern.len() {
      match pattern[index] {
        '*' => {
          if !matches!(tokens.last(), Some(GlobToken::Star)) {
            tokens.push(GlobToken::Star);
          }
          index += 1;
        }
        '?' => {
          tokens.push(GlobToken::Any);
          index += 1;
        }
        '[' => match parse_class(&pattern, index) {
          Some((class, next)) => {
            tokens.push(GlobToken::Class(class));
            index = next;
          }
          None => {
            tokens.push(GlobToken::Literal('['));
            index += 1;
          }
        },
        literal => {
          tokens.push(GlobToken::Literal(literal));
          index += 1;
        }
      }
    }
    Self { tokens, name: Vec::new() }
  }

  fn is_match(&mut self, name: &str) -> bool {
    self.name.clear();
    self.name.extend(name.chars());
    let (mut name_index, mut token_index) = (0, 0);
    let (mut star_index, mut star_end) = (None, 0);
    while name_index < self.name.len() {
      match self.tokens.get(token_index) {
        Some(GlobToken::Star) => {
          star_index = Some(token_index);
          star_end = name_index;
          token_index += 1;
        }
        Some(token) if glob_token_matches(token, self.name[name_index]) => {
          name_index += 1;
          token_index += 1;
        }
        _ => match star_index {
          Some(star) => {
            star_end += 1;
            name_index = star_end;
            token_index = star + 1;
          }
          None => return false,
        },
      }
    }
    while matches!(self.tokens.get(token_index), Some(GlobToken::Star)) {
      token_index += 1;
    }
    token_index == self.tokens.len()
  }
}

fn glob_token_matches(token: &GlobToken, value: char) -> bool {
  match token {
    GlobToken::Star => unreachable!("star tokens are consumed by the matcher"),
    GlobToken::Any => true,
    GlobToken::Literal(literal) => *literal == value,
    GlobToken::Class(class) => class_matches(class, value),
  }
}

fn parse_class(pattern: &[char], start: usize) -> Option<(Vec<char>, usize)> {
  let mut cursor = start + 1;
  if cursor < pattern.len() && pattern[cursor] == '!' {
    cursor += 1;
  }
  if cursor < pattern.len() && pattern[cursor] == ']' {
    cursor += 1;
  }
  let close = (cursor..pattern.len()).find(|index| pattern[*index] == ']')?;
  Some((pattern[start + 1..close].to_vec(), close + 1))
}

fn class_matches(class: &[char], value: char) -> bool {
  let negated = class.first() == Some(&'!');
  let chars = if negated { &class[1..] } else { class };
  let mut matched = false;
  let mut index = 0;
  while index < chars.len() {
    if index + 2 < chars.len() && chars[index + 1] == '-' {
      if chars[index] <= chars[index + 2] && (chars[index]..=chars[index + 2]).contains(&value) {
        matched = true;
      }
      index += 3;
    } else {
      if chars[index] == value {
        matched = true;
      }
      index += 1;
    }
  }
  matched != negated
}

fn json_error(status: StatusCode, detail: &str) -> Response {
  responses::json(status, &json!({"detail": detail}))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn query_and_path_helpers_keep_order_and_encoding() {
    let query = query_pairs(Some("trace=one&trace=two+words&slim=No&slim=yes"));
    assert_eq!(
      query.iter().filter(|(name, _)| name == "trace").map(|(_, value)| value.as_str()).collect::<Vec<_>>(),
      vec!["one", "two words"]
    );
    assert_eq!(last_value(&query, "slim"), Some("yes"));
    assert_eq!(quote_plus("a b/é~"), "a+b%2F%C3%A9~");
    assert!(GlobMatcher::new("trace_rank[0-9].json").is_match("trace_rank2.json"));
    assert!(GlobMatcher::new("*.json").is_match("trace.json"));
    assert!(!GlobMatcher::new("*.json").is_match("trace.JSON"));
  }

  #[test]
  fn trace_input_preserves_mount_and_absolute_file_urls() {
    assert_eq!(
      trace_input("/absolute_filepath/tmp/a.json"),
      ("/absolute_filepath/tmp/a.json".to_string(), Some(PathBuf::from("/tmp/a.json")))
    );
    assert_eq!(
      trace_input("/tmp/a.json"),
      ("/absolute_filepath/tmp/a.json".to_string(), Some(PathBuf::from("/tmp/a.json")))
    );
    assert_eq!(trace_input("https://example.invalid/a.json"), ("https://example.invalid/a.json".to_string(), None));
  }
}
