//! The linked worktrees a `~/scripts` candidate holds. Each is removed by its own main repository with
//! `git worktree remove` and no `--force`, so git re-checks the checkout at the moment it goes.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use charliebot_core::git::worktree_remove_args;

use crate::git;

pub(crate) struct LinkedWorktree {
  pub path: PathBuf,
  pub main_repo: PathBuf,
}

/// The linked worktrees among `repositories`. `None` when a `.git` file does not point at `<main>/.git/worktrees/<name>`,
/// which the sweep keeps as `foreign-gitdir`: removing its directory would leave a registration in some other git
/// directory.
pub(crate) fn linked_worktrees(repositories: &[PathBuf]) -> Result<Option<Vec<LinkedWorktree>>> {
  let mut linked = Vec::new();
  for repository in repositories {
    let dot_git = repository.join(".git");
    let metadata = fs::symlink_metadata(&dot_git).with_context(|| format!("inspecting {}", dot_git.display()))?;
    if !metadata.is_file() {
      continue;
    }
    let text = fs::read_to_string(&dot_git).with_context(|| format!("reading {}", dot_git.display()))?;
    let Some(gitdir) = text.trim_end().strip_prefix("gitdir: ") else {
      return Ok(None);
    };
    let gitdir = repository.join(gitdir);
    let Some(main_repo) = gitdir
      .parent()
      .filter(|dir| dir.file_name().is_some_and(|name| name == "worktrees"))
      .and_then(Path::parent)
      .filter(|dir| dir.file_name().is_some_and(|name| name == ".git"))
      .and_then(Path::parent)
    else {
      return Ok(None);
    };
    linked.push(LinkedWorktree { path: repository.clone(), main_repo: main_repo.to_path_buf() });
  }
  Ok(Some(linked))
}

/// Removes each linked worktree through its main repository. Returns git's refusal for the first one it rejects, and
/// removes nothing after it.
pub(crate) fn remove_linked_worktrees(linked: &[LinkedWorktree]) -> Result<Option<String>> {
  for worktree in linked {
    let path = worktree
      .path
      .to_str()
      .with_context(|| format!("linked worktree path {} is not UTF-8", worktree.path.display()))?;
    let output = git::run_git(&worktree.main_repo, &worktree_remove_args(path), None)?;
    if !output.status.success() {
      return Ok(Some(git::decode_error(&output.stderr)));
    }
  }
  Ok(None)
}
