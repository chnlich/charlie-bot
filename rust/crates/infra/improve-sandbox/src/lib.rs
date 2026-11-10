//! The writable paths of one improve round's sandbox allow-list.
//!
//! `direction_paths` returns the paths that one direction may write and the paths that every direction shares.
//! `tmp_directories` names the two `/tmp` directories that the round creates for one direction.

use std::path::{Path, PathBuf};

/// The two `/tmp` directories that belong to one direction: the round's scratch and its TMPDIR.
pub fn tmp_directories(direction: &str) -> [PathBuf; 2] {
  [PathBuf::from(format!("/tmp/improve-{direction}")), PathBuf::from(format!("/tmp/improve-{direction}-tmp"))]
}

/// The writable paths of one direction, and the writable paths that every direction shares.
pub fn direction_paths(home: &Path, direction: &str) -> (Vec<PathBuf>, Vec<PathBuf>) {
  let round = home.join("improve").join(direction);
  let mut round_paths = vec![round.join("charlie-bot"), round.join("evidence"), round.join("clc-sessions")];
  round_paths.extend(tmp_directories(direction));
  round_paths.push(home.join(".cache/improve-target").join(direction));
  let shared_paths = vec![
    home.join("improve/mock-home"),
    home.join(".cargo/registry"),
    home.join(".cargo/git"),
    home.join(".cargo/.package-cache"),
    home.join(".claude-improve"),
  ];
  (round_paths, shared_paths)
}
