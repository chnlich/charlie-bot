//! The `improve-review` command against a scratch home and a recording `charlie-code` stub: the child carries the
//! configured key only in its environment, preflight names its missing item, and the key never reaches a file or a
//! byte the command forwards.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use charliebot_shell_harness::{self as harness, Env, Run, Sandbox};

const KEY: &str = "synthetic-gateway-key-4c1f9a";

static DIRECTIONS: AtomicU64 = AtomicU64::new(0);

/// A fresh `/tmp/improve-<direction>` path, removed when the test ends.
struct Scratch(PathBuf);

impl Scratch {
  fn new() -> (String, Scratch) {
    let direction = format!("review_test_{}_{}", std::process::id(), DIRECTIONS.fetch_add(1, Ordering::Relaxed));
    let path = PathBuf::from(format!("/tmp/improve-{direction}"));
    (direction, Scratch(path))
  }
}

impl Drop for Scratch {
  fn drop(&mut self) {
    let _ = fs::remove_dir_all(&self.0);
  }
}

/// A scratch home: `config.yaml` names one `cc-anthropic-compatible` option with the review fields and the
/// unmapped `effort`/`extra_body` fields, and `credentials.yaml` holds the named section.
fn scratch_home(sandbox: &Sandbox, credential: &str, api_key: &str) -> PathBuf {
  let home = sandbox.dir("home");
  fs::write(
    home.join("config.yaml"),
    "backends:\n  options:\n    - id: review-entry\n      label: Synthetic Review\n      \
     type: cc-anthropic-compatible\n      model: synthetic-model\n      api_base: https://gateway.test\n      \
     context_window: 409600\n      credential: gateway\n      effort: high\n      \
     extra_body:\n        cache:\n          no-cache: true\n",
  )
  .expect("write the scratch config");
  fs::write(home.join("credentials.yaml"), format!("{credential}:\n  api_key: {api_key}\n"))
    .expect("write the scratch credentials");
  home
}

/// A `charlie-code` stub that appends its argv and the SHA-256 digest of `CHARLIE_CODE_API_KEY` to `record`, then
/// runs `body`.
fn recording_stub(sandbox: &Sandbox, record: &Path, body: &str) -> PathBuf {
  let script = format!(
    "record='{}'\n\
     {{\n\
       printf 'argv'\n\
       for arg in \"$@\"; do printf '\\t%s' \"$arg\"; done\n\
       printf '\\n'\n\
       printf 'digest\\t%s\\n' \"$(printf '%s' \"${{CHARLIE_CODE_API_KEY:-}}\" | sha256sum | cut -d' ' -f1)\"\n\
     }} >> \"$record\"\n\
     {body}\n",
    record.display()
  );
  sandbox.stub("charlie-code", &script);
  sandbox.bin().join("charlie-code")
}

/// The environment one run sees: the sandbox's stub directory before the system directories, a scratch HOME, the
/// scratch profile home, and the record path the stub writes.
fn env(sandbox: &Sandbox, home: &Path, record: &Path, with_stub: bool) -> Env {
  let mut env = sandbox.env();
  let path = if with_stub { format!("{}:/usr/bin:/bin", sandbox.bin().display()) } else { "/usr/bin:/bin".to_string() };
  env.insert("PATH".to_string(), path);
  env.insert("HOME".to_string(), sandbox.root().display().to_string());
  env.insert(charliebot_core::home::CHARLIEBOT_HOME_ENV.to_string(), home.display().to_string());
  env.insert("RECORD".to_string(), record.display().to_string());
  env
}

fn run(env: &Env, direction: &str, backend: &str, prompt: &Path) -> Run {
  let mut command = harness::command(env!("CARGO_BIN_EXE_charliebot"), env);
  command.args([
    "improve-review",
    "--direction",
    direction,
    "--backend",
    backend,
    "--prompt",
    &prompt.display().to_string(),
    "--pr",
    "123",
    "--pr-url",
    "https://example.test/pr/123",
  ]);
  harness::run(&mut command)
}

fn sha256(text: &str) -> String {
  let mut child = Command::new("sha256sum").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
  child.stdin.take().unwrap().write_all(text.as_bytes()).unwrap();
  let output = child.wait_with_output().unwrap();
  String::from_utf8(output.stdout).unwrap().split_whitespace().next().unwrap().to_string()
}

/// The single `review-*` directory under the direction's scratch base.
fn review_scratch(direction: &str) -> PathBuf {
  let base = PathBuf::from(format!("/tmp/improve-{direction}"));
  let mut entries: Vec<PathBuf> = fs::read_dir(&base)
    .unwrap_or_else(|error| panic!("read {}: {error}", base.display()))
    .map(|entry| entry.unwrap().path())
    .filter(|path| path.is_dir())
    .collect();
  assert_eq!(entries.len(), 1, "one review scratch directory, got {entries:?}");
  entries.pop().unwrap()
}

