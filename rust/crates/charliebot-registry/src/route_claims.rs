//! The paths a router's routes answer, built from the route patterns the router mounts.
//!
//! The trailing-slash redirect asks every router whether it claims the slash-toggled path. A parameter in a
//! route pattern captures one non-empty segment, and matchit also accepts an empty capture, so a match whose
//! capture is empty is not a claim.

use matchit::Router;

/// The route patterns of one router, in axum's syntax, such as `/api/sessions/{session_id}/retry`.
pub struct RouteClaims {
  routes: Router<()>,
}

impl RouteClaims {
  /// Build the claims from the patterns a router mounts. Fails when two patterns conflict.
  pub fn new(patterns: &[&str]) -> Result<Self, matchit::InsertError> {
    let mut routes = Router::new();
    for pattern in patterns {
      routes.insert(*pattern, ())?;
    }
    Ok(Self { routes })
  }

  /// Whether a decoded request path falls under one of the patterns, whatever the method.
  pub fn claims(&self, path: &str) -> bool {
    self.routes.at(path).is_ok_and(|found| found.params.iter().all(|(_, segment)| !segment.is_empty()))
  }
}

#[cfg(test)]
mod tests {
  use super::RouteClaims;

  #[test]
  fn a_parameter_claims_one_non_empty_segment() {
    let claims = RouteClaims::new(&["/api/x/{id}/run", "/api/y"]).unwrap();
    assert!(claims.claims("/api/x/a/run"));
    assert!(claims.claims("/api/y"));
    for path in ["/api/x//run", "/api/x/a/run/", "/api/y/"] {
      assert!(!claims.claims(path), "{path} should not be claimed");
    }
  }
}
