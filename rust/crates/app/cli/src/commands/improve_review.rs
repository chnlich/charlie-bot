//! The `improve-review` command: one improve round's landing-protocol Step 6 review, end to end.
//!
//! It resolves the backend entry's `model`, `api_base`, `context_window` and `credential` from
//! `$CHARLIEBOT_HOME/config.yaml`, resolves that credential's non-empty key from `credentials.yaml`, and runs
//! `charlie-code` with the key in the child's environment only. Every byte the command writes or forwards —
//! its task file, the event-stream file, the verdict and its stdout/stderr text — passes through the scrub, so
//! the key value never appears in its output or in a file it writes.
//!
//! The scratch directory sits outside the clone the round reviews: the reviewer's own git checkouts stay away
//! from the clone's HEAD, and no non-PR content enters the clone. The session directory is the direction's own
//! folder, because the improve sandbox allows writes only under the round's own directories.

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use charliebot_charlie_code::{API_KEY_ENV, TASK_FILE_NAME, flags};
use charliebot_cli_common::args::with_infer_long_args;
use charliebot_cli_common::exit::CliExit;
use charliebot_core::config::{API_BASE_KEY, BackendOption, CONFIG_FILENAME, CONTEXT_WINDOW_KEY, CREDENTIAL_KEY};
use charliebot_core::{credentials, home, paths};
use clap::{CommandFactory, FromArgMatches, Parser};
use serde_json::Value;
use serde_yaml::Value as YamlValue;

/// The scratch and session directory names carry the direction, so it must be one path component.
fn is_direction(name: &str) -> bool {
  let mut chars = name.chars();
  matches!(chars.next(), Some('a'..='z')) && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

#[derive(Parser)]
#[command(name = "improve-review", about = "Run one improve round's Step 6 review end to end")]
struct ImproveReviewCli {
  /// The improve direction, which names the scratch and session directories
  #[arg(long, required = true)]
  direction: String,
  /// The backends.options id whose fields and credential the review uses
  #[arg(long, required = true)]
  backend: String,
  /// The review prompt file
  #[arg(long, required = true)]
  prompt: String,
  /// The pull request number
  #[arg(long, required = true)]
  pr: u64,
  /// The pull request URL
  #[arg(long, required = true)]
  pr_url: String,
}

/// Parse the arguments, run the review, and print the verdict file's path. A preflight or review failure
/// prints its scrubbed message to stderr and exits 1.
pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  let command = with_infer_long_args(ImproveReviewCli::command());
  let argv = std::iter::once(OsString::from("charliebot improve-review")).chain(args.iter().cloned());
  let matches = match command.try_get_matches_from(argv) {
    Ok(matches) => matches,
    Err(error) => error.exit(),
  };
  let cli = ImproveReviewCli::from_arg_matches(&matches).expect("try_get_matches_from validated the arguments");
  match review(&cli) {
    Ok(verdict) => {
      println!("{}", verdict.display());
      Ok(())
    }
    Err(message) => {
      eprintln!("{message}");
      std::process::exit(1);
    }
  }
}

