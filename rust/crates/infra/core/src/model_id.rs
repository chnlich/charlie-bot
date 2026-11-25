//! The family of a model id: `fable` for `claude-fable-5-1`, and `gpt` for `gpt-5.5`.
//!
//! The usage panel scopes its windows by family, compaction matches a model by family, and the store's compaction
//! note names the family. This module is the one reader of that rule.

/// The lowercase family of `model`: the segment after a leading `claude-`, else the id's first dash segment.
/// An absent or empty model has no family and returns an empty string.
pub fn family(model: Option<&str>) -> String {
  let Some(model) = model.filter(|model| !model.is_empty()) else {
    return String::new();
  };
  let parts = model.to_lowercase().split('-').map(str::to_string).collect::<Vec<_>>();
  if parts.first().is_some_and(|part| part == "claude") && parts.len() > 1 {
    parts[1].clone()
  } else {
    parts[0].clone()
  }
}
