//! The repository list of the diff page's datalist (`CharlieBotConfig.discover_repos`, `src/infra/config.py`).

use std::collections::HashSet;
use std::io;
use std::path::Path;

use charliebot_core::config::CharlieBotConfig;
use charliebot_core::paths;

/// One discovered repository: the directory's name and its resolved absolute path.
pub(crate) struct Repo {
  pub(crate) name: String,
  pub(crate) path: String,
}

/// `discover_repos`: scans each `paths.workspace_dirs` entry one level deep for directories holding `.git`, keeps the
/// first entry per resolved path, and sorts by name. The sort is stable, so equal names keep the scan order.
pub(crate) fn discover(cfg: &CharlieBotConfig) -> io::Result<Vec<Repo>> {
  let mut found: Vec<Repo> = Vec::new();
  let mut seen: HashSet<String> = HashSet::new();
  for dir in &cfg.paths.workspace_dirs {
    let parent = Path::new(dir);
    if !parent.is_dir() {
      continue;
    }
    for child in std::fs::read_dir(parent)? {
      let child = child?;
      let child_path = child.path();
      if !child_path.is_dir() || !child_path.join(".git").exists() {
        continue;
      }
      let path = paths::display(&paths::realpath(&child_path));
      if seen.insert(path.clone()) {
        found.push(Repo { name: child.file_name().to_string_lossy().into_owned(), path });
      }
    }
  }
  found.sort_by(|left, right| left.name.cmp(&right.name));
  Ok(found)
}
