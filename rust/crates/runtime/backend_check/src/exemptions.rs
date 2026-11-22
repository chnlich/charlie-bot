//! The exemption table: the reviewed sites that a class matches and that stay as they are. It has one row per owned
//! value.
//!
//! A row exempts a hit by its file and its value, never by its line, so another value in the same file still refuses.
//! Each row names the owner that keeps the value.

use crate::rules::Class;

/// The values that one row exempts in its file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Values {
  /// These exact words: a constant's value, or a directory or kind name.
  Words(&'static [&'static str]),
  /// A backend CLI run that carries no model, option or pool: the program's name and the marker that follows it.
  Invocation { command: &'static str, marker: &'static str },
  /// Any model id in the file. The file owns the model spellings it holds.
  AnyModel,
}

/// One exempt site class: the file that owns the values, the values, and the owner that keeps them.
#[derive(Debug)]
pub struct Exemption {
  /// The file's path under the root it is read from: the repository root, or the profile home.
  pub path: &'static str,
  pub values: Values,
  pub owner: &'static str,
}

/// The six reviewed rows. Each row's values are the sites that the match definition finds and that the owner keeps.
static EXEMPTIONS: [Exemption; 6] = [
  Exemption {
    path: "scripts/sync-skills.sh",
    values: Values::Invocation { command: "claude", marker: "-p" },
    owner: "skill discovery probe: tests the local Claude Code's skill discovery, with no model",
  },
  Exemption {
    path: "scripts/host-auth/claude_host2_readback.py",
    values: Values::Invocation { command: "claude", marker: "-p" },
    owner: "host login readback: one turn on the local login, with no model",
  },
  Exemption {
    path: "rust/crates/infra/core/src/backend_types.rs",
    values: Values::Words(&["charlie-code", "codex"]),
    owner: "backend type names: each type's one name, which its backend crate registers",
  },
  Exemption {
    path: "rust/crates/infra/core/src/home.rs",
    values: Values::Words(&["opencode"]),
    owner: "profile directory names: OPENCODE_DIR_NAME",
  },
  Exemption {
    path: "rust/crates/runtime/registry/src/usage.rs",
    values: Values::Words(&["claude"]),
    owner: "usage reading kinds: CLAUDE_READING_KIND, a read of usage that is no LLM call",
  },
  Exemption {
    path: "rust/crates/infra/core/src/list_price.rs",
    values: Values::AnyModel,
    owner: "price table: the ledger's model spellings and aliases",
  },
];

/// The row that exempts a site, or `None` when the site must be fixed. `path` is the site's file under its root.
pub fn find(path: &str, class: Class, value: &str) -> Option<&'static Exemption> {
  EXEMPTIONS.iter().find(|row| row.path == path && row.values.covers(class, value))
}

impl Values {
  /// Whether a site of `class` with `value` is one of these values. A row exempts its class only, so a value that
  /// names a backend in another position stays refused.
  fn covers(self, class: Class, value: &str) -> bool {
    match self {
      Values::Words(words) => class == Class::FixedCommand && words.contains(&value),
      Values::Invocation { command, marker } => {
        class == Class::FixedCommand && value.split_once(' ') == Some((command, marker))
      }
      Values::AnyModel => class == Class::ModelId,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The row for `path` exempts `value` of `class` in its file, and each refused site of the same file stays a hit.
  fn assert_row(path: &str, class: Class, value: &str, refused: &[(Class, &str)]) {
    assert!(find(path, class, value).is_some(), "{path} should exempt {value}");
    for (refused_class, refused_value) in refused {
      assert!(find(path, *refused_class, refused_value).is_none(), "{path} must still refuse {refused_value}");
    }
  }

  #[test]
  fn sync_skills_probe_row_exempts_its_invocation_only() {
    assert_row(
      "scripts/sync-skills.sh",
      Class::FixedCommand,
      "claude -p",
      &[
        (Class::FixedCommand, "claude -p --model claude-opus-5"),
        (Class::ModelId, "claude-opus-5"),
        (Class::FixedCommand, "codex"),
      ],
    );
  }

  #[test]
  fn host_login_readback_row_exempts_its_invocation_only() {
    assert_row(
      "scripts/host-auth/claude_host2_readback.py",
      Class::FixedCommand,
      "claude -p",
      &[(Class::FixedCommand, "claude -p --model claude-opus-5"), (Class::FixedCommand, "codex")],
    );
  }

  #[test]
  fn backend_type_names_row_exempts_each_type_name() {
    let path = "rust/crates/infra/core/src/backend_types.rs";
    assert_row(path, Class::FixedCommand, "charlie-code", &[(Class::FixedCommand, "claude")]);
    assert_row(path, Class::FixedCommand, "codex", &[(Class::FixedCommand, "claude")]);
  }

  #[test]
  fn profile_directory_name_row_exempts_its_word_only() {
    assert_row(
      "rust/crates/infra/core/src/home.rs",
      Class::FixedCommand,
      "opencode",
      &[(Class::FixedCommand, "claude"), (Class::Gateway, "opencode")],
    );
  }

  #[test]
  fn usage_kind_row_exempts_the_claude_word_only() {
    assert_row(
      "rust/crates/runtime/registry/src/usage.rs",
      Class::FixedCommand,
      "claude",
      &[(Class::FixedCommand, "claude -p")],
    );
  }

  #[test]
  fn price_table_row_exempts_any_model_only() {
    assert_row(
      "rust/crates/infra/core/src/list_price.rs",
      Class::ModelId,
      "gemini-4-argon-high",
      &[(Class::FixedCommand, "claude")],
    );
  }

  /// Every row names an owner, and the six rows are the reviewed set.
  #[test]
  fn every_row_names_its_owner() {
    assert_eq!(EXEMPTIONS.len(), 6);
    assert!(EXEMPTIONS.iter().all(|row| !row.owner.is_empty()));
  }
}
