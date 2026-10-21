//! The qwen3_hf recognizer. The transformers Qwen3-ASR model runs only in Python, so the model stays in the helper
//! `rust/helpers/qwen3_hf_helper.py`. The server starts that helper once, and each window is one request to it.
//!
//! The helper's reply carries the recognizer's raw text. The whitespace normalization of `_decode_samples` runs here,
//! with the sherpa path's, so both engines hand the same text to the segment join.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, bail};
use base64::Engine;
use charliebot_core::python_helper::{LongLived, PythonHelper};
use serde_json::{Value, json};

use crate::transcriber::{WindowDecoder, normalize_text};

/// The helper script, relative to the repo root.
pub(crate) const HELPER_SCRIPT: &str = "rust/helpers/qwen3_hf_helper.py";
/// The helper's name in its log events and in the errors that name it.
const HELPER_LABEL: &str = "qwen3_hf";

/// One helper: a model loaded once, and one request per decoded window.
pub(crate) struct Qwen3HfRecognizer {
  helper: Arc<LongLived>,
}

impl Qwen3HfRecognizer {
  /// Start `script` under `helper`'s interpreter and environment, and wait for its first line. An `init_error` line
  /// fails with the helper's error text. So does a helper that exits before it prints a first line.
  pub(crate) fn start(helper: PythonHelper, script: &Path) -> anyhow::Result<Self> {
    let helper = helper.start_long_lived(script, &[], HELPER_LABEL)?;
    let first = helper.receive()?;
    match first.get("status").and_then(Value::as_str) {
      Some("ready") => {}
      Some("init_error") => bail!("{}", string_field(&first, "error")?),
      _ => bail!("the qwen3_hf helper's first line is neither ready nor init_error: {first}"),
    }
    Ok(Self { helper: Arc::new(helper) })
  }

  /// The helper, for a bundle that stops it on server shutdown.
  pub(crate) fn helper(&self) -> Arc<LongLived> {
    Arc::clone(&self.helper)
  }
}

impl WindowDecoder for Qwen3HfRecognizer {
  /// Send the window as base64 of its float32 little-endian samples. The helper's `error` text is the Python exception
  /// text, so the caller's error reads as a Python decode failure does.
  fn decode_window(&mut self, samples: &[f32]) -> anyhow::Result<String> {
    let bytes: Vec<u8> = samples.iter().flat_map(|sample| sample.to_le_bytes()).collect();
    let request = json!({"samples": base64::engine::general_purpose::STANDARD.encode(bytes)});
    let reply = self.helper.request(&request)?;
    match reply.get("status").and_then(Value::as_str) {
      Some("ok") => Ok(normalize_text(string_field(&reply, "text")?)),
      Some("error") => bail!("{}", string_field(&reply, "error")?),
      _ => bail!("the qwen3_hf helper replied with neither ok nor error: {reply}"),
    }
  }
}

/// A string field of a helper line. A line without it breaks the protocol, so the error carries the whole line.
fn string_field<'a>(line: &'a Value, field: &str) -> anyhow::Result<&'a str> {
  line
    .get(field)
    .and_then(Value::as_str)
    .with_context(|| format!("the qwen3_hf helper line has no string {field}: {line}"))
}

#[cfg(test)]
pub(crate) mod test_support {
  use std::fs;
  use std::path::{Path, PathBuf};
  use std::thread;
  use std::time::{Duration, Instant};

  use charliebot_core::python_helper::PythonHelper;

  /// The helper script as the server ships it. The tests run this text, so only the model package is a stub.
  const HELPER_TEXT: &str = include_str!("../../../../helpers/qwen3_hf_helper.py");

  /// The stand-in for `src.features.voice.transcriber`, with the stub `src` package around it. `STUB_MODE` shapes the
  /// start and the decodes, `STUB_LOG` records each decode's sample count, and `STUB_PID_FILE` receives the pid of a
  /// `sleep` child the stub starts while it loads.
  const STUB_FILES: &[(&str, &str)] = &[
    ("src/__init__.py", ""),
    ("src/infra/__init__.py", ""),
    ("src/infra/config.py", STUB_CONFIG),
    ("src/features/__init__.py", ""),
    ("src/features/voice/__init__.py", ""),
    ("src/features/voice/transcription/__init__.py", ""),
    ("src/features/voice/transcription/base.py", "SAMPLE_RATE = 16000\n"),
    ("src/features/voice/transcriber.py", STUB_TRANSCRIBER),
  ];

  const STUB_CONFIG: &str = r#"from types import SimpleNamespace


def get_config():
  return SimpleNamespace(charliebot_home=None, voice=SimpleNamespace(model_id='synthetic/model'))
"#;

  const STUB_TRANSCRIBER: &str = r#"import os
import subprocess
import sys
from types import SimpleNamespace

MODE = os.environ['STUB_MODE']


def voice_model_paths(cfg):
  return None


def create_qwen3_hf_bundle(cfg, paths):
  print('synthetic weights: 100%', flush=True)
  if MODE == 'init_error':
    raise RuntimeError('synthetic init failure')
  if MODE == 'exit_before_ready':
    sys.exit(3)
  if 'STUB_PID_FILE' in os.environ:
    child = subprocess.Popen(['sleep', '60'])
    with open(os.environ['STUB_PID_FILE'], 'w') as pid_file:
      pid_file.write(str(child.pid))
  return SimpleNamespace(recognizers=(_Recognizer(),))


class _Stream:
  def __init__(self):
    self.samples = None
    self.result = SimpleNamespace(text='')

  def accept_waveform(self, sample_rate, samples):
    self.samples = samples


class _Recognizer:
  def create_stream(self):
    return _Stream()

