//! Atomic file publication (`src/infra/json_utils.py` `atomic_write_text` and `atomic_write_stream`) and the read-only
//! mapping that the log walkers scan backward.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::num::NonZeroU64;
use std::path::Path;
use std::sync::OnceLock;

use memmap2::Mmap;

/// Write `text` to `path` through a uniquely named sibling `.<name>.<uuid hex>.tmp` swapped in by
/// `rename`. A reader then sees the old content or the new, never a partial write. The sibling is
/// removed when any step before the swap fails.
pub fn atomic_write_text(path: &Path, text: &str) -> std::io::Result<()> {
  write_sibling_then_rename(path, text, false)
}

/// [`atomic_write_text`] with an fsync of the sibling before the swap. A rename over a fresh file does not force its
/// data to disk on ext4 (auto_da_alloc covers only a rename that replaces an existing file). The file can therefore
/// read back empty after a power loss unless the sync comes first.
pub fn atomic_write_text_synced(path: &Path, text: &str) -> std::io::Result<()> {
  write_sibling_then_rename(path, text, true)
}

fn write_sibling_then_rename(path: &Path, text: &str, sync: bool) -> std::io::Result<()> {
  let name = path.file_name().expect("an atomic write target names a file").to_string_lossy();
  let temporary = path.with_file_name(format!(".{name}.{}.tmp", uuid::Uuid::new_v4().simple()));
  let result = (|| {
    let mut file = std::fs::File::create(&temporary)?;
    file.write_all(text.as_bytes())?;
    if sync {
      file.sync_all()?;
      synced_write_step("sync");
    }
    drop(file);
    std::fs::rename(&temporary, path)?;
    if sync {
      synced_write_step("rename");
    }
    Ok(())
  })();
  if result.is_err() {
    // The write already failed; a leftover sibling only matters if it exists.
    if let Err(error) = std::fs::remove_file(&temporary)
      && error.kind() != std::io::ErrorKind::NotFound
    {
      tracing::warn!(path = %temporary.display(), error = %error, "atomic_write_cleanup_failed");
    }
  }
  result
}

#[cfg(feature = "test-support")]
thread_local! {
  /// The steps of this thread's synced writes, in order: `sync` for the sibling's fsync, `rename` for the swap. A test
  /// reads it to check that the fsync comes before the swap.
  static SYNCED_WRITE_STEPS: std::cell::RefCell<Vec<&'static str>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Clear this thread's recorded synced-write steps.
#[cfg(feature = "test-support")]
pub fn clear_synced_write_steps() {
  SYNCED_WRITE_STEPS.with(|steps| steps.borrow_mut().clear());
}

/// This thread's recorded synced-write steps, in order.
#[cfg(feature = "test-support")]
pub fn synced_write_steps() -> Vec<&'static str> {
  SYNCED_WRITE_STEPS.with(|steps| steps.borrow().clone())
}

#[cfg(feature = "test-support")]
fn synced_write_step(step: &'static str) {
  SYNCED_WRITE_STEPS.with(|steps| steps.borrow_mut().push(step));
}

#[cfg(not(feature = "test-support"))]
fn synced_write_step(_step: &'static str) {}

/// Stream `write`'s bytes into `path` through a uniquely named sibling, swapped in by `rename` as
/// [`atomic_write_text`] does. The payload never has to exist as one buffer, and a failing `write` leaves the old
/// content and no sibling behind. The error type is the caller's, so a refusal from `write` passes through unchanged.
pub fn atomic_write_stream<E: From<std::io::Error>>(
  path: &Path,
  write: impl FnOnce(&mut BufWriter<File>) -> Result<(), E>,
) -> Result<(), E> {
  let name = path.file_name().expect("an atomic write target names a file").to_string_lossy();
  let temporary = path.with_file_name(format!(".{name}.{}.tmp", uuid::Uuid::new_v4().simple()));
  let result: Result<(), E> = (|| {
    let mut stream = BufWriter::new(File::create(&temporary)?);
    write(&mut stream)?;
    stream.flush()?;
    drop(stream);
    std::fs::rename(&temporary, path)?;
    Ok(())
  })();
  if result.is_err()
    && let Err(error) = std::fs::remove_file(&temporary)
    && error.kind() != std::io::ErrorKind::NotFound
  {
    tracing::warn!(path = %temporary.display(), error = %error, "atomic_write_cleanup_failed");
  }
  result
}

/// The OS page size in bytes: the granularity at which Python's mmap walks fault a file in. A backward walk that
/// reads page-aligned spans therefore touches the same pages Python does.
pub fn page_bytes() -> u64 {
  static PAGE_BYTES: OnceLock<NonZeroU64> = OnceLock::new();
  PAGE_BYTES
    .get_or_init(|| {
      let page_bytes = nix::unistd::sysconf(nix::unistd::SysconfVar::PAGE_SIZE)
        .expect("sysconf(_SC_PAGESIZE) must succeed")
        .expect("sysconf(_SC_PAGESIZE) must report a page size");
      let page_bytes = u64::try_from(page_bytes).expect("sysconf(_SC_PAGESIZE) must return a positive page size");
      NonZeroU64::new(page_bytes).expect("sysconf(_SC_PAGESIZE) must return a positive page size")
    })
    .get()
}

/// Map `path` read-only, or return `None` when the file holds no bytes (an empty mapping is invalid).
///
/// A backward walk over a mapped log faults the pages it crosses, while a `read` per page pays one syscall per page
/// and gets none of the kernel's readahead. On a cold copy of a large log that is the difference between seconds and
/// a minute (`python: src/infra/ndjson.py:405-470`).
///
/// The mapping is safe against the writers these readers meet because they only append. A rotated log publishes
/// through `rename` onto a new inode, and a truncating writer is no more allowed here than under Python's mapping.
pub fn map_file(path: &Path) -> io::Result<Option<Mmap>> {
  let file = File::open(path)?;
  if file.metadata()?.len() == 0 {
    return Ok(None);
  }
  // SAFETY: the log is append-only while mapped, as the doc comment above requires; no writer truncates it in place.
  let map = unsafe { Mmap::map(&file)? };
  Ok(Some(map))
}
