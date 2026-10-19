//! Test helper that plays the fake headless Chrome of the artifacts tests.
//!
//! Cargo builds this binary once. Each fake is a symlink named `chrome` that points at this binary, next to a
//! `probe.json` data file. The data file maps the role to its steps, which the helper runs in order: append text to a
//! file, write text into a file inside the directory that an argument names, write stdout or stderr, sleep, and exit.
//! A step list that ends without `exit` is an error. The helper reads its role from `argv[0]`. A bare name is resolved
//! on `PATH`, as the caller's own `exec` resolved it, so the data file is found beside the symlink.
//!
//! Tests link fakes to this binary and never write an executable and then run it: a fork in another test thread can
//! hold the written file open for writing, and the exec then fails with ETXTBSY.

use std::collections::HashMap;
use std::env;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use serde::Deserialize;

/// One step of a fake. The variant names are the step keys of `probe.json`.
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Step {
  /// Appends the text to the file, creating the file when absent.
  Append {
    path: PathBuf,
    text: String,
  },
  /// Writes the text to `file` inside the directory that the last argument starting with `arg_prefix` names.
  WriteInArgDir {
    arg_prefix: String,
    file: String,
    text: String,
  },
  SleepMs(u64),
  Stdout(String),
  Stderr(String),
  Exit(i32),
}

fn main() {
  let invoked = env::args_os().next().expect("the helper always receives argv[0]");
  let program = program_path(&invoked);
  let role = program.file_name().expect("argv[0] names the role").to_string_lossy().into_owned();
  let data_path = program.with_file_name("probe.json");
  let body =
    fs::read_to_string(&data_path).unwrap_or_else(|error| panic!("read probe steps {}: {error}", data_path.display()));
  let mut roles: HashMap<String, Vec<Step>> =
    serde_json::from_str(&body).unwrap_or_else(|error| panic!("parse probe steps {}: {error}", data_path.display()));
  let steps = roles.remove(&role).unwrap_or_else(|| panic!("probe steps {} name no role {role}", data_path.display()));
  for step in steps {
    match step {
      Step::Append { path, text } => {
        let mut file = OpenOptions::new()
          .create(true)
          .append(true)
          .open(&path)
          .unwrap_or_else(|error| panic!("open {} for append: {error}", path.display()));
        file.write_all(text.as_bytes()).unwrap_or_else(|error| panic!("append to {}: {error}", path.display()));
      }
      Step::WriteInArgDir { arg_prefix, file, text } => {
        let directory = env::args_os()
          .skip(1)
          .filter_map(|arg| arg.to_str()?.strip_prefix(&arg_prefix).map(PathBuf::from))
          .next_back()
          .unwrap_or_else(|| panic!("no argument starts with {arg_prefix}"));
        let path = directory.join(&file);
        fs::write(&path, text).unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
      }
      Step::SleepMs(ms) => thread::sleep(Duration::from_millis(ms)),
      Step::Stdout(text) => {
        let mut out = std::io::stdout();
        out.write_all(text.as_bytes()).expect("write fake stdout");
        out.flush().expect("flush fake stdout");
      }
      Step::Stderr(text) => {
        let mut err = std::io::stderr();
        err.write_all(text.as_bytes()).expect("write fake stderr");
        err.flush().expect("flush fake stderr");
      }
      Step::Exit(code) => std::process::exit(code),
    }
  }
  panic!("probe steps {} for {role} end without an exit step", data_path.display());
}

/// The file the caller ran: `argv[0]` when it has a directory part, else the first `PATH` entry that holds it.
fn program_path(invoked: &OsStr) -> PathBuf {
  let path = Path::new(invoked);
  if path.components().count() > 1 {
    return path.to_path_buf();
  }
  let search = env::var_os("PATH").expect("a bare argv[0] is resolved on PATH");
  env::split_paths(&search)
    .map(|dir| dir.join(path))
    .find(|candidate| candidate.is_file())
    .unwrap_or_else(|| panic!("{} is on no PATH entry", path.display()))
}
