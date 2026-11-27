//! The paths that one direction's round may write, and the paths that every direction shares.

use std::path::{Path, PathBuf};

/// The two `/tmp` directories that belong to one direction: the round's scratch and its TMPDIR.
pub fn tmp_directories(direction: &str) -> [PathBuf; 2] {
  [PathBuf::from(format!("/tmp/improve-{direction}")), PathBuf::from(format!("/tmp/improve-{direction}-tmp"))]
}

/// The fixed test scratch directories, each with the test that writes under it. Every direction shares them.
const TEST_SCRATCH: [(&str, &str); 1] = [("/tmp/charliebot-golden", "golden_cases")];

/// The writable paths of one direction. `clone` is the round's clone: the round's protocol makes it and checks its
/// identity before the round, so the boundary never makes it. `owned` holds the directories that accept a write and
/// that the boundary creates when they are missing: the round's own, and the ones that every direction shares.
pub struct DirectionPaths {
  pub clone: PathBuf,
  pub owned: Vec<PathBuf>,
}

/// The writable paths of one direction, with the clone kept apart from the directories the boundary owns.
pub fn direction_paths(home: &Path, direction: &str) -> DirectionPaths {
  let round = home.join("improve").join(direction);
  let mut owned = vec![round.join("evidence")];
  owned.extend(tmp_directories(direction));
  owned.push(home.join(".cache/improve-target").join(direction));
  owned.extend([
    home.join("improve/mock-home"),
    home.join(".cargo/registry"),
    home.join(".cargo/git"),
    home.join(".cargo/.package-cache"),
    home.join(".claude-improve"),
  ]);
  owned.extend(TEST_SCRATCH.iter().map(|(path, _test)| PathBuf::from(path)));
  DirectionPaths { clone: round.join("charlie-bot"), owned }
}
