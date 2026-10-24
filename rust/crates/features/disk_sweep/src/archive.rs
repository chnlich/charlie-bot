use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use tar::{Builder, EntryType, Header};

use crate::git;

pub(crate) fn archive_worktree(worktree: &Path, archive_path: &Path, untracked: &[PathBuf]) -> Result<()> {
  let parent = archive_path.parent().context("the worktree archive path has no parent directory")?;
  if !parent.is_dir() {
    bail!("run directory is missing for archive: {}", parent.display());
  }
  let patch = git::tracked_diff(worktree)?;
  let mut temp = tempfile::Builder::new()
    .prefix(".worktree-changes-")
    .suffix(".tar")
    .tempfile_in(parent)
    .with_context(|| format!("creating temporary archive in {}", parent.display()))?;
  let mut builder = Builder::new(temp.as_file_mut());
  let mut patch_header = Header::new_gnu();
  patch_header.set_entry_type(EntryType::Regular);
  patch_header.set_size(patch.len() as u64);
  patch_header.set_mode(0o644);
  patch_header.set_mtime(
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).context("system time is before the Unix epoch")?.as_secs(),
  );
  patch_header.set_cksum();
  builder
    .append_data(&mut patch_header, "tracked.patch", patch.as_slice())
    .context("adding tracked.patch to the worktree archive")?;
  for path in untracked {
    let relative = path
      .strip_prefix(worktree)
      .with_context(|| format!("untracked path {} is outside worktree {}", path.display(), worktree.display()))?;
    let mut archive_name = PathBuf::from("untracked");
    archive_name.push(relative);
    builder
      .append_path_with_name(path, &archive_name)
      .with_context(|| format!("adding {} to the worktree archive", archive_name.display()))?;
  }
  builder.finish().context("finishing the worktree archive")?;
  drop(builder);
  fs::rename(temp.path(), archive_path)
    .with_context(|| format!("publishing worktree archive {}", archive_path.display()))?;
  Ok(())
}