  def decode_stream(self, stream):
    with open(os.environ['STUB_LOG'], 'a') as log:
      log.write(f'{stream.samples.size}\n')
    if MODE == 'exit_on_decode':
      os._exit(9)
    marker = int(round(float(stream.samples[0]) * 1000))
    stream.result.text = f'  marker\t{marker} '
"#;

  /// A helper tree: the helper script beside the stub `src` package. The helper puts its own directory first on
  /// `sys.path`, so the stub package shadows the checkout's `src` and nothing else changes.
  pub(crate) struct StubTree {
    root: tempfile::TempDir,
  }

  impl StubTree {
    pub(crate) fn new() -> StubTree {
      let root = tempfile::tempdir().expect("create the stub tree");
      fs::write(root.path().join("qwen3_hf_helper.py"), HELPER_TEXT).expect("write the helper script");
      for (relative, text) in STUB_FILES {
        let path = root.path().join(relative);
        fs::create_dir_all(path.parent().expect("a stub file has a directory")).expect("create a stub directory");
        fs::write(path, text).expect("write a stub file");
      }
      StubTree { root }
    }

    pub(crate) fn script(&self) -> PathBuf {
      self.root.path().join("qwen3_hf_helper.py")
    }

    pub(crate) fn path(&self, name: &str) -> PathBuf {
      self.root.path().join(name)
    }

    /// The helper's runner for `mode`, on the checkout's interpreter.
    pub(crate) fn helper(&self, mode: &str) -> PythonHelper {
      PythonHelper::new(&repo_root())
        .env("STUB_MODE", mode)
        .env("STUB_LOG", self.path("decodes.log"))
        .env("STUB_PID_FILE", self.path("child.pid"))
    }

    /// The sample counts the stub decoded, in order.
    pub(crate) fn decoded_sample_counts(&self) -> Vec<usize> {
      fs::read_to_string(self.path("decodes.log"))
        .unwrap_or_default()
        .lines()
        .map(|line| line.parse().expect("the log holds sample counts"))
        .collect()
    }

    /// The pid of the stub's `sleep` child, once the stub has loaded.
    pub(crate) fn child_pid(&self) -> i32 {
      fs::read_to_string(self.path("child.pid")).expect("the stub wrote its child's pid").parse().expect("a pid")
    }
  }

  /// The checkout that holds this crate: its `.venv` is the interpreter the helper tests start.
  pub(crate) fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../..").canonicalize().expect("the checkout root exists")
  }

  /// Wait until `pid` has exited. A kill is asynchronous, so a check right after it can still see the process. A
  /// zombie counts as exited: nothing else reaps a helper's child when the init process does not.
  pub(crate) fn wait_until_gone(pid: i32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !process_is_gone(pid) {
      assert!(Instant::now() < deadline, "process {pid} is still running");
      thread::sleep(Duration::from_millis(10));
    }
  }

  fn process_is_gone(pid: i32) -> bool {
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
      Err(_) => true,
      Ok(stat) => stat.rsplit(')').next().and_then(|rest| rest.trim().chars().next()) == Some('Z'),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::test_support::{StubTree, wait_until_gone};
  use super::*;

  #[test]
  fn ready_helper_decodes_each_window_to_its_normalized_text() {
    let _guard = crate::HELPER_LIFECYCLE_TEST_LOCK.lock().expect("the helper lifecycle test lock is never poisoned");
    // The stub prints while it loads, on the stdout the helper must keep for its replies.
    let tree = StubTree::new();
    let mut recognizer = Qwen3HfRecognizer::start(tree.helper("ready"), &tree.script()).expect("the helper is ready");

    let text = recognizer.decode_window(&[0.25, 0.0, 0.0]).expect("the window decodes");

    assert_eq!(text, "marker 250");
    assert_eq!(tree.decoded_sample_counts(), vec![3]);
  }

  #[test]
  fn init_error_fails_the_start_with_the_helpers_error_text() {
    let _guard = crate::HELPER_LIFECYCLE_TEST_LOCK.lock().expect("the helper lifecycle test lock is never poisoned");
    let tree = StubTree::new();

    let error = Qwen3HfRecognizer::start(tree.helper("init_error"), &tree.script()).err().expect("the start fails");

    assert_eq!(error.to_string(), "synthetic init failure");
  }

  #[test]
  fn exit_before_ready_fails_the_start_with_the_exit_status() {
    let _guard = crate::HELPER_LIFECYCLE_TEST_LOCK.lock().expect("the helper lifecycle test lock is never poisoned");
    let tree = StubTree::new();

    let error = Qwen3HfRecognizer::start(tree.helper("exit_before_ready"), &tree.script()).err().expect("start fails");

    assert_eq!(error.to_string(), "Python helper qwen3_hf stopped answering: exited with code 3");
  }

  #[test]
  fn exit_after_ready_fails_the_request_and_every_later_one_and_kills_the_helpers_children() {
    let _guard = crate::HELPER_LIFECYCLE_TEST_LOCK.lock().expect("the helper lifecycle test lock is never poisoned");
    let tree = StubTree::new();
    let mut recognizer =
      Qwen3HfRecognizer::start(tree.helper("exit_on_decode"), &tree.script()).expect("the helper is ready");

    let first = recognizer.decode_window(&[0.5]).expect_err("the decode fails").to_string();
    let later = recognizer.decode_window(&[0.5]).expect_err("the next decode fails").to_string();

    assert_eq!(first, "Python helper qwen3_hf stopped answering: exited with code 9");
    assert_eq!(later, first);
    wait_until_gone(tree.child_pid());
  }
}
