//! The match definition: the classes of a hardcoded backend choice, and the words that name each class.
//!
//! The Rust scan (`rust_scan`) and the text scan (`text_scan`) both ask this module whether a word names a backend, so
//! the two scans share one definition. The ids and hosts come from the loaded configuration (`vocabulary`); the family
//! words and the command names are the constants below.

use std::collections::BTreeSet;

use crate::vocabulary::Vocabulary;

/// The classes of a hardcoded backend choice. A site falls in the first class that its word matches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Class {
  /// A `backends.options` id, or a word that a registered id prefix and a family name make.
  OptionId,
  /// A `backends.pools` id, in a position that selects a backend.
  PoolId,
  /// A configured or priced model id, or a family name, bare or behind a provider prefix such as `openai/`.
  ModelId,
  /// The URL of a configured endpoint, or of a provider host.
  Gateway,
  /// A backend CLI name as a program's first argument or a constant's value, with the argument that makes a call.
  FixedCommand,
  /// A backend word on the right of a default operator.
  DefaultValue,
  /// A non-Rust code file that reads the `backends` section of `config.yaml` itself.
  ConfigBypass,
}

impl Class {
  /// The class name a report line prints.
  pub const fn label(self) -> &'static str {
    match self {
      Class::OptionId => "option-id",
      Class::PoolId => "pool-id",
      Class::ModelId => "model-id",
      Class::Gateway => "gateway",
      Class::FixedCommand => "fixed-command",
      Class::DefaultValue => "default-value",
      Class::ConfigBypass => "config-bypass",
    }
  }
}

/// One site a scan found: its line, its class, and the word that names the backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
  pub line: usize,
  pub class: Class,
  pub value: String,
}

/// The backend CLI names: the programs a backend type runs.
pub const FIXED_COMMANDS: [&str; 6] = ["charlie-code", "claude", "codex", "opencode", "devin", "gemini"];

/// The words that make a backend CLI run a model call: a print or prompt flag, or a subcommand that runs one.
pub const CALL_MARKERS: [&str; 6] = ["-p", "--print", "exec", "run", "--model", "--json"];

/// The Claude family names: `claude-` followed by one of them is a model id.
const CLAUDE_FAMILIES: [&str; 4] = ["opus", "sonnet", "haiku", "fable"];

/// The model family prefixes: a word that starts with one and then has a name is a model id.
const MODEL_FAMILY_PREFIXES: [&str; 7] = ["gemini-", "gpt-", "kimi-", "deepseek-", "glm-", "muse-", "jev-"];

/// The family names that mark a segment of an option id made from a registered prefix, as in `claude-code-kimi-...`.
const FAMILY_SEGMENTS: [&str; 11] =
  ["opus", "sonnet", "haiku", "fable", "gemini", "gpt", "kimi", "deepseek", "glm", "muse", "jev"];

/// The class of a word that names a backend by value, or `None`. A model id is checked before an option id made from a
/// prefix, so a word such as `claude-opus-5` is a model id.
pub fn value_class(vocab: &Vocabulary, word: &str) -> Option<Class> {
  if vocab.options.contains(word) {
    Some(Class::OptionId)
  } else if vocab.models.contains(word) || is_model_family(word) {
    Some(Class::ModelId)
  } else if has_option_family(&vocab.id_prefixes, word) {
    Some(Class::OptionId)
  } else {
    None
  }
}

/// Whether a word names a backend that a default operand may hold: an option, model or pool id, or a backend CLI name.
pub fn names_backend(vocab: &Vocabulary, word: &str) -> bool {
  value_class(vocab, word).is_some() || vocab.pools.contains(word) || fixed_command(word).is_some()
}

/// The backend CLI name that a word names, with its directory stripped: `claude` for `~/.local/bin/claude`.
pub fn fixed_command(word: &str) -> Option<&'static str> {
  let name = word.rsplit_once('/').map_or(word, |(_, name)| name);
  FIXED_COMMANDS.into_iter().find(|command| *command == name)
}

/// Whether a word is a model id by its family: `claude-` and a family name, or a family prefix and a name after it. A
/// provider prefix such as `openai/` may come first.
pub fn is_model_family(word: &str) -> bool {
  let name = word.rsplit_once('/').map_or(word, |(_, name)| name);
  if let Some(rest) = name.strip_prefix("claude-") {
    return CLAUDE_FAMILIES
      .iter()
      .any(|family| rest.strip_prefix(family).is_some_and(|tail| tail.is_empty() || tail.starts_with(['-', '.'])));
  }
  MODEL_FAMILY_PREFIXES
    .iter()
    .any(|prefix| name.strip_prefix(prefix).is_some_and(|tail| tail.starts_with(|c: char| c.is_ascii_alphanumeric())))
}

/// Whether a word is an option id made from a registered id prefix and a family segment: `claude-code-kimi-k3.1-pro`
/// under the prefix `claude-`.
pub fn has_option_family(prefixes: &BTreeSet<String>, word: &str) -> bool {
  prefixes.iter().any(|prefix| {
    word
      .strip_prefix(prefix.as_str())
      .is_some_and(|rest| rest.split('-').any(|segment| FAMILY_SEGMENTS.contains(&segment)))
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  fn vocab() -> Vocabulary {
    Vocabulary {
      options: ["claude-fable-5-1"].into_iter().map(str::to_string).collect(),
      pools: ["cheap"].into_iter().map(str::to_string).collect(),
      models: ["claude-opus-5-5"].into_iter().map(str::to_string).collect(),
      hosts: BTreeSet::new(),
      id_prefixes: ["claude-", "codex-"].into_iter().map(str::to_string).collect(),
    }
  }

  /// A model family word names a model id bare or behind a provider prefix, and a bare family prefix names none.
  #[test]
  fn model_family_patterns_name_model_ids() {
    for word in ["claude-opus", "claude-haiku-5-5", "gemini-4-hydrogen", "openai/glm-5.3-flash", "models/gemini-3.5"] {
      assert!(is_model_family(word), "{word} should name a model");
    }
    for word in ["gemini-", "chatgpt-4", "claude-code", "claude-opusx"] {
      assert!(!is_model_family(word), "{word} should name no model");
    }
  }

  /// An option id is a registered prefix with a family segment after it, and a configured id needs no prefix.
  #[test]
  fn option_ids_come_from_a_registered_prefix_and_a_family_segment() {
    let vocab = vocab();
    assert_eq!(value_class(&vocab, "claude-code-kimi-k3.1-pro"), Some(Class::OptionId));
    assert_eq!(value_class(&vocab, "claude-fable-5-1"), Some(Class::OptionId));
    assert_eq!(value_class(&vocab, "claude-opus-5-5"), Some(Class::ModelId));
    assert_eq!(value_class(&vocab, "claude-code-search"), None);
    assert_eq!(value_class(&vocab, "cheap"), None, "a pool id names a backend only in a selecting position");
  }
}