/// Preflight, then the review. Preflight failures name their item; review failures carry the child's scrubbed
/// text. The key is resolved before the first error that could carry child output, so every later message passes
/// through the scrub.
fn review(cli: &ImproveReviewCli) -> Result<PathBuf, String> {
  if !is_direction(&cli.direction) {
    return Err(format!(
      "invalid direction {:?}: a direction is a lowercase word of letters, digits and underscores",
      cli.direction
    ));
  }
  let home_dir = home::charliebot_home_dir().map_err(|error| error.to_string())?;
  let registry = charliebot_packages::registry().map_err(|error| error.to_string())?;
  let cfg = registry.load_config(&home_dir).map_err(|error| error.to_string())?;
  let config_file = cfg.charliebot_home.join(CONFIG_FILENAME);
  let option = cfg
    .get_backend_option(&cli.backend)
    .ok_or_else(|| format!("missing backend entry {} in {}", cli.backend, config_file.display()))?;
  if option.model.is_empty() {
    return Err(format!("backend entry {} has no model in {}", option.id, config_file.display()));
  }
  let api_base = text_field(option, API_BASE_KEY, &config_file)?;
  let context_window = context_window_field(option, &config_file)?;
  let credential = text_field(option, CREDENTIAL_KEY, &config_file)?;
  let credentials = credentials::load_credentials(&cfg.charliebot_home).map_err(|error| error.to_string())?;
  let key = credentials.require_api_key(&credential).map_err(|error| error.to_string())?;

  let prompt_path = Path::new(&cli.prompt);
  if !prompt_path.is_file() {
    return Err(format!("missing prompt file {}", prompt_path.display()));
  }
  let prompt = fs::read_to_string(prompt_path)
    .map_err(|error| format!("cannot read prompt file {}: {error}", prompt_path.display()))?;
  if !command_on_path("charlie-code") {
    return Err("charlie-code not found on PATH".to_string());
  }

  run_review(cli, option, &api_base, context_window, &credential, &key, &prompt)
    .map_err(|message| scrub(&message, &key))
}

/// One required string field of the backend entry's type fields.
fn text_field(option: &BackendOption, field: &str, config_file: &Path) -> Result<String, String> {
  match option.type_fields.get(field) {
    Some(YamlValue::String(text)) if !text.is_empty() => Ok(text.clone()),
    _ => Err(format!("backend entry {} has no {field} in {}", option.id, config_file.display())),
  }
}

/// The entry's `context_window`, which the review must pass to the child.
fn context_window_field(option: &BackendOption, config_file: &Path) -> Result<i64, String> {
  option.type_fields.get(CONTEXT_WINDOW_KEY).and_then(YamlValue::as_i64).filter(|window| *window > 0).ok_or_else(|| {
    format!("backend entry {} has no positive {} in {}", option.id, CONTEXT_WINDOW_KEY, config_file.display())
  })
}

/// Whether `name` resolves to an executable file through `PATH`.
fn command_on_path(name: &str) -> bool {
  std::env::var_os("PATH").is_some_and(|path| {
    std::env::split_paths(&path).any(|directory| {
      fs::metadata(directory.join(name))
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    })
  })
}

/// Write the task file, run `charlie-code`, and write the event stream and the verdict.
fn run_review(
  cli: &ImproveReviewCli,
  option: &BackendOption,
  api_base: &str,
  context_window: i64,
  credential: &str,
  key: &str,
  prompt: &str,
) -> Result<PathBuf, String> {
  let scratch_base = PathBuf::from(format!("/tmp/improve-{}", cli.direction));
  fs::create_dir_all(&scratch_base).map_err(|error| format!("cannot create {}: {error}", scratch_base.display()))?;
  let scratch = scratch_base.join(format!("review-{}", uuid::Uuid::new_v4().simple()));
  fs::create_dir(&scratch).map_err(|error| format!("cannot create {}: {error}", scratch.display()))?;

  let task_path = scratch.join(TASK_FILE_NAME);
  fs::write(&task_path, scrub(&task_text(prompt, cli.pr, &cli.pr_url), key))
    .map_err(|error| format!("cannot write {}: {error}", task_path.display()))?;

  let session_dir = PathBuf::from(paths::expanduser(&format!("~/improve/{}/clc-sessions", cli.direction)));
  fs::create_dir_all(&session_dir).map_err(|error| format!("cannot create {}: {error}", session_dir.display()))?;

  let model = format!("openai/{}", option.model);
  let endpoint = format!("{}/v1", api_base.trim_end_matches('/'));
  let child = Command::new("charlie-code")
    .arg(flags::JSON)
    .arg(flags::MODEL)
    .arg(&model)
    .arg(flags::API_BASE)
    .arg(&endpoint)
    .arg(flags::CONTEXT_WINDOW)
    .arg(context_window.to_string())
    .arg(flags::SESSION_DIR)
    .arg(&session_dir)
    .arg(flags::TASK_FILE)
    .arg(&task_path)
    .current_dir(&scratch)
    .env(API_KEY_ENV, key)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .map_err(|error| format!("cannot start charlie-code: {error}"))?;
  let output = child.wait_with_output().map_err(|error| format!("cannot read charlie-code: {error}"))?;

  let stream = String::from_utf8_lossy(&output.stdout);
  let events_path = scratch.join("events.ndjson");
  fs::write(&events_path, scrub(&stream, key))
    .map_err(|error| format!("cannot write {}: {error}", events_path.display()))?;

  if !output.status.success() {
    let stderr_text = String::from_utf8_lossy(&output.stderr);
    let detail = if stderr_text.trim().is_empty() {
      error_event(&stream).unwrap_or_else(|| format!("charlie-code exited with {}", output.status))
    } else {
      stderr_text.trim_end().to_string()
    };
    return Err(format!("charlie-code review of {} failed: {detail}", credential));
  }

  let verdict_path = scratch.join("verdict.md");
  fs::write(&verdict_path, scrub(&final_verdict(&stream)?, key))
    .map_err(|error| format!("cannot write {}: {error}", verdict_path.display()))?;
  Ok(verdict_path)
}

