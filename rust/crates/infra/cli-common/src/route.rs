//! Fills a route pattern's `{name}` parameters into the request path that a client sends.

/// The route pattern with its `{name}` parameters replaced by `values`, in order. The owner's pattern and the caller's
/// values must agree in count, so a mismatch panics.
pub fn fill(pattern: &str, values: &[&str]) -> String {
  let mut values = values.iter();
  let mut parts = pattern.split('{');
  let mut path = parts.next().unwrap_or_default().to_string();
  for part in parts {
    let (_, literal) = part.split_once('}').expect("a route parameter closes with `}`");
    path.push_str(values.next().expect("a value for each route parameter"));
    path.push_str(literal);
  }
  assert!(values.next().is_none(), "a value for no route parameter in {pattern}");
  path
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn fill_replaces_each_parameter_in_order() {
    assert_eq!(
      fill("/api/sessions/{session_id}/runs/{run_id}/cancel", &["s1", "r1"]),
      "/api/sessions/s1/runs/r1/cancel"
    );
    assert_eq!(fill("/api/sessions/", &[]), "/api/sessions/");
  }
}
