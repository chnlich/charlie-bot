//! Python-compatible temporary directories: 8 characters from `abcdefghijklmnopqrstuvwxyz0123456789_` after the
//! caller's prefix, created with mode `0700` and removed when the guard drops.

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

const NAME_ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789_";
const NAME_LENGTH: usize = 8;
const ACCEPTED_RANDOM_BYTE_LIMIT: u8 = 222;
// Python's `tempfile.TMP_MAX`: the name retries for one candidate directory.
const TEST_FILE_ATTEMPTS: usize = 20;
static TEMPFILE_DIRECTORY_CACHE: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Create a temporary directory using Python `tempfile.mkdtemp` naming and permissions.
pub fn mkdtemp(prefix: &str) -> io::Result<TempDir> {
  mkdtemp_with(prefix, &tempfile_directory()?, random_suffix)
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

fn tempfile_directory() -> io::Result<PathBuf> {
  let mut cached = TEMPFILE_DIRECTORY_CACHE.lock().expect("temporary directory cache mutex is not poisoned");
  if let Some(path) = cached.as_ref() {
    return Ok(path.clone());
  }
  let tmpdir = env::var_os("TMPDIR");
  let temp = env::var_os("TEMP");
  let tmp = env::var_os("TMP");
  let cwd = match env::current_dir() {
    Ok(cwd) => cwd,
    Err(error) => {
      // Python keeps "." as the last candidate when the working directory cannot be read.
      tracing::debug!(error = %error, "temporary_directory_cwd_unreadable");
      PathBuf::from(".")
    }
  };
  let path = choose_tempdir(tmpdir.as_deref(), temp.as_deref(), tmp.as_deref(), &cwd, can_create_test_file)?;
  *cached = Some(path.clone());
  Ok(path)
}

fn choose_tempdir(
  tmpdir: Option<&OsStr>,
  temp: Option<&OsStr>,
  tmp: Option<&OsStr>,
  cwd: &Path,
  mut can_create_test_file: impl FnMut(&Path) -> io::Result<bool>,
) -> io::Result<PathBuf> {
  let candidates = candidate_tempdir_list(tmpdir, temp, tmp, cwd);
  for candidate in &candidates {
    let directory =
      if candidate.as_os_str() == OsStr::new(".") { candidate.clone() } else { absolute_path(candidate, cwd) };
    if can_create_test_file(&directory)? {
      return Ok(directory);
    }
  }
  Err(io::Error::new(
    io::ErrorKind::NotFound,
    format!("No usable temporary directory found in {}", python_path_list(&candidates)),
  ))
}

fn candidate_tempdir_list(
  tmpdir: Option<&OsStr>,
  temp: Option<&OsStr>,
  tmp: Option<&OsStr>,
  cwd: &Path,
) -> Vec<PathBuf> {
  let mut candidates = Vec::new();
  for value in [tmpdir, temp, tmp].into_iter().flatten().filter(|value| !value.is_empty()) {
    candidates.push(PathBuf::from(value));
  }
  candidates.extend([PathBuf::from("/tmp"), PathBuf::from("/var/tmp"), PathBuf::from("/usr/tmp"), cwd.to_path_buf()]);
  candidates
}

fn absolute_path(path: &Path, cwd: &Path) -> PathBuf {
  let path = if path.is_absolute() { path.to_path_buf() } else { cwd.join(path) };
  let mut absolute = PathBuf::new();
  for component in path.components() {
    match component {
      Component::CurDir => {}
      Component::ParentDir => {
        absolute.pop();
      }
      component => absolute.push(component.as_os_str()),
    }
  }
  absolute
}

fn python_path_list(paths: &[PathBuf]) -> String {
  let values = paths.iter().map(|path| python_string_literal(&path.to_string_lossy())).collect::<Vec<_>>().join(", ");
  format!("[{values}]")
}

fn python_string_literal(value: &str) -> String {
  let quote = if value.contains('\'') && !value.contains('"') { '"' } else { '\'' };
  let mut literal = String::new();
  literal.push(quote);
  for character in value.chars() {
    match character {
      '\\' => literal.push_str("\\\\"),
      '\n' => literal.push_str("\\n"),
      '\r' => literal.push_str("\\r"),
      '\t' => literal.push_str("\\t"),
      character if character == quote => {
        literal.push('\\');
        literal.push(character);
      }
      character if character.is_control() => {
        let codepoint = u32::from(character);
        if codepoint <= 0xff {
          literal.push_str(&format!("\\x{codepoint:02x}"));
        } else if codepoint <= 0xffff {
          literal.push_str(&format!("\\u{codepoint:04x}"));
        } else {
          literal.push_str(&format!("\\U{codepoint:08x}"));
        }
      }
      character => literal.push(character),
    }
  }
  literal.push(quote);
  literal
}

fn can_create_test_file(directory: &Path) -> io::Result<bool> {
  for _ in 0..TEST_FILE_ATTEMPTS {
    let path = directory.join(random_suffix()?);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    let mut file = match options.open(&path) {
      Ok(file) => file,
      Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
      Err(error) => {
        tracing::debug!(path = %path.display(), error = %error, "temporary_directory_candidate_unusable");
        return Ok(false);
      }
    };
    let write_result = file.write_all(b"blat");
    drop(file);
    let remove_result = fs::remove_file(&path);
    if let Err(error) = write_result {
      tracing::debug!(path = %path.display(), error = %error, "temporary_directory_probe_write_failed");
      if let Err(cleanup_error) = remove_result {
        tracing::debug!(path = %path.display(), error = %cleanup_error, "temporary_directory_probe_cleanup_failed");
      }
      return Ok(false);
    }
    if let Err(error) = remove_result {
      tracing::debug!(path = %path.display(), error = %error, "temporary_directory_probe_cleanup_failed");
      return Ok(false);
    }
    return Ok(true);
  }
  Ok(false)
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

#[cfg(test)]
mod tests {
  use super::*;
  use std::os::unix::fs::PermissionsExt;
  use std::process::Command;

  #[test]
  fn candidate_environment_variables_win_in_python_order() {
    let cwd = Path::new("/synthetic/cwd");
    let tmpdir = choose_tempdir(
      Some(OsStr::new("/synthetic/tmpdir")),
      Some(OsStr::new("/synthetic/temp")),
      Some(OsStr::new("/synthetic/tmp")),
      cwd,
      |path| Ok(path == Path::new("/synthetic/tmpdir")),
    )
    .unwrap();
    assert_eq!(tmpdir, Path::new("/synthetic/tmpdir"));

    let temp =
      choose_tempdir(None, Some(OsStr::new("/synthetic/temp")), Some(OsStr::new("/synthetic/tmp")), cwd, |path| {
        Ok(path == Path::new("/synthetic/temp"))
      })
      .unwrap();
    assert_eq!(temp, Path::new("/synthetic/temp"));

    let tmp = choose_tempdir(None, None, Some(OsStr::new("/synthetic/tmp")), cwd, |path| {
      Ok(path == Path::new("/synthetic/tmp"))
    })
    .unwrap();
    assert_eq!(tmp, Path::new("/synthetic/tmp"));
  }

  #[test]
  fn unset_and_empty_environment_values_are_skipped() {
    let cwd = Path::new("/synthetic/cwd");
    let mut attempted = Vec::new();
    let selected = choose_tempdir(None, Some(OsStr::new("")), None, cwd, |path| {
      attempted.push(path.to_path_buf());
      Ok(path == Path::new("/var/tmp"))
    })
    .unwrap();

    assert_eq!(selected, Path::new("/var/tmp"));
    assert_eq!(attempted, [PathBuf::from("/tmp"), PathBuf::from("/var/tmp")]);
  }

  #[test]
  fn non_writable_environment_candidate_is_skipped() {
    let cwd = Path::new("/synthetic/cwd");
    let mut attempted = Vec::new();
    let selected = choose_tempdir(
      Some(OsStr::new("/synthetic/readonly")),
      Some(OsStr::new("/synthetic/writable")),
      None,
      cwd,
      |path| {
        attempted.push(path.to_path_buf());
        Ok(path == Path::new("/synthetic/writable"))
      },
    )
    .unwrap();

    assert_eq!(selected, Path::new("/synthetic/writable"));
    assert_eq!(attempted, [PathBuf::from("/synthetic/readonly"), PathBuf::from("/synthetic/writable")]);
  }

  #[test]
  fn fallback_candidates_follow_python_order_through_the_current_directory() {
    let cwd = Path::new("/synthetic/cwd");
    let mut attempted = Vec::new();
    let selected = choose_tempdir(None, None, None, cwd, |path| {
      attempted.push(path.to_path_buf());
      Ok(path == cwd)
    })
    .unwrap();

    assert_eq!(selected, cwd);
    assert_eq!(
      attempted,
      [PathBuf::from("/tmp"), PathBuf::from("/var/tmp"), PathBuf::from("/usr/tmp"), cwd.to_path_buf()]
    );
  }

  #[test]
  fn no_usable_candidate_returns_python_tempfile_error_text() {
    let cwd = Path::new("/synthetic/cwd");
    let error = choose_tempdir(None, None, None, cwd, |_| Ok(false)).unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    assert_eq!(
      error.to_string(),
      "No usable temporary directory found in ['/tmp', '/var/tmp', '/usr/tmp', '/synthetic/cwd']"
    );
  }

  #[test]
  fn dot_candidate_is_probed_as_written() {
    let selected = choose_tempdir(None, None, None, Path::new("."), |path| Ok(path == Path::new("."))).unwrap();
    assert_eq!(selected, Path::new("."));
  }

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
