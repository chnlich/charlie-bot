//! Python-compatible temporary directories: 8 characters from `abcdefghijklmnopqrstuvwxyz0123456789_` after the
//! caller's prefix, created with mode `0700` and removed when the guard drops.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const NAME_ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789_";
const NAME_LENGTH: usize = 8;
const ACCEPTED_RANDOM_BYTE_LIMIT: u8 = 222;

/// Create a temporary directory using Python `tempfile.mkdtemp` naming and permissions.
pub fn mkdtemp(prefix: &str) -> io::Result<TempDir> {
  mkdtemp_with(prefix, &tempfile_directory(), random_suffix)
}

/// A temporary directory removed recursively when its guard drops.
pub struct TempDir {
  path: PathBuf,
}

impl TempDir {
  /// Return the path created by `mkdtemp`.
  pub fn path(&self) -> &Path {
    &self.path
  }
}

impl Drop for TempDir {
  fn drop(&mut self) {
    if let Err(error) = fs::remove_dir_all(&self.path) {
      tracing::warn!(path = %self.path.display(), error = %error, "temporary_directory_cleanup_failed");
    }
  }
}

fn tempfile_directory() -> PathBuf {
  if let Some(path) = env::var_os("TMPDIR").filter(|path| !path.is_empty()) {
    let path = PathBuf::from(path);
    if path.is_dir() {
      return path;
    }
  }
  #[cfg(unix)]
  {
    PathBuf::from("/tmp")
  }
  #[cfg(not(unix))]
  {
    env::temp_dir()
  }
}

fn mkdtemp_with(
  prefix: &str,
  parent: &Path,
  mut next_suffix: impl FnMut() -> io::Result<String>,
) -> io::Result<TempDir> {
  loop {
    let mut name = OsString::from(prefix);
    name.push(next_suffix()?);
    let path = parent.join(name);
    match create_private_dir(&path) {
      Ok(()) => return Ok(TempDir { path }),
      Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
      Err(error) => return Err(error),
    }
  }
}

fn random_suffix() -> io::Result<String> {
  let mut suffix = String::with_capacity(NAME_LENGTH);
  while suffix.len() < NAME_LENGTH {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| io::Error::other(error.to_string()))?;
    for byte in bytes {
      if byte < ACCEPTED_RANDOM_BYTE_LIMIT {
        suffix.push(NAME_ALPHABET[usize::from(byte % NAME_ALPHABET.len() as u8)] as char);
        if suffix.len() == NAME_LENGTH {
          break;
        }
      }
    }
  }
  Ok(suffix)
}

#[cfg(unix)]
fn create_private_dir(path: &Path) -> io::Result<()> {
  use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

  let mut builder = fs::DirBuilder::new();
  builder.mode(0o700).create(path)?;
  if let Err(error) = fs::set_permissions(path, fs::Permissions::from_mode(0o700)) {
    return match fs::remove_dir(path) {
      Ok(()) => Err(error),
      Err(cleanup_error) => Err(io::Error::new(
        error.kind(),
        format!("setting permissions on {} failed: {error}; cleanup failed: {cleanup_error}", path.display()),
      )),
    };
  }
  Ok(())
}

#[cfg(not(unix))]
fn create_private_dir(path: &Path) -> io::Result<()> {
  fs::create_dir(path)
}

#[cfg(test)]
mod tests {
  use super::*;
  #[cfg(unix)]
  use std::os::unix::fs::PermissionsExt;
  use std::process::Command;

  #[test]
  fn mkdtemp_matches_python_name_mode_parent_and_collision_behavior() {
    if env::var_os("CHARLIEBOT_TEST_TMPDIR_CHILD").is_none() {
      let root = tempfile::tempdir().unwrap();
      let output = Command::new(env::current_exe().unwrap())
        .args(["--exact", "tempdir::tests::mkdtemp_matches_python_name_mode_parent_and_collision_behavior"])
        .env("TMPDIR", root.path())
        .env("CHARLIEBOT_TEST_TMPDIR_CHILD", "1")
        .output()
        .unwrap();
      assert!(output.status.success(), "tempdir child test failed: {}", String::from_utf8_lossy(&output.stderr));
      return;
    }

    let parent = PathBuf::from(env::var_os("TMPDIR").expect("the isolated test receives TMPDIR"));

    let directories = (0..200).map(|_| mkdtemp("synthetic-").unwrap()).collect::<Vec<_>>();
    assert_eq!(directories.len(), 200);
    for directory in &directories {
      assert_eq!(directory.path().parent(), Some(parent.as_path()));
      let name = directory.path().file_name().unwrap().to_str().unwrap();
      let suffix = name.strip_prefix("synthetic-").unwrap();
      assert_eq!(suffix.len(), NAME_LENGTH);
      assert!(suffix.bytes().all(|byte| NAME_ALPHABET.contains(&byte)));
      #[cfg(unix)]
      assert_eq!(fs::metadata(directory.path()).unwrap().permissions().mode() & 0o777, 0o700);
    }

    let collision_path = parent.join("collision-aaaaaaaa");
    fs::create_dir(&collision_path).unwrap();
    let mut suffixes = ["aaaaaaaa", "bbbbbbbb"].into_iter();
    let collision = mkdtemp_with("collision-", &parent, || Ok(suffixes.next().unwrap().to_string())).unwrap();
    assert_eq!(collision.path(), parent.join("collision-bbbbbbbb"));
    assert!(collision_path.is_dir());

    let removed_path = directories[0].path().to_path_buf();
    drop(collision);
    drop(directories);
    assert!(!removed_path.exists());
  }
}
