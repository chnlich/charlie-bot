//! Atomic file publication (`src/infra/json_utils.py` `atomic_write_text` and `atomic_write_stream`).

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

/// Write `text` to `path` through a uniquely named sibling `.<name>.<uuid hex>.tmp` swapped in by
/// `rename`, so a reader sees the old content or the new, never a partial write. The sibling is
/// removed when any step before the swap fails.
pub fn atomic_write_text(path: &Path, text: &str) -> std::io::Result<()> {
  let name = path.file_name().expect("an atomic write target names a file").to_string_lossy();
  let temporary = path.with_file_name(format!(".{name}.{}.tmp", uuid::Uuid::new_v4().simple()));
  let result = (|| {
    let mut file = std::fs::File::create(&temporary)?;
    file.write_all(text.as_bytes())?;
    drop(file);
    std::fs::rename(&temporary, path)
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
