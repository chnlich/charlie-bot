//! The fake programs' probe steps: the runner the test helpers share when they play a backend CLI or a browser.
//!
//! A fake is a symlink named for its role (for example `claude` or `chrome`) beside a `probe.json` data file that maps
//! each role to its steps. The helper reads its role from `argv[0]`, resolves a bare name on `PATH` as the caller's
//! own `exec` resolved it, and runs the role's steps in order. The steps record argv, the child environment or stdin
//! into a file. They append text to a file, write text into a file inside the directory that an argument names, sleep,
//! clear their dumpable flag, write stdout or stderr, and exit. A step list that ends without `exit` is an error.
//!
//! The two step-based helpers run this one vocabulary: `charliebot-agents`'s `fake_program` binary and
//! `charliebot-artifacts`'s `chrome_stub` binary. A step added for one helper is available to the other, and the
//! panic text that reports a malformed data file stays in one place.

use std::collections::HashMap;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use charliebot_core::paths::{PROBE_FILE_NAME, program_path};
use serde::Deserialize;

/// One step of a fake program. The variant names are the step keys of `probe.json`.
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Step {
  /// The arguments after argv[0], written to the file one per line.
  RecordArgv(PathBuf),
  /// The child environment, written to the file one `NAME=VALUE` per line.
  RecordEnv(PathBuf),
  /// All of stdin, written to the file.
  RecordStdin(PathBuf),
  /// The text, appended to the file, created when absent.
  Append {
    path: PathBuf,
    text: String,
  },
  /// The text, written to `file` inside the directory that the last argument starting with `arg_prefix` names.
  WriteInArgDir {
    arg_prefix: String,
    file: String,
    text: String,
  },
  SleepMs(u64),
  /// Clears the process's dumpable flag. A non-root reader's `/proc/<pid>/exe` read then fails with a permission
  /// error, as it does for a Chrome crashpad handler.
  NonDumpable,
  Stdout(String),
  Stderr(String),
  Exit(i32),
}

/// Run the steps of the role that the fake program at `argv[0]` plays, and exit from its `exit` step.
///
/// The role is the symlink's name, and the data file sits beside the symlink. A step list that ends without an
/// `exit` step panics: the helper would otherwise return a zero status to a caller that reads the run as a success.
pub fn run() -> ! {
  let invoked = env::args_os().next().expect("the helper always receives argv[0]");
  let program = program_path(&invoked);
  let role = program.file_name().expect("argv[0] names the role").to_string_lossy().into_owned();
  let data_path = program.with_file_name(PROBE_FILE_NAME);
  let body =
    fs::read_to_string(&data_path).unwrap_or_else(|error| panic!("read probe steps {}: {error}", data_path.display()));
  let mut roles: HashMap<String, Vec<Step>> =
    serde_json::from_str(&body).unwrap_or_else(|error| panic!("parse probe steps {}: {error}", data_path.display()));
  let steps = roles.remove(&role).unwrap_or_else(|| panic!("probe steps {} name no role {role}", data_path.display()));
  for step in steps {
    run_step(step);
  }
  panic!("probe steps {} for {role} end without an exit step", data_path.display());
}

/// Run one step. The steps with an effect outside this process exit or write before they return.
fn run_step(step: Step) {
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
    Step::NonDumpable => {
      // SAFETY: PR_SET_DUMPABLE with a constant 0 changes only this process's own dumpable flag.
      let ret = unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) };
      assert_eq!(ret, 0, "clear the dumpable flag: {}", io::Error::last_os_error());
    }
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
