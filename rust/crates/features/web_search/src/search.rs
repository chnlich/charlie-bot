//! The Google search call and its grounding sources: one `generateContent` request with the `google_search` tool,
//! then one redirect resolution per grounding chunk.

use charliebot_core::credentials::Credentials;
use futures_util::future::join_all;
use serde_json::{Value, json};

/// The model that serves `google_search`. It is the model this path was measured on.
pub const SEARCH_MODEL: &str = "gemini-3.8-flash";

/// The Google Generative Language API base URL.
pub const GEMINI_ENDPOINT: &str = "https://generativelanguage.googleapis.com/v1beta";

/// How many characters of an upstream body an error text carries.
const BODY_EXCERPT_CHARS: usize = 500;

/// One grounding chunk: the redirect URL Google returned and the page title it named.
struct Chunk {
  uri: String,
  title: String,
}

/// One source line: the real URL when its redirect resolved, else the redirect URL and the failure text.
struct Source {
  title: String,
  url: String,
  error: Option<String>,
}

/// One search: the answer text, the queries Google ran, and the sources.
pub struct Search {
  answer: String,
  queries: Vec<String>,
  sources: Vec<Source>,
}

impl Search {
  /// The tool result text: the answer, one `Search queries:` line, then `Sources:` with one line per source.
  pub fn render(&self) -> String {
    let queries = match self.queries.is_empty() {
      true => "none".to_string(),
      false => self.queries.join(", "),
    };
    let mut lines = vec![self.answer.clone(), format!("Search queries: {queries}"), "Sources:".to_string()];
    for (index, source) in self.sources.iter().enumerate() {
      let mut line = format!("[{}] {} — {}", index + 1, source.title, source.url);
      if let Some(error) = &source.error {
        line.push_str(&format!(" (redirect resolution failed: {error})"));
      }
      lines.push(line);
    }
    lines.join("\n")
  }
}

/// Search the web for `query` and resolve every source's redirect URL.
///
/// Every failure is the tool result text: a missing `gemini.api_key`, a non-200 answer, a body without a candidate,
/// or a request that did not complete.
pub async fn search(
  credentials: &Credentials,
  client: &reqwest::Client,
  endpoint: &str,
  query: &str,
) -> Result<Search, String> {
  let key = credentials
    .get("gemini", "api_key")
    .filter(|value| value.is_truthy())
    .map(|value| value.value_str())
    .ok_or_else(|| "web search is not configured: credentials.yaml has no gemini.api_key".to_string())?;
  let url = format!("{}/models/{SEARCH_MODEL}:generateContent", endpoint.trim_end_matches('/'));
  let payload = json!({
    "contents": [{"role": "user", "parts": [{"text": query}]}],
    "tools": [{"google_search": {}}],
  });
  let response = client
    .post(&url)
    .header(reqwest::header::CONTENT_TYPE, "application/json")
    .header("x-goog-api-key", key)
    .body(serde_json::to_vec(&payload).expect("a JSON value serializes"))
    .send()
    .await
    .map_err(|error| format!("the Google search request failed: {error}"))?;
  let status = response.status();
  let body = response.bytes().await.map_err(|error| format!("reading the Google search answer failed: {error}"))?;
  if status != reqwest::StatusCode::OK {
    return Err(format!("Google search failed with HTTP {}: {}", status.as_u16(), excerpt(&body)));
  }
  let document: Value = serde_json::from_slice(&body)
    .map_err(|error| format!("the Google search answer is not JSON: {error}: {}", excerpt(&body)))?;
  let candidate = document
    .get("candidates")
    .and_then(Value::as_array)
    .and_then(|candidates| candidates.first())
    .ok_or_else(|| format!("Google search returned no candidate (HTTP 200): {}", excerpt(&body)))?;
  let answer = candidate
    .pointer("/content/parts")
    .and_then(Value::as_array)
    .map(|parts| parts.iter().filter_map(|part| part.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join(""))
    .unwrap_or_default();
  let grounding = candidate.get("groundingMetadata");
  let queries = grounding
    .and_then(|grounding| grounding.get("webSearchQueries"))
    .and_then(Value::as_array)
    .map(|values| values.iter().filter_map(Value::as_str).map(str::to_string).collect())
    .unwrap_or_default();
  let chunks = grounding
    .and_then(|grounding| grounding.get("groundingChunks"))
    .and_then(Value::as_array)
    .map(|values| values.iter().filter_map(chunk).collect())
    .unwrap_or_default();
  Ok(Search { answer, queries, sources: resolve_sources(client, chunks).await })
}

/// One grounding chunk's redirect URL and title; a chunk without a `web.uri` names no source.
fn chunk(value: &Value) -> Option<Chunk> {
  let web = value.get("web")?;
  let uri = web.get("uri").and_then(Value::as_str).filter(|uri| !uri.is_empty())?;
  let title = web.get("title").and_then(Value::as_str).unwrap_or("").to_string();
  Some(Chunk { uri: uri.to_string(), title })
}

/// Resolve every chunk's redirect URL in parallel, keeping a failed chunk's redirect URL on its line.
async fn resolve_sources(client: &reqwest::Client, chunks: Vec<Chunk>) -> Vec<Source> {
  join_all(chunks.into_iter().map(|chunk| resolve_source(client, chunk))).await
}

async fn resolve_source(client: &reqwest::Client, chunk: Chunk) -> Source {
  match redirect_target(client, &chunk.uri).await {
    Ok(url) => Source { title: chunk.title, url, error: None },
    Err(error) => Source { title: chunk.title, url: chunk.uri, error: Some(error) },
  }
}

/// One GET that does not follow redirects; a source's real URL is the `Location` of its grounding redirect.
async fn redirect_target(client: &reqwest::Client, uri: &str) -> Result<String, String> {
  let response = client.get(uri).send().await.map_err(|error| error.to_string())?;
  let status = response.status();
  match response.headers().get(reqwest::header::LOCATION).and_then(|value| value.to_str().ok()) {
    Some(location) => Ok(location.to_string()),
    None => Err(format!("HTTP {} without a Location header", status.as_u16())),
  }
}

/// The start of an upstream body, for an error text.
fn excerpt(body: &[u8]) -> String {
  String::from_utf8_lossy(body).chars().take(BODY_EXCERPT_CHARS).collect()
}
