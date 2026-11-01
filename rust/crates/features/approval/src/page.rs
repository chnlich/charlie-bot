//! `GET /approval`: the approval page, one Pacific day's queue, login sheet and digest. The view
//! moves between days through the `date` query, which an absent value answers with today.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::request::Parts;
use axum::response::Response;
use charliebot_compat::{responses, routing};
use charliebot_registry::FeatureContext;
use charliebot_registry::templating::Templating;
use jiff::{Span, civil::Date};
use serde_json::{Value, json};

use super::day::{self, DayError};

const TEMPLATE: &str = "approval_day.html";

/// The router of the page's one route. The feature context is the route's state.
pub fn routes(features: &Arc<FeatureContext>) -> Router {
  Router::new().route("/approval", routing::get_only(approval_page)).with_state(Arc::clone(features))
}

/// The paths the routes answer, for the trailing-slash redirect.
pub fn matches(path: &str) -> bool {
  path == "/approval"
}

/// The page. The read and the render run off the async runtime, as the usage page's do.
async fn approval_page(State(features): State<Arc<FeatureContext>>, request: Parts) -> Response {
  let date = match date_param(&request) {
    Ok(date) => date,
    Err(error) => return refused_answer(error),
  };
  let view = match day::approval_day(&features, date).await {
    Ok(view) => view,
    Err(error) => return refused_answer(error),
  };
  let rendered = tokio::task::spawn_blocking(move || render(&features, view, &request))
    .await
    .expect("rendering the approval page does not panic");
  match rendered {
    Ok(response) => response,
    Err((kind, detail)) => internal_error(&kind, detail),
  }
}

/// The rendered page: the view plus the day navigation.
fn render(features: &FeatureContext, view: Value, request: &Parts) -> Result<Response, (String, String)> {
  let date_text = view["date"].as_str().unwrap_or_default().to_string();
  let day: Date = date_text.parse().map_err(|error| ("ValueError".to_string(), format!("the view's date: {error}")))?;
  let previous = day.checked_sub(Span::new().days(1)).map_err(|error| ("ValueError".to_string(), error.to_string()))?;
  let next = day.checked_add(Span::new().days(1)).map_err(|error| ("ValueError".to_string(), error.to_string()))?;
  let context = json!({
    "ctx": {
      "date": date_text,
      "previous": previous.to_string(),
      "next": next.to_string(),
      "queue": view["queue"],
      "login_sheet": view["login_sheet"],
      "digest": view["digest"],
    },
  });
  let templating =
    features.startup.require::<Templating>().map_err(|error| ("RuntimeError".to_string(), error.to_string()))?;
  templating
    .render_page(features, TEMPLATE, &context, request)
    .map_err(|error| (error.kind().to_string(), error.to_string()))
}

/// A refusal answer: a bad `date` query answers its status and detail before any read.
fn refused_answer(error: DayError) -> Response {
  match error {
    DayError::Refused(code, detail) => responses::json(code, &json!({"detail": detail})),
    DayError::Failed(error) => internal_error("RuntimeError", format!("{error:#}")),
  }
}

fn internal_error(kind: &str, detail: String) -> Response {
  responses::internal_error(kind, detail)
}

/// The `date` query's value, `None` when the query names no date. A value that is not a `YYYY-MM-DD` date is a
/// refusal, as the API refuses.
fn date_param(request: &Parts) -> Result<Option<Date>, DayError> {
  let query = request.uri.query().unwrap_or_default();
  let pairs: HashMap<String, String> = query
    .split('&')
    .filter_map(|pair| {
      let (name, value) = pair.split_once('=')?;
      Some((name.to_string(), value.to_string()))
    })
    .collect();
  match pairs.get("date").map(String::as_str) {
    None => Ok(None),
    Some(text) => day::parse_date_param(text).map(Some),
  }
}

#[cfg(test)]
#[path = "page_tests.rs"]
mod tests;