/// Every regular file under `root`.
fn files_under(root: &Path) -> Vec<PathBuf> {
  let mut files = Vec::new();
  let Ok(entries) = fs::read_dir(root) else { return files };
  for entry in entries {
    let path = entry.unwrap().path();
    if path.is_dir() {
      files.extend(files_under(&path));
    } else {
      files.push(path);
    }
  }
  files
}

fn assert_no_key_anywhere(scratch: &Path, record: &Path) {
  for path in files_under(scratch).into_iter().chain(std::iter::once(record.to_path_buf())) {
    let bytes = fs::read(&path).unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains(KEY), "the key reached {}", path.display());
  }
}

#[test]
fn the_child_carries_the_entry_fields_and_the_key_digest() {
  let sandbox = Sandbox::new();
  let (direction, _scratch) = Scratch::new();
  let home = scratch_home(&sandbox, "gateway", KEY);
  let prompt = sandbox.dir("prompt");
  fs::write(prompt.join("review.md"), "review this pull request\n").unwrap();
  let record = sandbox.root().join("record.txt");
  recording_stub(
    &sandbox,
    &record,
    "printf '%s\\n' '{\"type\":\"result\",\"completed\":true,\"final_output\":\"the review verdict\"}'",
  );
  let env = env(&sandbox, &home, &record, true);

  let result = run(&env, &direction, "review-entry", &prompt.join("review.md"));

  assert_eq!(result.code, 0, "{}", result.describe());
  let scratch_dir = review_scratch(&direction);
  let verdict_path = scratch_dir.join("verdict.md");
  assert_eq!(result.stdout, format!("{}\n", verdict_path.display()), "{}", result.describe());
  assert_eq!(fs::read_to_string(&verdict_path).unwrap(), "the review verdict");

  let text = fs::read_to_string(&record).unwrap();
  let mut lines = text.lines();
  let argv: Vec<&str> = lines.next().unwrap().strip_prefix("argv").unwrap().split('\t').skip(1).collect();
  let digest = lines.next().unwrap().strip_prefix("digest\t").unwrap();
  assert_eq!(digest, sha256(KEY));
  assert_eq!(
    argv,
    [
      "--json".to_string(),
      "--model".to_string(),
      "openai/synthetic-model".to_string(),
      "--api-base".to_string(),
      "https://gateway.test/v1".to_string(),
      "--context-window".to_string(),
      "409600".to_string(),
      "--session-dir".to_string(),
      sandbox.root().join(format!("improve/{direction}/clc-sessions")).display().to_string(),
      "--task-file".to_string(),
      scratch_dir.join("task.md").display().to_string(),
    ]
  );
  assert!(!argv.iter().any(|arg| arg.contains(KEY)), "{argv:?}");

  assert_eq!(
    fs::read_to_string(scratch_dir.join("task.md")).unwrap(),
    "review this pull request\nPR: 123 https://example.test/pr/123\n"
  );
  assert!(
    sandbox.root().join(format!("improve/{direction}/clc-sessions")).is_dir(),
    "the direction's session directory exists"
  );
  assert_no_key_anywhere(&scratch_dir, &record);
}

#[test]
fn preflight_names_the_missing_entry() {
  let sandbox = Sandbox::new();
  let (direction, _scratch) = Scratch::new();
  let home = scratch_home(&sandbox, "gateway", KEY);
  let prompt = sandbox.dir("prompt");
  fs::write(prompt.join("review.md"), "review\n").unwrap();
  let record = sandbox.root().join("record.txt");
  recording_stub(&sandbox, &record, "exit 0");
  let env = env(&sandbox, &home, &record, true);

  let result = run(&env, &direction, "no-such-entry", &prompt.join("review.md"));

  assert_eq!(result.code, 1, "{}", result.describe());
  assert!(result.stderr.contains("missing backend entry no-such-entry"), "{}", result.stderr);
  assert!(result.stdout.is_empty(), "{}", result.stdout);
  assert!(!record.exists(), "the fake charlie-code never started");
}

#[test]
fn preflight_names_the_missing_credential_section() {
  let sandbox = Sandbox::new();
  let (direction, _scratch) = Scratch::new();
  let home = scratch_home(&sandbox, "another-section", KEY);
  let prompt = sandbox.dir("prompt");
  fs::write(prompt.join("review.md"), "review\n").unwrap();
  let record = sandbox.root().join("record.txt");
  recording_stub(&sandbox, &record, "exit 0");
  let env = env(&sandbox, &home, &record, true);

  let result = run(&env, &direction, "review-entry", &prompt.join("review.md"));

  assert_eq!(result.code, 1, "{}", result.describe());
  assert!(result.stderr.contains("credentials.gateway is not set"), "{}", result.stderr);
  assert!(!record.exists(), "the fake charlie-code never started");
}

