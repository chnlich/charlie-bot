//! `GET /api/approval/day`: one PT day's queue, login sheet and digest as JSON.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Query, State};
use axum::response::Response;
use charliebot_compat::{responses, routing};
use charliebot_registry::FeatureContext;
use http::StatusCode;
use serde_json::{Value, json};

use super::day::{self, DayError};

/// The router of the API's one route. The feature context is the route's state.
pub fn routes(features: &Arc<FeatureContext>) -> Router {
  Router::new().route("/api/approval/day", routing::get_only(day_api)).with_state(Arc::clone(features))
}

/// The paths the routes answer, for the trailing-slash redirect.
pub fn matches(path: &str) -> bool {
  path == "/api/approval/day"
}

/// `GET /api/approval/day?date=YYYY-MM-DD`: the day view. An absent `date` answers today's Pacific day.
async fn day_api(
  State(features): State<Arc<FeatureContext>>,
  Query(query): Query<HashMap<String, String>>,
) -> Response {
  let date = match query.get("date").map(String::as_str) {
    None => None,
    Some(text) => match day::parse_date_param(text) {
      Ok(date) => Some(date),
      Err(error) => return answer(Err(error)),
    },
  };
  answer(day::approval_day(&features, date).await)
}

/// A handler's answer: the body with 200, a refusal with its status and detail, or the unhandled-exception 500.
fn answer(result: Result<Value, DayError>) -> Response {
  match result {
    Ok(body) => responses::json(StatusCode::OK, &body),
    Err(DayError::Refused(code, detail)) => responses::json(code, &json!({"detail": detail})),
    Err(DayError::Failed(error)) => responses::internal_error("RuntimeError", format!("{error:#}")),
  }
}
