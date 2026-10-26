//! `_discover_ncu_python_dir`: the Nsight Compute `extras/python` directory that holds `ncu_report.py`.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

/// `shutil.which`'s search path when `PATH` is unset: `os.confstr("CS_PATH")` on glibc.
const DEFAULT_SEARCH_PATH: &str = "/bin:/usr/bin";

/// The `extras/python` next to the `ncu` that `path_env` finds, else the newest install under `nsight_root`.
/// `None` when neither holds an `ncu_report.py`.
pub fn discover_python_dir(path_env: Option<&OsStr>, nsight_root: &Path) -> anyhow::Result<Option<PathBuf>> {
  if let Some(ncu) = which("ncu", path_env)? {
    for parent in ncu.ancestors().skip(1) {
      let candidate = parent.join("extras").join("python");
      if candidate.join("ncu_report.py").is_file() {
        return Ok(Some(candidate));
      }
    }
  }
  Ok(newest_install(nsight_root))
}

/// `shutil.which(name)`: the first executable regular file called `name` in the search path's directories.
fn which(name: &str, path_env: Option<&OsStr>) -> anyhow::Result<Option<PathBuf>> {
  let search_path = path_env.unwrap_or(OsStr::new(DEFAULT_SEARCH_PATH));
  let cwd = std::env::current_dir()?;
  match which::which_in(name, Some(search_path), cwd) {
    Ok(path) => Ok(Some(path)),
    Err(which::Error::CannotFindBinaryPath | which::Error::CannotGetCurrentDirAndPathListEmpty) => Ok(None),
    Err(error) => Err(error.into()),
  }
}

/// The glob fallback: `glob("<root>/*/extras/python")`, newest name first, takes the first that holds the module.
fn newest_install(nsight_root: &Path) -> Option<PathBuf> {
  // glob reads no root that cannot be listed, and it skips names that start with a dot.
  let entries = fs::read_dir(nsight_root).ok()?;
  let mut candidates: Vec<PathBuf> = entries
    .filter_map(Result::ok)
    .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
    .map(|entry| entry.path().join("extras").join("python"))
    .filter(|candidate| candidate.symlink_metadata().is_ok())
    .collect();
  candidates.sort();
  candidates.into_iter().rev().find(|candidate| candidate.join("ncu_report.py").is_file())
}

#[cfg(test)]
mod tests {
  use std::os::unix::fs::PermissionsExt;

  use super::*;

  fn write_module(python_dir: &Path) {
    fs::create_dir_all(python_dir).unwrap();
    fs::write(python_dir.join("ncu_report.py"), "").unwrap();
  }

  fn write_executable(path: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, "#!/bin/sh\n").unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
  }

  #[test]
  fn the_ncu_on_the_search_path_names_its_install() {
    let root = tempfile::tempdir().unwrap();
    let install = root.path().join("nsight");
    write_executable(&install.join("bin").join("ncu"));
    write_module(&install.join("extras").join("python"));
    let empty_root = tempfile::tempdir().unwrap();

    let found = discover_python_dir(Some(install.join("bin").as_os_str()), empty_root.path()).unwrap();

    assert_eq!(found, Some(install.join("extras").join("python")));
  }

  #[test]
  fn the_glob_fallback_takes_the_newest_install_that_holds_the_module() {
    let root = tempfile::tempdir().unwrap();
    write_module(&root.path().join("2024.1").join("extras").join("python"));
    write_module(&root.path().join("2025.3").join("extras").join("python"));
    fs::create_dir_all(root.path().join("2026.1").join("extras").join("python")).unwrap();
    let empty_path = tempfile::tempdir().unwrap();

    let found = discover_python_dir(Some(empty_path.path().as_os_str()), root.path()).unwrap();

    assert_eq!(found, Some(root.path().join("2025.3").join("extras").join("python")));
  }

  #[test]
  fn no_install_is_none() {
    let root = tempfile::tempdir().unwrap();
    let empty_path = tempfile::tempdir().unwrap();

    assert_eq!(discover_python_dir(Some(empty_path.path().as_os_str()), root.path()).unwrap(), None);
  }
}
