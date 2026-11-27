//! Whether a `charliebot` process runs inside an improve round, and the variables that the runner exports to a round.
//!
//! `dev/improve/remote-runner.sh` starts each round with its launch directory in `IMPROVE_LAUNCH_DIR` and its profile
//! home at `$HOME/improve/home`. A round's host runs no CharlieBot server, so the commands that need one refuse at
//! once and name the round's state sources, which the runner exports beside the launch directory in `IMPROVE_DIRECTION`
//! and `IMPROVE_KIT_DIR`. A trial server moves `HOME` and `CHARLIEBOT_HOME`, so it is not a round.

use std::env::VarError;
use std::path::Path;

use charliebot_core::paths;

use crate::exit::{CliExit, charliebot_home_or_exit};

/// The variable that names the round's launch directory. Only the runner sets it.
pub const LAUNCH_DIR_ENV: &str = "IMPROVE_LAUNCH_DIR";
/// The variable that names the round's direction, which is also the name of its prompt file. The runner exports it.
pub const DIRECTION_ENV: &str = "IMPROVE_DIRECTION";
/// The variable that names the kit: the `dev/` tree of the revision that the round runs. The runner exports it.
pub const KIT_DIR_ENV: &str = "IMPROVE_KIT_DIR";
/// The round home beneath the user's home, as the runner names it.
const ROUND_HOME_SUFFIX: &str = "improve/home";

/// Whether this process runs in an improve round: the launch directory is set and `CHARLIEBOT_HOME` is the round home.
pub fn in_improve_round() -> Result<bool, CliExit> {
  if std::env::var_os(LAUNCH_DIR_ENV).is_none_or(|value| value.is_empty()) {
    return Ok(false);
  }
  let charliebot_home = charliebot_home_or_exit()?;
  Ok(is_round_home(&charliebot_home, Path::new(&paths::expanduser("~"))))
}

/// The value of a variable that the runner exports to every round. A missing, empty or non-UTF-8 value is a kit
/// defect, and the error names the variable.
pub fn round_variable(name: &str) -> Result<String, CliExit> {
  match std::env::var(name) {
    Ok(value) if !value.is_empty() => Ok(value),
    Ok(_) | Err(VarError::NotPresent) => Err(CliExit::error(format!(
      "{name} is unset or empty in this improve round: the runner exports it, so the round's kit is defective"
    ))),
    Err(VarError::NotUnicode(_)) => {
      Err(CliExit::error(format!("{name} is not valid UTF-8 in this improve round: the runner's kit is defective")))
    }
  }
}

/// Whether `charliebot_home` is the round home beneath `user_home`. The round home is resolved the same way as
/// `CHARLIEBOT_HOME`, so a symbolic link in the user's home path changes nothing.
pub fn is_round_home(charliebot_home: &Path, user_home: &Path) -> bool {
  charliebot_home == paths::realpath(&user_home.join(ROUND_HOME_SUFFIX))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_round_home_beneath_the_user_home_is_the_round_home() {
    let user = tempfile::tempdir().unwrap();
    let round_home = user.path().join(ROUND_HOME_SUFFIX);
    std::fs::create_dir_all(&round_home).unwrap();
    assert!(is_round_home(&paths::realpath(&round_home), user.path()));
  }

  #[test]
  fn a_trial_home_is_not_the_round_home() {
    let user = tempfile::tempdir().unwrap();
    let trial_home = user.path().join("trial").join("charliebot");
    std::fs::create_dir_all(&trial_home).unwrap();
    assert!(!is_round_home(&paths::realpath(&trial_home), user.path()));
  }
}
