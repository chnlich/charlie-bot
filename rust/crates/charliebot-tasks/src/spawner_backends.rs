//! Backend-resolution refusals used by the manager spawner.

/// Return the refusal core when a session pins a backend id absent from `config.yaml`.
pub fn unknown_backend_pin_refusal(backend_id: &str, fallback_id: &str) -> String {
  format!("'{backend_id}' is not in config.yaml backends.options — refusing to substitute '{fallback_id}'")
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn refusal_names_the_missing_pin_and_rejected_substitute() {
    assert_eq!(
      unknown_backend_pin_refusal("synthetic-missing", "synthetic-default"),
      "'synthetic-missing' is not in config.yaml backends.options — refusing to substitute 'synthetic-default'"
    );
  }
}
