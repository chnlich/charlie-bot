use std::collections::HashSet;
use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use charliebot_core::paths;

type ProcessPaths = Vec<(u32, Vec<PathBuf>)>;

pub(crate) fn in_use_paths(candidates: &[PathBuf], proc_root: &Path) -> Result<(HashSet<PathBuf>, HashSet<u32>)> {
  let (processes, unreadable) = process_paths(proc_root)?;
  let mut used = HashSet::new();
  for candidate in candidates {
    let resolved_candidate = paths::realpath(candidate);
    if processes
      .iter()
      .flat_map(|(_pid, process_paths)| process_paths)
      .any(|process_path| process_path == &resolved_candidate || process_path.starts_with(&resolved_candidate))
    {
      used.insert(candidate.clone());
    }
  }
  Ok((used, unreadable))
}

fn process_paths(proc_root: &Path) -> Result<(ProcessPaths, HashSet<u32>)> {
  let mut proc_dirs = fs::read_dir(proc_root)
    .with_context(|| format!("reading process root {}", proc_root.display()))?
    .collect::<std::io::Result<Vec<_>>>()
    .with_context(|| format!("reading process entry in {}", proc_root.display()))?;
  proc_dirs.sort_by_key(|entry| entry.file_name());
  let own_uid = unsafe { libc::getuid() };
  let mut processes = Vec::new();
  let mut unreadable = HashSet::new();
  for entry in proc_dirs {
    let name = entry.file_name();
    let name = name.to_string_lossy();
    if name.is_empty() || !name.bytes().all(|byte| byte.is_ascii_digit()) {
      continue;
    }
    let pid = match name.parse::<u32>() {
      Ok(pid) => pid,
      Err(_) => continue,
    };
    let process_dir = entry.path();
    let process_metadata = match fs::metadata(&process_dir) {
      Ok(metadata) => metadata,
      Err(error) if process_gone(&error) => continue,
      Err(_) => {
        unreadable.insert(pid);
        continue;
      }
    };
    if process_metadata.uid() != own_uid {
      unreadable.insert(pid);
      continue;
    }
    let mut process_unreadable = false;
    let mut resolved = Vec::new();
    let mut cwd = None;
    for link in ["cwd", "root", "exe"] {
      match read_link(&process_dir.join(link)) {
        Ok(Some(target)) => {
          if link == "cwd" {
            cwd = Some(target.clone());
          }
          resolved.push(target);
        }
        Ok(None) => {}
        Err(_) => process_unreadable = true,
      }
    }
    let fd_dir = process_dir.join("fd");
    let fd_entries = match fs::read_dir(&fd_dir) {
      Ok(entries) => match entries.collect::<std::io::Result<Vec<_>>>() {
        Ok(mut entries) => {
          entries.sort_by_key(|entry| entry.file_name());
          entries
        }
        Err(_) => {
          process_unreadable = true;
          Vec::new()
        }
      },
      Err(error) if process_gone(&error) => Vec::new(),
      Err(_) => {
        process_unreadable = true;
        Vec::new()
      }
    };
    for fd in fd_entries {
      match read_link(&fd.path()) {
        Ok(Some(target)) => resolved.push(target),
        Ok(None) => {}
        Err(_) => process_unreadable = true,
      }
    }
    match maps_paths(&process_dir.join("maps")) {
      Ok(paths) => resolved.extend(paths),
      Err(_) => process_unreadable = true,
    }
    let cmdline = match fs::read(process_dir.join("cmdline")) {
      Ok(cmdline) => cmdline,
      Err(error) if process_gone(&error) => Vec::new(),
      Err(_) => {
        process_unreadable = true;
        Vec::new()
      }
    };
    resolved.extend(cmdline_paths(&cmdline, cwd.as_deref()));
    if process_unreadable {
      unreadable.insert(pid);
    }
    processes.push((pid, resolved));
  }
  Ok((processes, unreadable))
}

fn read_link(path: &Path) -> Result<Option<PathBuf>> {
  let target = match fs::read_link(path) {
    Ok(target) => target,
    Err(error) if process_gone(&error) => return Ok(None),
    Err(error) => return Err(error).with_context(|| format!("reading process link {}", path.display())),
  };
  let target_bytes = target.as_os_str().as_bytes();
  let target_bytes = target_bytes.strip_suffix(b" (deleted)").unwrap_or(target_bytes);
  let target = PathBuf::from(std::ffi::OsString::from_vec(target_bytes.to_vec()));
  if target.is_absolute() { Ok(Some(paths::realpath(&target))) } else { Ok(None) }
}

fn maps_paths(path: &Path) -> Result<Vec<PathBuf>> {
  let contents = match fs::read(path) {
    Ok(contents) => contents,
    Err(error) if process_gone(&error) => return Ok(Vec::new()),
    Err(error) => return Err(error).with_context(|| format!("reading process maps {}", path.display())),
  };
  let mut result = Vec::new();
  for line in String::from_utf8_lossy(&contents).lines() {
    let fields = maps_fields(line.as_bytes());
    let Some(target) = fields else {
      continue;
    };
    let target = target.strip_suffix(b" (deleted)").unwrap_or(target);
    let path = PathBuf::from(std::ffi::OsString::from_vec(target.to_vec()));
    if path.is_absolute() {
      result.push(paths::realpath(&path));
    }
  }
  Ok(result)
}

fn maps_fields(line: &[u8]) -> Option<&[u8]> {
  let mut offset = 0;
  for _ in 0..5 {
    while line.get(offset).is_some_and(u8::is_ascii_whitespace) {
      offset += 1;
    }
    while line.get(offset).is_some_and(|byte| !byte.is_ascii_whitespace()) {
      offset += 1;
    }
  }
  while line.get(offset).is_some_and(u8::is_ascii_whitespace) {
    offset += 1;
  }
  (offset < line.len()).then_some(&line[offset..])
}

fn cmdline_paths(cmdline: &[u8], cwd: Option<&Path>) -> Vec<PathBuf> {
  let mut result = Vec::new();
  for argument in cmdline.split(|byte| *byte == 0).filter(|argument| !argument.is_empty()) {
    let mut values = vec![argument];
    if let Some(index) = argument.iter().position(|byte| *byte == b'=') {
      values.push(&argument[index + 1..]);
    }
    for value in values {
      let value = value.strip_suffix(b" (deleted)").unwrap_or(value);
      let value = if value.starts_with(b"~") {
        match std::str::from_utf8(value) {
          Ok(value) => std::ffi::OsString::from(paths::expanduser(value)).into_vec(),
          Err(_) => value.to_vec(),
        }
      } else {
        value.to_vec()
      };
      let path = PathBuf::from(std::ffi::OsString::from_vec(value));
      if path.is_absolute() {
        result.push(paths::realpath(&path));
      } else if let Some(cwd) = cwd {
        result.push(paths::realpath(&cwd.join(path)));
      }
    }
  }
  result
}

fn process_gone(error: &std::io::Error) -> bool {
  matches!(error.raw_os_error(), Some(libc::ENOENT) | Some(libc::ESRCH))
}
