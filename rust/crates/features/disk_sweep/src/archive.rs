use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use charliebot_core::fsutil;
use tar::{Builder, EntryType, Header};

use crate::git;

pub(crate) fn archive_worktree(worktree: &Path, archive_path: &Path, untracked: &[PathBuf]) -> Result<()> {
  let parent = archive_path.parent().context("the worktree archive path has no parent directory")?;
  if !parent.is_dir() {
    bail!("run directory is missing for archive: {}", parent.display());
  }
  let patch = git::tracked_diff(worktree)?;
  // The archive holds the worktree's uncommitted content, so it stays owner-only.
  fsutil::atomic_build_mode(archive_path, 0o600, |temporary| {
    let mut file =
      fs::File::create(temporary).with_context(|| format!("creating temporary archive in {}", parent.display()))?;
    let mut builder = Builder::new(&mut file);
    let mut patch_header = Header::new_gnu();
    patch_header.set_entry_type(EntryType::Regular);
    patch_header.set_size(patch.len() as u64);
    patch_header.set_mode(0o644);
    patch_header.set_mtime(
      SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .context("system time is before the Unix epoch")?
        .as_secs(),
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
      append_untracked(&mut builder, path, &archive_name)
        .with_context(|| format!("adding {} to the worktree archive", archive_name.display()))?;
    }
    builder.finish().context("finishing the worktree archive")
  })
  .with_context(|| format!("publishing worktree archive {}", archive_path.display()))
}

/// One untracked path into the archive. A link that still resolves archives its target's content. A link whose target
/// is gone is written as a link entry, so the archive keeps the tree's shape. Any other read failure fails the archive.
fn append_untracked(builder: &mut Builder<&mut fs::File>, path: &Path, archive_name: &Path) -> Result<()> {
  match fs::metadata(path) {
    Ok(_) => builder.append_path_with_name(path, archive_name)?,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
      let link = fs::symlink_metadata(path)?;
      if !link.file_type().is_symlink() {
        return Err(error.into());
      }
      let target = fs::read_link(path)?;
      let mut header = Header::new_gnu();
      header.set_metadata(&link);
      builder.append_link(&mut header, archive_name, &target)?;
    }
    Err(error) => return Err(error.into()),
  }
  Ok(())
}
