//! `shutil.which`: the executable a command names.

/// `os.confstr("CS_PATH")` on glibc, the search path `shutil.which` uses when `PATH` is unset.
const CS_PATH: &str = "/bin:/usr/bin";

/// The path of the executable `command` names, or `None`. `search_path` is `PATH`'s value.
pub(crate) fn which(command: &str, search_path: Option<&str>) -> anyhow::Result<Option<String>> {
  let search_path = search_path.unwrap_or(CS_PATH);
  let cwd = std::env::current_dir()?;
  match which::which_in(command, Some(search_path), cwd) {
    Ok(path) => Ok(Some(path.to_string_lossy().into_owned())),
    Err(which::Error::CannotFindBinaryPath | which::Error::CannotGetCurrentDirAndPathListEmpty) => Ok(None),
    Err(error) => Err(error.into()),
  }
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::os::unix::fs::PermissionsExt;

  use super::which;

  fn executable(path: &std::path::Path) {
    fs::write(path, "#!/bin/sh\n").expect("the file writes");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("the mode sets");
  }

  #[test]
  fn a_name_with_a_directory_part_is_checked_as_it_stands() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let binary = dir.path().join("code-server");
    executable(&binary);
    assert_eq!(which(binary.to_str().unwrap(), None).unwrap(), binary.to_str().map(str::to_string));
    assert_eq!(which(dir.path().join("missing").to_str().unwrap(), None).unwrap(), None);
  }

  #[test]
  fn a_bare_name_is_searched_along_the_path_and_a_directory_or_a_non_executable_file_is_skipped() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let first = dir.path().join("first");
    let second = dir.path().join("second");
    fs::create_dir_all(first.join("code-server")).expect("a directory named like the command");
    fs::create_dir(&second).expect("the directory exists");
    fs::write(second.join("code-server"), "not executable\n").expect("the file writes");
    let search = format!("{}:{}", first.display(), second.display());
    assert_eq!(which("code-server", Some(&search)).unwrap(), None);
    executable(&second.join("code-server"));
    assert_eq!(which("code-server", Some(&search)).unwrap(), Some(format!("{}/code-server", second.display())));
  }

  #[test]
  fn an_empty_path_searches_nothing() {
    assert_eq!(which("code-server", Some("")).unwrap(), None);
  }
}
