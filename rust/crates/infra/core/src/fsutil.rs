//! Atomic file publication, the synced line append, the exclusive file lock, and the read-only mapping that the log
//! walkers scan backward.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Read, Write};
use std::num::NonZeroU64;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::OnceLock;

use memmap2::Mmap;
use nix::fcntl::{Flock, FlockArg};

/// `path` written with `text` through a uniquely named sibling `.<name>.<uuid hex>.tmp` swapped in by
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
    remove_sibling(&temporary);
  }
  result
}

/// Remove the sibling that a failed write left behind. A sibling that was never created is not an error.
fn remove_sibling(temporary: &Path) {
  if let Err(error) = std::fs::remove_file(temporary)
    && error.kind() != std::io::ErrorKind::NotFound
  {
    tracing::warn!(path = %temporary.display(), error = %error, "atomic_write_cleanup_failed");
  }
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
  if result.is_err() {
    remove_sibling(&temporary);
  }
  result
}

/// `bytes` written to `path` with the access bits `mode`, through a uniquely named sibling that is created with `mode`,
/// fsynced, and swapped in by `rename`. The directory is fsynced after the swap, so the new file survives a power loss.
/// The sibling is removed when a step before the swap fails. Callers that hold owner-only data pass `0o600`, so the
/// file never sits at a wider mode.
pub fn atomic_write_mode(path: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
  let name = path.file_name().expect("an atomic write target names a file").to_string_lossy();
  let directory = path.parent().filter(|dir| !dir.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
  let temporary = path.with_file_name(format!(".{name}.{}.tmp", uuid::Uuid::new_v4().simple()));
  let result = (|| {
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(&temporary)?;
    file.write_all(bytes)?;
    // The creating umask can clear bits that `mode` sets, so the mode is applied to the open file.
    file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temporary, path)?;
    File::open(directory)?.sync_all()
  })();
  if result.is_err() {
    remove_sibling(&temporary);
  }
  result
}

/// Build `path` with `build`, which writes the new content to the sibling it is given. The sibling is created with the
/// access bits `mode` before `build` runs, so a builder that creates or truncates it keeps `mode`: owner-only data
/// passes `0o600`. The sibling is then swapped in by `rename` as [`atomic_write_text`] does, and removed when a step
/// before the swap fails.
pub fn atomic_build_mode<E: From<std::io::Error>>(
  path: &Path,
  mode: u32,
  build: impl FnOnce(&Path) -> Result<(), E>,
) -> Result<(), E> {
  let name = path.file_name().expect("an atomic write target names a file").to_string_lossy();
  let temporary = path.with_file_name(format!(".{name}.{}.tmp", uuid::Uuid::new_v4().simple()));
  let result: Result<(), E> = (|| {
    let file = std::fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(&temporary)?;
    // The creating umask can clear bits that `mode` sets, so the mode is applied to the open file.
    file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    drop(file);
    build(&temporary)?;
    std::fs::rename(&temporary, path)?;
    Ok(())
  })();
  if result.is_err() {
    remove_sibling(&temporary);
  }
  result
}

/// Append `line` to `path`, creating the file with the access bits `mode` when it is absent, and sync its data before
/// the call returns. `line` is one whole record with its newline included. The parent directory is not created here.
pub fn append_line(path: &Path, line: &[u8], mode: u32) -> std::io::Result<()> {
  let mut file = OpenOptions::new().create(true).append(true).mode(mode).open(path)?;
  file.write_all(line)?;
  file.sync_data()
}

/// An exclusive advisory lock on a file, held until the guard drops.
pub struct ExclusiveLock {
  _file: Flock<File>,
}

/// Open `path`, creating it when absent, and take an exclusive `flock` on it, blocking until the lock is granted. The
/// caller holds the guard to the end of its read-modify-write, so the writers of one file run one at a time. The lock
/// belongs to the open file description, so a second call on the same path waits in this process too.
pub fn lock_exclusive(path: &Path) -> io::Result<ExclusiveLock> {
  let file = OpenOptions::new().create(true).read(true).write(true).truncate(false).open(path)?;
  let file = Flock::lock(file, FlockArg::LockExclusive).map_err(|(_, errno)| io::Error::from(errno))?;
  Ok(ExclusiveLock { _file: file })
}

