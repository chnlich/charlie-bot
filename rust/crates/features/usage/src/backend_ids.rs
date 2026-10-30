//! Usage's startup check (`src/features/usage/backend_ids.py`): every `backends.options` id attributes to its type's
//! usage source. The usage page attributes a charlie-bot row's account ids to the CLI that ran the call: a registered
//! id through its config type, an id that left the config through the id prefix its source registers. An id outside
//! every registered prefix would fail the page once it leaves the config, so the server refuses such an option at
//! start.

use charliebot_core::config::CharlieBotConfig;
use charliebot_core::{paths, text_repr};
use charliebot_registry::Registry;

/// `check_backend_ids`: `Err` lists every `backends.options` entry whose id its type's usage source cannot attribute.
/// One error carries every violation, and each line names the config file, the options index, the id, the type and the
/// allowed prefixes.
pub fn check_backend_ids(registry: &Registry, cfg: &CharlieBotConfig) -> Result<(), String> {
  let config_file = paths::display(&cfg.config_file());
  let mut problems = Vec::new();
  for (index, option) in cfg.backends.options.iter().enumerate() {
    match registry.usage_source_for(&option.backend_type).map_err(|error| error.to_string())? {
      None => problems.push(format!(
        "{config_file}: backends.options[{index}] id '{}' type '{}' attributes to no usage source",
        option.id, option.backend_type
      )),
      Some(source) if !source.id_prefixes.iter().any(|prefix| option.id.starts_with(prefix)) => problems.push(format!(
        "{config_file}: backends.options[{index}] id '{}' type '{}' starts with none of the id prefixes {} \
           registers {}",
        option.id,
        option.backend_type,
        source.name,
        python_tuple(source.id_prefixes)
      )),
      Some(_) => {}
    }
  }
  if problems.is_empty() {
    return Ok(());
  }
  Err(format!(
    "backend option ids must attribute to their type's usage source \
     (id rule: BackendsConfig in src/infra/config.py):\n{}",
    problems.iter().map(|problem| format!("  {problem}")).collect::<Vec<_>>().join("\n")
  ))
}

/// `str()` of a tuple of strings: each item in `repr` form, and a one-item tuple keeps its trailing comma.
fn python_tuple(items: &[&str]) -> String {
  let reprs: Vec<String> = items.iter().map(|item| text_repr::str_repr(item)).collect();
  match reprs.as_slice() {
    [only] => format!("({only},)"),
    _ => format!("({})", reprs.join(", ")),
  }
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::path::Path;

  use charliebot_core::config::{self, ConfigSchema};

  use super::*;
  use crate::test_support::registry;

  const BACKEND_TYPES: &[&str] = &["charlie-code", "codex", "cc-claude", "opencode", "antigravity", "gemini"];

  /// The config of one home, with `options` as its `backends.options` entries, each `(id, type, model)`.
  fn config(home: &Path, options: &[(&str, &str, &str)]) -> CharlieBotConfig {
    let mut yaml = String::from("backends:\n  options:\n");
    for (id, backend_type, model) in options {
      yaml.push_str(&format!("    - id: {id}\n      label: {id}\n      type: {backend_type}\n      model: {model}\n"));
    }
    fs::create_dir_all(home).expect("the home directory is created");
    fs::write(home.join(config::CONFIG_FILENAME), yaml).expect("the config is written");
    let schema = ConfigSchema { backend_types: BACKEND_TYPES.to_vec(), ..ConfigSchema::default() };
    config::load_config(home, &schema).expect("the config loads")
  }

  #[test]
  fn an_id_per_source_passes_whatever_prefix_spelling_it_uses() {
    let home = tempfile::tempdir().expect("a temporary home");
    let cfg = config(
      home.path(),
      &[
        ("codex-gpt-sol", "codex", "gpt-6.1-sol"),
        ("claude-opus-5", "cc-claude", "claude-opus-5-5"),
        ("oc-kimi-k3", "opencode", "kimi-k3"),
        ("clc-glm5-flash", "charlie-code", "glm"),
        ("agy-antigravity", "antigravity", "gemini-2.5-pro"),
      ],
    );
    assert_eq!(check_backend_ids(&registry(), &cfg), Ok(()));
  }

  #[test]
  fn an_id_outside_its_type_prefixes_is_rejected_and_every_violation_is_listed() {
    let home = tempfile::tempdir().expect("a temporary home");
    let cfg = config(
      home.path(),
      &[
        ("codex-gpt-sol", "codex", "gpt-6.1-sol"),
        ("glm-air", "charlie-code", "glm-air"),
        ("kimi-oc", "opencode", "kimi-k3"),
        ("gpt-plain", "codex", "gpt-6"),
      ],
    );
    let message = check_backend_ids(&registry(), &cfg).expect_err("three options attribute to no prefix");
    let config_file = paths::display(&cfg.config_file());
    assert!(message.contains(&format!("{config_file}: backends.options[1] id 'glm-air' type 'charlie-code'")));
    assert!(message.contains("('charlie-code-', 'clc-')"));
    assert!(message.contains("backends.options[2] id 'kimi-oc' type 'opencode'"));
    assert!(message.contains("('opencode-', 'oc-')"));
    assert!(message.contains("backends.options[3] id 'gpt-plain' type 'codex'"));
    assert!(message.contains("('codex-',)"));
    assert!(!message.contains("backends.options[0]"), "the passing codex entry is not listed: {message}");
  }

  #[test]
  fn a_type_with_no_usage_source_is_rejected() {
    let home = tempfile::tempdir().expect("a temporary home");
    let cfg = config(home.path(), &[("gem-pro", "gemini", "gemini-2")]);
    let message = check_backend_ids(&registry(), &cfg).expect_err("gemini registers no usage source");
    assert!(
      message.contains("backends.options[0] id 'gem-pro' type 'gemini' attributes to no usage source"),
      "{message}"
    );
  }

  #[test]
  fn a_tuple_of_prefixes_prints_as_python_prints_it() {
    assert_eq!(python_tuple(&["codex-"]), "('codex-',)");
    assert_eq!(python_tuple(&["charlie-code-", "clc-"]), "('charlie-code-', 'clc-')");
    assert_eq!(python_tuple(&[]), "()");
  }
}
