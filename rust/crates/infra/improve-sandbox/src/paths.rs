//! The paths that one direction's round may write, and the paths that every direction shares.

use std::path::{Path, PathBuf};

/// The two `/tmp` directories that belong to one direction: the round's scratch and its TMPDIR.
pub fn tmp_directories(direction: &str) -> [PathBuf; 2] {
  [PathBuf::from(format!("/tmp/improve-{direction}")), PathBuf::from(format!("/tmp/improve-{direction}-tmp"))]
}

/// The fixed test scratch directories, each with the test that writes under it. Every direction shares them.
const TEST_SCRATCH: [(&str, &str); 1] = [("/tmp/charliebot-golden", "golden_cases")];

/// The writable paths of one direction, and the writable paths that every direction shares.
pub fn direction_paths(home: &Path, direction: &str) -> (Vec<PathBuf>, Vec<PathBuf>) {
  let round = home.join("improve").join(direction);
  let mut round_paths = vec![round.join("charlie-bot"), round.join("evidence")];
  round_paths.extend(tmp_directories(direction));
  round_paths.push(home.join(".cache/improve-target").join(direction));
  let mut shared_paths = vec![
    home.join("improve/mock-home"),
    home.join(".cargo/registry"),
    home.join(".cargo/git"),
    home.join(".cargo/.package-cache"),
    home.join(".claude-improve"),
  ];
  shared_paths.extend(TEST_SCRATCH.iter().map(|(path, _test)| PathBuf::from(path)));
  (round_paths, shared_paths)
}
