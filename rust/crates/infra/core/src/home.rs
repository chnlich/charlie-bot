//! The profile home: the one reader of `CHARLIEBOT_HOME`.
//!
//! Every home fact a crate needs comes from this module: the profile home, the default home, and the explicit
//! home the terminal passes to its panes. The environment is read only in [`raw_env_value`]. The rules are pure
//! functions of the raw value, so tests pass that value in and set no environment variable.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::{paths, text_repr};

pub mod startup_rows;

pub const CHARLIEBOT_HOME_ENV: &str = "CHARLIEBOT_HOME";

/// The default home's directory name under the user's home, used when `CHARLIEBOT_HOME` is unset.
pub const DEFAULT_HOME_DIR_NAME: &str = ".charliebot";

/// The profile home as a message writes it: the variable's name in place of the machine-specific path.
pub const DISPLAY_HOME: &str = "$CHARLIEBOT_HOME";

/// A path under the profile home in the form [`DISPLAY_HOME`] writes. An empty `relative` is the home itself.
pub fn display_path(relative: &str) -> String {
  if relative.is_empty() { DISPLAY_HOME.to_string() } else { format!("{DISPLAY_HOME}/{relative}") }
}

/// A directory under the profile home in the form [`DISPLAY_HOME`] writes, with its trailing slash.
pub fn display_dir(relative: &str) -> String {
  format!("{}/", display_path(relative))
}

/// The profile's opencode store directory name, under the profile home.
pub const OPENCODE_DIR_NAME: &str = "opencode";
/// The `data` directory under the OpenCode store, which OpenCode itself lays out as its `XDG_DATA_HOME`.
pub const OPENCODE_XDG_DATA_DIR_NAME: &str = "data";
/// The `state` directory under the OpenCode store, which OpenCode itself lays out as its `XDG_STATE_HOME`.
pub const OPENCODE_XDG_STATE_DIR_NAME: &str = "state";

/// The directory under the profile home that holds the immutable rule bodies, one `<sha256>.md` per body.
pub const PROMPT_BODIES_DIR_NAME: &str = "prompt_bodies";

/// A `CHARLIEBOT_HOME` value that names no usable home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeError {
  raw: String,
}

impl fmt::Display for HomeError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(
      f,
      "{CHARLIEBOT_HOME_ENV} must be an absolute path or start with '~'; got {}",
      text_repr::str_repr(&self.raw)
    )
  }
}

impl std::error::Error for HomeError {}

/// The raw `CHARLIEBOT_HOME` value, or `""` when the variable is unset.
fn raw_env_value() -> String {
  // Unset reads as "".
  std::env::var_os(CHARLIEBOT_HOME_ENV).map(|value| value.to_string_lossy().into_owned()).unwrap_or_default()
}

/// The state directory this process belongs to.
///
/// `CHARLIEBOT_HOME` unset or blank selects [`default_charliebot_home`]. A set value must be absolute or start
/// with `~`, and resolves through `expanduser` then `realpath`. A relative value is an error, since
/// each process would resolve it against its own working directory.
pub fn charliebot_home_dir() -> Result<PathBuf, HomeError> {
  resolve_home(&raw_env_value())
}

/// The state directory used when `CHARLIEBOT_HOME` is unset: the user's home joined with
/// [`DEFAULT_HOME_DIR_NAME`], not resolved.
pub fn default_charliebot_home() -> PathBuf {
  PathBuf::from(paths::expanduser("~")).join(DEFAULT_HOME_DIR_NAME)
}

/// The profile home when `CHARLIEBOT_HOME` is set and not blank, or `None` when it is not.
///
/// This is the second read of the variable, separate from [`charliebot_home_dir`]. The terminal passes the home to
/// each new tmux pane only when a profile is set. A pane inherits the tmux server's environment rather than this
/// process's.
/// A set value that names no usable home is an error, as in [`charliebot_home_dir`].
pub fn explicit_charliebot_home() -> Result<Option<PathBuf>, HomeError> {
  explicit_home(&raw_env_value())
}

/// The home a raw `CHARLIEBOT_HOME` value names; a blank value names the default home.
pub(crate) fn resolve_home(raw: &str) -> Result<PathBuf, HomeError> {
  let raw = raw.trim();
  if raw.is_empty() {
    return Ok(default_charliebot_home());
  }
  if !raw.starts_with('~') && !raw.starts_with('/') {
    return Err(HomeError { raw: raw.to_string() });
  }
  Ok(paths::realpath(&PathBuf::from(paths::expanduser(raw))))
}

/// [`resolve_home`] for the explicit-home fact: a blank value gives `None` rather than the default home.
fn explicit_home(raw: &str) -> Result<Option<PathBuf>, HomeError> {
  if raw.trim().is_empty() {
    return Ok(None);
  }
  resolve_home(raw).map(Some)
}

/// The opencode 2.0 database, `<store>/data/opencode/opencode.db`: read by the usage source, vacuumed by its sweep.
pub fn opencode_db() -> Result<PathBuf, HomeError> {
  Ok(opencode_db_in(&charliebot_home_dir()?))
}

fn opencode_store_in(home: &Path) -> PathBuf {
  home.join(OPENCODE_DIR_NAME)
}

fn opencode_data_in(home: &Path) -> PathBuf {
  opencode_store_in(home).join(OPENCODE_XDG_DATA_DIR_NAME)
}

fn opencode_db_in(home: &Path) -> PathBuf {
  opencode_data_in(home).join("opencode").join("opencode.db")
}

