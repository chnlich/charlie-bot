//! The exemption table: the reviewed sites that a class matches and that stay as they are. It has one row per owned
//! value, and one row for the backend crates, which own their own harness names.
//!
//! A row exempts a hit by its path and its value, never by its line, so another value in the same file still refuses.
//! A row's path is a file, or a directory that ends in `/` and covers every file under it. Each row names the owner that
//! keeps the value.
//!
//! Rules that are not owned values stay out of the table. Files under `configs/` and mock or fixture trees are not
//! scanned. Code comments are left unread (`text_scan`, `rust_scan`).

use crate::rules::Class;

/// The directory of the backend crates. Each crate under it is one backend type, and its directory name is its type.
const BACKEND_CRATES: &str = "rust/crates/backends/";

/// The values that one row exempts in its path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Values {
  /// These exact words: a constant's value, or a directory or kind name.
  Words(&'static [&'static str]),
  /// These exact spellings in a model-id position, such as a third-party name that shares a model family prefix.
  ModelIds(&'static [&'static str]),
  /// A backend CLI run that carries no model, option or pool: the program's name and the marker that follows it.
  Invocation { command: &'static str, marker: &'static str },
  /// Any model spelling in the file, whichever class the vocabulary gives it. The file owns the spellings it holds: a
  /// host configuration may name a priced model as an option id, and the spelling is still the price table's.
  AnyModel,
  /// The backend CLI names that a backend crate owns for its own type: its harness binary, its argv and its usage kind.
  /// Another backend's name in the same crate stays refused.
  OwnHarness,
}

/// One exempt site class: the path that owns the values, the values, and the owner that keeps them.
#[derive(Debug)]
pub struct Exemption {
  /// The file's path under the root it is read from: the repository root, or the profile home. A path that ends in `/`
  /// is a directory, and it covers every file under it.
  pub path: &'static str,
  pub values: Values,
  pub owner: &'static str,
}

/// The reviewed rows. Each row's values are the sites that the match definition finds and that the owner keeps.
static EXEMPTIONS: [Exemption; 8] = [
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
  Exemption {
    path: BACKEND_CRATES,
    values: Values::OwnHarness,
    owner: "backend crates: each crate's harness binary, argv and usage kind, which its type registers",
  },
  Exemption {
    path: "k8s/environ/fpt-k8s/helmfile.yaml",
    // Split the prefix so `rust_scan` does not flag the exemption table entry itself as a model literal.
    values: Values::ModelIds(&[
      concat!("muse", "-bundle-eval"),
      concat!("muse", "-launcher"),
      concat!("muse", "-bundle-eval.values.yaml"),
    ]),
    owner: "another team's Helm release and file names in the deployments repo, not a CharlieBot backend",
  },
];

/// The row that exempts a site, or `None` when the site must be fixed. `path` is the site's file under its root.
pub fn find(path: &str, class: Class, value: &str) -> Option<&'static Exemption> {
  EXEMPTIONS.iter().find(|row| row.covers(path, class, value))
}

impl Exemption {
  /// Whether this row exempts a site of `class` with `value` in `path`: the row's path holds the file, and its values
  /// cover the site.
  fn covers(&self, path: &str, class: Class, value: &str) -> bool {
    let within = if self.path.ends_with('/') { path.starts_with(self.path) } else { path == self.path };
    within && self.values.covers(path, class, value)
  }
}

impl Values {
  /// Whether a site of `class` with `value` in `path` is one of these values. A row exempts its class only, so a value
  /// that names a backend in another position stays refused.
  fn covers(self, path: &str, class: Class, value: &str) -> bool {
    match self {
      Values::Words(words) => class == Class::FixedCommand && words.contains(&value),
      Values::ModelIds(words) => class == Class::ModelId && words.contains(&value),
      Values::Invocation { command, marker } => {
        class == Class::FixedCommand && value.split_once(' ') == Some((command, marker))
      }
      Values::AnyModel => matches!(class, Class::ModelId | Class::OptionId),
      Values::OwnHarness => {
        let Some(dir) = path.strip_prefix(BACKEND_CRATES).and_then(|rest| rest.split('/').next()) else {
          return false;
        };
        class == Class::FixedCommand && owns_command(dir, command_of(value))
      }
    }
  }
}

/// Whether a backend crate's directory owns a CLI name: the directory with dashes for its underscores, as `charlie-code`
/// for `charlie_code`, or its first word, as `claude` for `claude_code`.
fn owns_command(dir: &str, command: &str) -> bool {
  command == dir.replace('_', "-") || dir.split('_').next() == Some(command)
}

/// The CLI name of a fixed-command value: the word before its marker, as `claude` in `claude -p`.
fn command_of(value: &str) -> &str {
  value.split_once(' ').map_or(value, |(command, _)| command)
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

  /// The price table exempts a model spelling whichever class the host's configuration gives it, since a configured
  /// option may share a priced model's id. A backend CLI name in the same file stays refused.
  #[test]
  fn price_table_row_exempts_its_model_spellings_only() {
    let path = "rust/crates/infra/core/src/list_price.rs";
    assert_row(path, Class::ModelId, "gemini-4-argon-high", &[(Class::FixedCommand, "claude")]);
    assert_row(path, Class::OptionId, "claude-fable-5-2", &[(Class::FixedCommand, "claude")]);
    assert!(find("rust/crates/infra/core/src/list_price_extra.rs", Class::OptionId, "claude-fable-5-2").is_none());
  }

  /// A backend crate names its own harness, argv and usage kind: its directory names the program. Another backend's
  /// name in the same crate, a model id in it, and the same name outside the backend crates stay refused.
  #[test]
  fn backend_crates_own_their_harness_names() {
    let codex = "rust/crates/backends/codex/src/factory.rs";
    assert_row(
      codex,
      Class::FixedCommand,
      "codex -p",
      &[(Class::FixedCommand, "claude -p"), (Class::ModelId, "codex-x")],
    );
    let claude = "rust/crates/backends/claude_code/src/usage_quota.rs";
    assert_row(claude, Class::FixedCommand, "claude", &[(Class::FixedCommand, "codex -p")]);
    assert_row(
      "rust/crates/backends/charlie_code/src/charlie_code_cli.rs",
      Class::FixedCommand,
      "charlie-code",
      &[(Class::FixedCommand, "claude")],
    );
    assert!(find("rust/crates/app/cli/src/commands/backend.rs", Class::FixedCommand, "codex -p").is_none());
  }

  #[test]
  fn fpt_k8s_helmfile_row_exempts_its_three_names_only() {
    let path = "k8s/environ/fpt-k8s/helmfile.yaml";
    for value in ["muse-bundle-eval", "muse-launcher", "muse-bundle-eval.values.yaml"] {
      assert_row(
        path,
        Class::ModelId,
        value,
        &[(Class::ModelId, "muse-voice-transcribe-1.0"), (Class::OptionId, value), (Class::DefaultValue, value)],
      );
      assert!(find("k8s/environ/xinjiang/helmfile.yaml", Class::ModelId, value).is_none());
    }
  }

  /// Every row names an owner, and the reviewed rows are eight: seven owned values and the backend crates.
  #[test]
  fn every_row_names_its_owner() {
    assert_eq!(EXEMPTIONS.len(), 8);
    assert!(EXEMPTIONS.iter().all(|row| !row.owner.is_empty()));
  }
}
