//! Test helper that plays the fake programs of the agent tests.
//!
//! Cargo builds this binary once. Each fake program is a symlink named for its role (for example `claude`) that
//! points at this binary, next to a `probe.json` data file. The data file maps each role to its steps, which the
//! helper runs in order: record argv or stdin into a file, append text to a file, sleep, write stdout or stderr, and
//! exit. A step list that ends without `exit` is an error. The helper reads its role from `argv[0]`. A bare name is
//! resolved on `PATH`, as the caller's own `exec` resolved it, so the data file is found beside the symlink.
//!
//! Tests link fakes to this binary and never write an executable and then run it: a fork in another test thread can
//! hold the written file open for writing, and the exec then fails with ETXTBSY.

use std::collections::HashMap;
use std::env;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use serde::Deserialize;

/// One step of a fake program. The variant names are the step keys of `probe.json`.
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Step {
  /// Writes the arguments after argv[0] to the file, one per line.
  RecordArgv(PathBuf),
  /// Writes the child environment to the file, one `NAME=VALUE` per line.
  RecordEnv(PathBuf),
  /// Writes all of stdin to the file.
  RecordStdin(PathBuf),
  /// Appends the text to the file, creating the file when absent.
  Append {
    path: PathBuf,
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
      Step::RecordArgv(path) => {
        let mut text = Vec::new();
        for arg in env::args_os().skip(1) {
          text.extend_from_slice(arg.as_encoded_bytes());
          text.push(b'\n');
        }
        fs::write(&path, text).unwrap_or_else(|error| panic!("record argv to {}: {error}", path.display()));
      }
      Step::RecordEnv(path) => {
        let mut text = Vec::new();
        for (name, value) in env::vars_os() {
          text.extend_from_slice(name.as_encoded_bytes());
          text.push(b'=');
          text.extend_from_slice(value.as_encoded_bytes());
          text.push(b'\n');
        }
        fs::write(&path, text).unwrap_or_else(|error| panic!("record env to {}: {error}", path.display()));
      }
      Step::RecordStdin(path) => {
        let mut text = Vec::new();
        io::stdin().read_to_end(&mut text).expect("read stdin");
        fs::write(&path, text).unwrap_or_else(|error| panic!("record stdin to {}: {error}", path.display()));
      }
      Step::Append { path, text } => {
        let mut file = OpenOptions::new()
          .create(true)
          .append(true)
          .open(&path)
          .unwrap_or_else(|error| panic!("open {} for append: {error}", path.display()));
        file.write_all(text.as_bytes()).unwrap_or_else(|error| panic!("append to {}: {error}", path.display()));
      }
      Step::SleepMs(ms) => thread::sleep(Duration::from_millis(ms)),
      Step::Stdout(text) => {
        let mut out = io::stdout();
        out.write_all(text.as_bytes()).expect("write fake stdout");
        out.flush().expect("flush fake stdout");
      }
      Step::Stderr(text) => {
        let mut err = io::stderr();
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