/// The OS page size in bytes: the granularity at which a mapped-file walk faults a file in. A backward walk that
/// reads page-aligned spans therefore touches those same pages.
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

/// The size up to which [`read_or_map_file`] reads a file instead of mapping it.
///
/// A mapping costs an `mmap`, a fault per page touched and a `munmap`, and the two calls take the process's mapping lock
/// for writing. The boot walk opens thousands of small logs from sixteen threads, so they queue on that lock. One
/// read of a file this small copies less than the mapping costs.
const READ_WHOLE_MAX_BYTES: u64 = 64 * 1024;

/// A file's bytes: read into memory up to [`READ_WHOLE_MAX_BYTES`], mapped read-only above it.
pub enum FileBytes {
  Read(Vec<u8>),
  Mapped(Mmap),
}

impl std::fmt::Debug for FileBytes {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter.debug_struct("FileBytes").field("len", &self.len()).finish_non_exhaustive()
  }
}

impl std::ops::Deref for FileBytes {
  type Target = [u8];

  fn deref(&self) -> &[u8] {
    match self {
      FileBytes::Read(bytes) => bytes,
      FileBytes::Mapped(map) => map,
    }
  }
}

/// The bytes of `path`, or `None` when the file holds no bytes (an empty mapping is invalid). A file of up to
/// [`READ_WHOLE_MAX_BYTES`] is read whole; a larger one is mapped read-only.
///
/// A backward walk over a mapped log faults the pages it crosses, while a `read` per page pays one syscall per page
/// and gets none of the kernel's readahead. On a cold copy of a large log that is the difference between seconds and
/// a minute.
///
/// The mapping is safe against the writers these readers meet because they only append. A rotated log publishes
/// through `rename` onto a new inode, and a truncating writer is not allowed here.
pub fn read_or_map_file(path: &Path) -> io::Result<Option<FileBytes>> {
  let mut file = File::open(path)?;
  let len = file.metadata()?.len();
  if len == 0 {
    return Ok(None);
  }
  if len <= READ_WHOLE_MAX_BYTES {
    let mut bytes = Vec::with_capacity(usize::try_from(len).expect("a small file's length fits in usize"));
    file.read_to_end(&mut bytes)?;
    return Ok(Some(FileBytes::Read(bytes)));
  }
  // SAFETY: the log is append-only while mapped, as the doc comment above requires; no writer truncates it in place.
  let map = unsafe { Mmap::map(&file)? };
  Ok(Some(FileBytes::Mapped(map)))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_held_exclusive_lock_refuses_a_second_lock_until_it_drops() {
    let directory = tempfile::tempdir().expect("the test directory is created");
    let path = directory.path().join("state.lock");
    let held = lock_exclusive(&path).expect("the first lock is granted");
    let second = OpenOptions::new().read(true).write(true).open(&path).expect("the lock file opens again");
    assert!(
      Flock::lock(second, FlockArg::LockExclusiveNonblock).is_err(),
      "a second non-blocking lock is refused while the first is held"
    );
    drop(held);
    lock_exclusive(&path).expect("the lock is granted once the first drops");
  }

  #[test]
  fn a_built_file_keeps_the_mode_its_sibling_was_created_with() {
    let directory = tempfile::tempdir().expect("the test directory is created");
    let path = directory.path().join("owner-only.tar");
    atomic_build_mode(&path, 0o600, |temporary| std::fs::write(temporary, b"owner data"))
      .expect("the build publishes its file");
    let mode = std::fs::metadata(&path).expect("the published file exists").permissions().mode();
    assert_eq!(mode & 0o777, 0o600, "the published file keeps the owner-only mode");
    assert_eq!(std::fs::read(&path).expect("the published file reads back"), b"owner data");
  }
}