#[test]
fn preflight_names_the_empty_key() {
  let sandbox = Sandbox::new();
  let (direction, _scratch) = Scratch::new();
  let home = scratch_home(&sandbox, "gateway", "\"\"");
  let prompt = sandbox.dir("prompt");
  fs::write(prompt.join("review.md"), "review\n").unwrap();
  let record = sandbox.root().join("record.txt");
  recording_stub(&sandbox, &record, "exit 0");
  let env = env(&sandbox, &home, &record, true);

  let result = run(&env, &direction, "review-entry", &prompt.join("review.md"));

  assert_eq!(result.code, 1, "{}", result.describe());
  assert!(result.stderr.contains("credentials.gateway.api_key is empty"), "{}", result.stderr);
  assert!(!record.exists(), "the fake charlie-code never started");
}

#[test]
fn preflight_names_the_missing_prompt_file() {
  let sandbox = Sandbox::new();
  let (direction, _scratch) = Scratch::new();
  let home = scratch_home(&sandbox, "gateway", KEY);
  let record = sandbox.root().join("record.txt");
  recording_stub(&sandbox, &record, "exit 0");
  let env = env(&sandbox, &home, &record, true);
  let missing = sandbox.root().join("prompt/absent.md");

  let result = run(&env, &direction, "review-entry", &missing);

  assert_eq!(result.code, 1, "{}", result.describe());
  assert!(result.stderr.contains(&format!("missing prompt file {}", missing.display())), "{}", result.stderr);
  assert!(!record.exists(), "the fake charlie-code never started");
}

#[test]
fn preflight_names_the_missing_charlie_code() {
  let sandbox = Sandbox::new();
  let (direction, _scratch) = Scratch::new();
  let home = scratch_home(&sandbox, "gateway", KEY);
  let prompt = sandbox.dir("prompt");
  fs::write(prompt.join("review.md"), "review\n").unwrap();
  let record = sandbox.root().join("record.txt");
  let env = env(&sandbox, &home, &record, false);

  let result = run(&env, &direction, "review-entry", &prompt.join("review.md"));

  assert_eq!(result.code, 1, "{}", result.describe());
  assert!(result.stderr.contains("charlie-code not found on PATH"), "{}", result.stderr);
  assert!(!record.exists(), "the fake charlie-code never started");
}

#[test]
fn the_key_is_scrubbed_from_every_success_output_and_file() {
  let sandbox = Sandbox::new();
  let (direction, _scratch) = Scratch::new();
  let home = scratch_home(&sandbox, "gateway", KEY);
  let prompt = sandbox.dir("prompt");
  fs::write(prompt.join("review.md"), "review\n").unwrap();
  let record = sandbox.root().join("record.txt");
  recording_stub(
    &sandbox,
    &record,
    "printf 'stderr leak: %s\\n' \"$CHARLIE_CODE_API_KEY\" >&2\n\
     printf '{\"type\":\"thought\",\"text\":\"event leak: %s\"}\\n' \"$CHARLIE_CODE_API_KEY\"\n\
     printf '{\"type\":\"result\",\"completed\":true,\"final_output\":\"verdict leak: %s\"}\\n' \"$CHARLIE_CODE_API_KEY\"",
  );
  let env = env(&sandbox, &home, &record, true);

  let result = run(&env, &direction, "review-entry", &prompt.join("review.md"));

  assert_eq!(result.code, 0, "{}", result.describe());
  let scratch_dir = review_scratch(&direction);
  assert_eq!(result.stdout, format!("{}\n", scratch_dir.join("verdict.md").display()));
  assert!(!result.stdout.contains(KEY) && !result.stderr.contains(KEY), "{}", result.describe());
  assert_eq!(fs::read_to_string(scratch_dir.join("verdict.md")).unwrap(), "verdict leak: ****");
  assert!(fs::read_to_string(scratch_dir.join("events.ndjson")).unwrap().contains("event leak: ****"));
  assert_no_key_anywhere(&scratch_dir, &record);
}

#[test]
fn a_failing_review_exits_nonzero_with_its_scrubbed_error_text() {
  let sandbox = Sandbox::new();
  let (direction, _scratch) = Scratch::new();
  let home = scratch_home(&sandbox, "gateway", KEY);
  let prompt = sandbox.dir("prompt");
  fs::write(prompt.join("review.md"), "review\n").unwrap();
  let record = sandbox.root().join("record.txt");
  recording_stub(
    &sandbox,
    &record,
    "printf 'gateway refused %s\\n' \"$CHARLIE_CODE_API_KEY\" >&2\n\
     printf '{\"type\":\"error\",\"message\":\"gateway refused %s\"}\\n' \"$CHARLIE_CODE_API_KEY\"\n\
     exit 1",
  );
  let env = env(&sandbox, &home, &record, true);

  let result = run(&env, &direction, "review-entry", &prompt.join("review.md"));

  assert_eq!(result.code, 1, "{}", result.describe());
  assert!(result.stdout.is_empty(), "{}", result.stdout);
  assert!(result.stderr.contains("gateway refused ****"), "{}", result.stderr);
  assert!(!result.stderr.contains(KEY), "{}", result.stderr);
  let scratch_dir = review_scratch(&direction);
  assert!(fs::read_to_string(scratch_dir.join("events.ndjson")).unwrap().contains("gateway refused ****"));
  assert_no_key_anywhere(&scratch_dir, &record);
}