/// The prompt file verbatim, then one final line naming the pull request.
fn task_text(prompt: &str, pr: u64, pr_url: &str) -> String {
  let mut text = prompt.to_string();
  if !text.is_empty() && !text.ends_with('\n') {
    text.push('\n');
  }
  text.push_str(&format!("PR: {pr} {pr_url}\n"));
  text
}

/// The `final_output` of the stream's last `result` event. A stream with no result event is a review failure.
fn final_verdict(stream: &str) -> Result<String, String> {
  let mut verdict = None;
  for line in stream.lines() {
    let Ok(event) = serde_json::from_str::<Value>(line) else { continue };
    if event.get("type").and_then(Value::as_str) != Some("result") {
      continue;
    }
    let final_output = event
      .get("final_output")
      .and_then(Value::as_str)
      .ok_or_else(|| "charlie-code's result event holds no final_output".to_string())?;
    verdict = Some(final_output.to_string());
  }
  verdict.ok_or_else(|| "charlie-code wrote no result event".to_string())
}

/// The message of the stream's last `error` event, for a child that failed without stderr text.
fn error_event(stream: &str) -> Option<String> {
  stream.lines().rev().find_map(|line| {
    let event = serde_json::from_str::<Value>(line).ok()?;
    if event.get("type").and_then(Value::as_str) == Some("error") {
      event.get("message").and_then(Value::as_str).map(str::to_string)
    } else {
      None
    }
  })
}

/// Replace every occurrence of the key value: the command's only output form.
fn scrub(text: &str, key: &str) -> String {
  text.replace(key, "****")
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_task_file_is_the_prompt_then_one_pull_request_line() {
    assert_eq!(task_text("review", 7, "https://example.test/7"), "review\nPR: 7 https://example.test/7\n");
    assert_eq!(task_text("review\n", 7, "u"), "review\nPR: 7 u\n");
  }

  #[test]
  fn the_verdict_is_the_last_result_events_final_output() {
    let stream = "{\"type\":\"thought\",\"text\":\"x\"}\n\
                  {\"type\":\"result\",\"final_output\":\"first\"}\n\
                  {\"type\":\"result\",\"final_output\":\"last\"}\n";
    assert_eq!(final_verdict(stream).unwrap(), "last");
    assert_eq!(final_verdict("{\"type\":\"thought\"}\n").unwrap_err(), "charlie-code wrote no result event");
    assert_eq!(
      final_verdict("{\"type\":\"result\"}\n").unwrap_err(),
      "charlie-code's result event holds no final_output"
    );
  }

  #[test]
  fn a_direction_is_one_lowercase_path_component() {
    assert!(is_direction("review_check"));
    assert!(is_direction("perf2"));
    assert!(!is_direction(""));
    assert!(!is_direction("Perf"));
    assert!(!is_direction("../perf"));
  }

  #[test]
  fn the_scrub_replaces_every_occurrence() {
    assert_eq!(scrub("a k b k", "k"), "a **** b ****");
  }
}
