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
/// that member out, so a review never runs on the backend that wrote the change. Other classes ignore it.
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
    Some(CallTarget::Backend(id)) => id,
    Some(CallTarget::Session) => unreachable!("the config loader refuses session on the call classes resolved here"),
  };
  let excluded: Vec<String> = match (class, author) {
    (CallClass::ImproveReview, Some(author)) => vec![author.to_string()],
    _ => Vec::new(),
  };
  select(selector, id, &excluded)
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
