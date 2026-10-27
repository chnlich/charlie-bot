//! One route family: its axum routes and its path matcher.

use std::sync::Arc;

use axum::Router;

use crate::state::AppState;

/// One route family.
pub struct Family {
  /// The family's routes.
  pub routes: fn() -> Router<Arc<AppState>>,
  /// Whether a decoded HTTP path falls under one of the family's routes, whatever the method.
  pub matches: fn(&AppState, &str) -> bool,
}