/// The home's `state` directory: the writer fence's files and the files a boot leaves for the next boot.
pub const STATE_DIR_NAME: &str = "state";

/// The boot's recovery marker: the start of the last pass that reconciled the task tree, and the nodes whose pass
/// failed in it.
pub const BOOT_RECOVERY_FILE_NAME: &str = "boot_recovery.json";

/// The boot's skip proof: one row per task node the last pass skipped, with the identities its verdict was proven
/// from.
pub const BOOT_SKIP_PROOF_FILE_NAME: &str = "boot_skip_proof.json";

/// The boot's trigger index: one row per trigger file a boot proved not pending.
pub const BOOT_TRIGGER_INDEX_FILE_NAME: &str = "boot_trigger_index.json";

/// The backends' persisted usage availability: one row per backend option id that is out until a time.
pub const BACKEND_AVAILABILITY_FILE_NAME: &str = "backend_availability.json";

/// The home's `state` directory.
pub fn state_dir(home: &Path) -> PathBuf {
  home.join(STATE_DIR_NAME)
}

/// The path of `file_name` in the home's `state` directory, relative to the home: the form a home walk reports.
pub fn state_rel_path(file_name: &str) -> String {
  format!("{STATE_DIR_NAME}/{file_name}")
}

/// The boot's recovery marker.
pub fn boot_recovery_marker_path(home: &Path) -> PathBuf {
  state_dir(home).join(BOOT_RECOVERY_FILE_NAME)
}

/// The boot's skip proof.
pub fn boot_skip_proof_path(home: &Path) -> PathBuf {
  state_dir(home).join(BOOT_SKIP_PROOF_FILE_NAME)
}

/// The boot's trigger index.
pub fn boot_trigger_index_path(home: &Path) -> PathBuf {
  state_dir(home).join(BOOT_TRIGGER_INDEX_FILE_NAME)
}

/// The backends' persisted usage availability.
pub fn backend_availability_path(home: &Path) -> PathBuf {
  state_dir(home).join(BACKEND_AVAILABILITY_FILE_NAME)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn opencode_store_paths_derive_from_the_profile() {
    let home = Path::new("/synthetic/profile");
    assert_eq!(opencode_store_in(home), PathBuf::from("/synthetic/profile/opencode"));
    assert_eq!(opencode_data_in(home), PathBuf::from("/synthetic/profile/opencode/data"));
    assert_eq!(opencode_db_in(home), PathBuf::from("/synthetic/profile/opencode/data/opencode/opencode.db"));
  }

  #[test]
  fn boot_state_paths_derive_from_the_profile() {
    let home = Path::new("/synthetic/profile");
    assert_eq!(state_dir(home), PathBuf::from("/synthetic/profile/state"));
    assert_eq!(boot_recovery_marker_path(home), PathBuf::from("/synthetic/profile/state/boot_recovery.json"));
    assert_eq!(boot_skip_proof_path(home), PathBuf::from("/synthetic/profile/state/boot_skip_proof.json"));
    assert_eq!(boot_trigger_index_path(home), PathBuf::from("/synthetic/profile/state/boot_trigger_index.json"));
    assert_eq!(state_rel_path(BOOT_SKIP_PROOF_FILE_NAME), format!("{STATE_DIR_NAME}/{BOOT_SKIP_PROOF_FILE_NAME}"));
    assert_eq!(backend_availability_path(home), PathBuf::from("/synthetic/profile/state/backend_availability.json"));
  }

  #[test]
  fn a_relative_home_is_refused_with_the_message() {
    let error = resolve_home("rel/home").unwrap_err();
    assert_eq!(error.to_string(), "CHARLIEBOT_HOME must be an absolute path or start with '~'; got 'rel/home'");
  }

  #[test]
  fn an_absolute_home_resolves() {
    let dir = tempfile::tempdir().unwrap();
    let home = resolve_home(dir.path().to_str().unwrap()).unwrap();
    assert_eq!(home, paths::realpath(dir.path()));
  }

  #[test]
  fn a_trailing_slash_does_not_change_the_home() {
    let dir = tempfile::tempdir().unwrap();
    let profile = dir.path().join("profile");
    std::fs::create_dir(&profile).unwrap();
    assert_eq!(resolve_home(&format!("{}/", profile.display())).unwrap(), paths::realpath(&profile));
  }

  #[test]
  fn a_blank_home_selects_the_default_home() {
    let default = PathBuf::from(paths::expanduser("~")).join(DEFAULT_HOME_DIR_NAME);
    assert_eq!(default_charliebot_home(), default);
    assert_eq!(resolve_home("").unwrap(), default);
    assert_eq!(resolve_home("   ").unwrap(), default);
  }

  #[test]
  fn a_tilde_home_expands_under_the_user_home() {
    let expected = paths::realpath(&PathBuf::from(paths::expanduser("~/dbg")));
    assert_eq!(resolve_home("~/dbg").unwrap(), expected);
  }

  #[test]
  fn an_explicit_home_is_none_when_unset_or_blank() {
    assert_eq!(explicit_home("").unwrap(), None);
    assert_eq!(explicit_home("   ").unwrap(), None);
  }

  #[test]
  fn an_explicit_home_resolves_a_set_value_and_refuses_a_relative_one() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(explicit_home(dir.path().to_str().unwrap()).unwrap(), Some(paths::realpath(dir.path())));
    assert_eq!(explicit_home("rel/home").unwrap_err(), HomeError { raw: "rel/home".to_string() });
  }
}
