//! The one resolver for the option a call runs on: a `backends.calls` class or a backend id, picked by the server's pool
//! selection.
//!
//! `POST /api/backends/resolve` and the `charliebot backend` verbs both resolve here, so every caller reads the same
//! pick. A refusal names the key or the id to change.

use charliebot_core::config::{BackendOption, CallClass, CallTarget, CharlieBotConfig};

use crate::model_pool::Selector;

/// The path of the resolve route: the server answers it, and the `backend` verbs call it.
pub const RESOLVE_PATH: &str = "/api/backends/resolve";

/// Why a resolution gave no option: a refusal that names what to change, or a failure of the pool state.
#[derive(Debug)]
pub enum ResolveError {
  /// The key is unset, the id is unknown, or no member of the pool is selectable.
  Refused(String),
  /// The pool selection itself failed.
  Failed(anyhow::Error),
}

/// The option a call of `class` runs on. `author` is the backend of the run that asks: an `improve_review` call leaves
/// that member out first, and falls back to it only when no other member in the target is selectable.
pub fn resolve_call(
  cfg: &CharlieBotConfig,
  selector: &Selector<'_>,
  class: CallClass,
  author: Option<&str>,
) -> Result<BackendOption, ResolveError> {
  let id = match cfg.backends.calls.get(class) {
    None => {
      return Err(ResolveError::Refused(format!(
        "backends.calls.{} is unset; name a backends.options or backends.pools id for it in config.yaml",
        class.key()
      )));
    }
    Some(CallTarget::Backend(id)) => id.as_str(),
    Some(CallTarget::Session) => {
      let member = author.ok_or_else(|| {
        ResolveError::Refused(format!(
          "backends.calls.{} is 'session', which requires a session run token",
          class.key()
        ))
      })?;
      return select(selector, member, &[]);
    }
  };
  if class == CallClass::ImproveReview
    && let Some(author) = author
    && let Some(option) = selector.select(id, &[author.to_string()]).map_err(ResolveError::Failed)?
  {
    return Ok(option);
  }
  select(selector, id, &[])
}

/// The option a backend id names: an option that selects itself, or a pool's selectable member.
pub fn resolve_backend(
  cfg: &CharlieBotConfig,
  selector: &Selector<'_>,
  id: &str,
) -> Result<BackendOption, ResolveError> {
  cfg.backends.check_backend_id(id).map_err(ResolveError::Refused)?;
  select(selector, id, &[])
}

fn select(selector: &Selector<'_>, id: &str, excluded: &[String]) -> Result<BackendOption, ResolveError> {
  match selector.select(id, excluded).map_err(ResolveError::Failed)? {
    Some(option) => Ok(option),
    None => Err(ResolveError::Refused(selector.exhausted_message(id).map_err(ResolveError::Failed)?)),
  }
}

impl std::fmt::Display for ResolveError {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      ResolveError::Refused(message) => formatter.write_str(message),
      ResolveError::Failed(error) => write!(formatter, "{error:#}"),
    }
  }
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use charliebot_core::config::{ConfigSchema, load_config};
  use charliebot_registry::Registry;

  use super::*;
  use crate::backend_availability::BackendAvailability;
  use crate::model_pool::RotationPositions;

  fn load(yaml: &str) -> (tempfile::TempDir, CharlieBotConfig) {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("config.yaml"), yaml).unwrap();
    let schema = ConfigSchema { sections: Vec::new(), legacy_keys: Vec::new(), backend_types: vec!["synthetic"] };
    let cfg = load_config(home.path(), &schema).unwrap();
    (home, cfg)
  }

  #[test]
  fn improve_review_excludes_author_first_and_falls_back_when_no_other_is_selectable() {
    let (home, cfg) = load(
      "backends:\n  options:\n    - {id: a, label: A, type: synthetic, model: m}\n    - {id: b, label: B, type: synthetic, model: m}\n  pools:\n    - {id: rev, label: Rev, rule: rotate, members: [a, b]}\n  calls:\n    improve_review: rev\n",
    );
    let registry = Registry::new();
    let availability = Arc::new(BackendAvailability::load(home.path()).unwrap());
    let positions = Arc::new(RotationPositions::default());
    let selector = Selector::new(&cfg, &registry, Arc::clone(&availability), positions);

    let picked = resolve_call(&cfg, &selector, CallClass::ImproveReview, Some("a")).unwrap();
    assert_eq!(picked.id, "b");

    // When `b` is unavailable, fall back to `a` instead of failing on a single selectable member.
    let future =
      charliebot_store::metadata::UtcDateTime::from(time::OffsetDateTime::now_utc() + time::Duration::hours(1));
    availability.record(&cfg, &registry, "b", Some(future), "quota_error", "exhausted").unwrap();
    let fallback = resolve_call(&cfg, &selector, CallClass::ImproveReview, Some("a")).unwrap();
    assert_eq!(fallback.id, "a");
  }

  #[test]
  fn session_call_target_refuses_without_a_session_and_resolves_with_one() {
    let (home, cfg) = load(
      "backends:\n  options:\n    - {id: a, label: A, type: synthetic, model: m}\n  calls:\n    review: session\n",
    );
    let registry = Registry::new();
    let availability = Arc::new(BackendAvailability::load(home.path()).unwrap());
    let positions = Arc::new(RotationPositions::default());
    let selector = Selector::new(&cfg, &registry, availability, positions);

    let err = resolve_call(&cfg, &selector, CallClass::Review, None).unwrap_err();
    assert!(matches!(err, ResolveError::Refused(ref msg) if msg.contains("backends.calls.review")), "{err}");

    let picked = resolve_call(&cfg, &selector, CallClass::Review, Some("a")).unwrap();
    assert_eq!(picked.id, "a");
  }

  /// Each of the four call classes T1 adds refuses an unset key, naming the key to set rather than falling back.
  #[test]
  fn each_new_call_class_refuses_its_unset_key_by_name() {
    let (home, cfg) = load("backends:\n  options:\n    - {id: a, label: A, type: synthetic, model: m}\n");
    let registry = Registry::new();
    let availability = Arc::new(BackendAvailability::load(home.path()).unwrap());
    let positions = Arc::new(RotationPositions::default());
    let selector = Selector::new(&cfg, &registry, availability, positions);

    for class in [CallClass::ImproveReview, CallClass::Compaction, CallClass::WebSearch, CallClass::Judge] {
      let err = resolve_call(&cfg, &selector, class, Some("a")).unwrap_err();
      let key = format!("backends.calls.{}", class.key());
      assert!(matches!(err, ResolveError::Refused(ref message) if message.contains(&key)), "{class:?}: {err}");
    }
  }
}
