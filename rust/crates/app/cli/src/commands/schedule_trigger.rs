//! `charliebot schedule-trigger` (`src/runtime/cli/schedule_trigger.py`): register a wake that fires when the watched
//! targets finish or `--max-wait` elapses, or start `--cmd` detached and register the wake on its wrapper.
//!
//! Exit codes: 0 registered; 2 usage error, a 422 refusal or a failed readback check; 3 the ssh start failed or the
//! remote PID did not parse (nothing registered); 1 the client contract. Under `--cmd` every failed registration exits
//! 2 after the started wrapper is killed, as the Python CLI does.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use charliebot_cli_common::args::with_infer_long_args;
use charliebot_cli_common::client;
use charliebot_cli_common::exit::CliExit;
use charliebot_cli_common::json_out;
use charliebot_cli_common::resolve;
use charliebot_core::config;
use charliebot_core::pyfmt::{self, JsonStyle};
use charliebot_core::timefmt;
use charliebot_store::{layout, trigger_files};
use charliebot_triggers::triggers;
use clap::{Arg, ArgMatches, Command};
use serde_json::{Map, Value, json};
use time::OffsetDateTime;

use crate::launcher::{self, Launch, LaunchError};

const ENDPOINT: &str = triggers::SCHEDULE_TRIGGER_PATH;

/// `constants.MAX_TRIGGER_MESSAGE_CHARS`: the short-label limit on `--message`.
const MAX_TRIGGER_MESSAGE_CHARS: usize = 200;

/// Exit code for a refused or unverified registration: argparse's usage-error code, as the 422 mapping uses.
const EXIT_VERIFY_REJECTED: i32 = 2;

/// Exit code when the `--cmd` start over ssh failed or the remote PID did not parse.
const EXIT_SSH_FAILED: i32 = 3;

/// `json.dumps(record, separators=(",", ":"))`: the launch record's bytes, with the default ASCII escapes.
const LAUNCH_RECORD_JSON: JsonStyle =
  JsonStyle { ensure_ascii: true, sort_keys: false, item_separator: ",", key_separator: ":", indent: None };

const LOCAL_PID: &str = "local_pid";
const REMOTE_PID: &str = "remote_pid";
const SLURM_JOB: &str = "slurm_job";

/// One `--watch` target, shaped as the Python parser's dict: the key order here is the order on the wire.
#[derive(Clone, Debug, PartialEq, Eq)]
enum WatchSpec {
  LocalPid { pid: i64 },
  RemotePid { host: String, pid: i64 },
  SlurmJob { host: Option<String>, job_id: i64 },
}

impl WatchSpec {
  fn to_json(&self) -> Value {
    match self {
      WatchSpec::LocalPid { pid } => json!({ "kind": LOCAL_PID, "pid": pid }),
      WatchSpec::RemotePid { host, pid } => json!({ "kind": REMOTE_PID, "host": host, "pid": pid }),
      WatchSpec::SlurmJob { host: None, job_id } => json!({ "kind": SLURM_JOB, "job_id": job_id }),
      WatchSpec::SlurmJob { host: Some(host), job_id } => json!({ "kind": SLURM_JOB, "host": host, "job_id": job_id }),
    }
  }

  /// `_target_matches`: a stored target has this target's kind, pid or job id, and host. A requested target with no
  /// host matches only a stored target whose host is null.
  fn matches_stored(&self, stored: &Value) -> bool {
    let (kind, pid, job_id, host) = match self {
      WatchSpec::LocalPid { pid } => (LOCAL_PID, Some(*pid), None, None),
      WatchSpec::RemotePid { host, pid } => (REMOTE_PID, Some(*pid), None, Some(host.as_str())),
      WatchSpec::SlurmJob { host, job_id } => (SLURM_JOB, None, Some(*job_id), host.as_deref()),
    };
    if stored.get("kind").and_then(Value::as_str) != Some(kind) {
      return false;
    }
    if pid.is_some_and(|pid| stored.get("pid").and_then(Value::as_i64) != Some(pid)) {
      return false;
    }
    if job_id.is_some_and(|job_id| stored.get("job_id").and_then(Value::as_i64) != Some(job_id)) {
      return false;
    }
    match host {
      Some(host) => stored.get("host").and_then(Value::as_str) == Some(host),
      None => stored.get("host").is_none_or(Value::is_null),
    }
  }
}

/// What one registration call asks the server for: the sent-but-lost readback and the post-registration check both
/// look for a stored trigger with these fields.
#[derive(Clone)]
struct TriggerKey {
  session_id: String,
  message: String,
  watch_targets: Vec<WatchSpec>,
}

impl TriggerKey {
  /// `_readback_trigger`: the newest pending trigger of the session with this message and these targets, in the
  /// endpoint's response shape. Fired triggers stay on disk and a renewed watch repeats its message, so only a pending
  /// record proves that this call landed.
  fn readback(&self, sessions_dir: &Path) -> Option<Value> {
    let triggers_dir = layout::triggers_dir(&sessions_dir.join(&self.session_id));
    if !triggers_dir.is_dir() {
      return None;
    }
    let files = match trigger_files::iter_trigger_file_stats(&triggers_dir) {
      Ok(files) => files,
      Err(error) => {
        eprintln!("warning: could not list {}: {error}", triggers_dir.display());
        return None;
      }
    };
    let mut candidates = Vec::new();
    for (path, _) in files {
      // A file that cannot be read or parsed is skipped, as the Python readback skips it.
      let Ok(text) = fs::read_to_string(&path) else { continue };
      let Ok(stored) = serde_json::from_str::<Value>(&text) else { continue };
      if stored.get("status").and_then(Value::as_str) != Some("pending") {
        continue;
      }
      if stored.get("message").and_then(Value::as_str) != Some(self.message.as_str()) {
        continue;
      }
      let stored_targets = stored.get("watch_targets").and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
      if stored_targets.len() != self.watch_targets.len() {
        continue;
      }
      let all_matched =
        self.watch_targets.iter().all(|requested| stored_targets.iter().any(|target| requested.matches_stored(target)));
      if all_matched {
        candidates.push(stored);
      }
    }
    // The newest by `created_at`, the stored string compared as Python compares it.
    let newest = candidates
      .iter()
      .filter_map(|stored| Some((stored.get("created_at")?.as_str()?, stored)))
      .max_by_key(|(created_at, _)| *created_at)
      .map(|(_, stored)| stored)?;
    Some(json!({ "trigger_id": newest.get("id")?, "fire_at": newest.get("fire_at")? }))
  }
}

/// The parsed flags. `max_wait` and the watch specs are validated by their parsers.
struct Args {
  session: Option<String>,
  max_wait: i64,
  message: String,
  watch: Option<Vec<WatchSpec>>,
  cmd: Option<String>,
  host: Option<String>,
  cwd: Option<String>,
}

impl Args {
  fn from_matches(matches: &ArgMatches) -> Args {
    Args {
      session: matches.get_one::<String>("session").cloned(),
      max_wait: *matches.get_one::<i64>("max-wait").expect("clap requires --max-wait"),
      message: matches.get_one::<String>("message").cloned().expect("clap requires --message"),
      watch: matches.get_many::<WatchSpec>("watch").map(|specs| specs.cloned().collect()),
      cmd: matches.get_one::<String>("cmd").cloned(),
      host: matches.get_one::<String>("host").cloned(),
      cwd: matches.get_one::<String>("cwd").cloned(),
    }
  }
}

/// The argument parser: argparse's unique long-option prefixes, and the last value of a repeated flag.
fn command() -> Command {
  with_infer_long_args(
    Command::new("schedule-trigger")
      .about("Schedule a delayed trigger for a CharlieBot session")
      .arg(
        Arg::new("session")
          .long("session")
          .value_name("SESSION_ID")
          .help("Session ID (optional; taken from the CHARLIEBOT_SESSION_ID the server writes)")
          .overrides_with("session"),
      )
      .arg(
        Arg::new("max-wait")
          .long("max-wait")
          .value_name("SECONDS")
          .required(true)
          .allow_negative_numbers(true)
          .value_parser(parse_max_wait)
          .overrides_with("max-wait")
          .help("Max wait in seconds before the trigger fires"),
      )
      .arg(
        Arg::new("message")
          .long("message")
          .value_name("MESSAGE")
          .required(true)
          .value_parser(parse_message)
          .overrides_with("message")
          .help(format!("Short label naming which watch fired, at most {MAX_TRIGGER_MESSAGE_CHARS} characters")),
      )
      .arg(
        Arg::new("watch")
          .long("watch")
          .value_name("SPEC")
          .num_args(1..)
          .value_parser(parse_watch_target)
          .overrides_with("watch")
          .help("Targets to watch: PID, HOST:PID, slurm:JOBID or HOST:slurm:JOBID"),
      )
      .arg(
        Arg::new("cmd")
          .long("cmd")
          .value_name("CMD")
          .overrides_with("cmd")
          .help("Start CMD detached and watch the started process. Mutually exclusive with --watch."),
      )
      .arg(
        Arg::new("host")
          .long("host")
          .value_name("HOST")
          .overrides_with("host")
          .help("Run --cmd on this ssh host. Requires --cmd."),
      )
      .arg(
        Arg::new("cwd")
          .long("cwd")
          .value_name("DIR")
          .overrides_with("cwd")
          .help("Working directory for --cmd. Requires --cmd."),
      ),
  )
}

/// Parse the schedule-trigger arguments and run the registration. Parser errors print and exit on their own
/// (argparse-compatible: usage error 2).
pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  let argv = std::iter::once(OsString::from("charliebot schedule-trigger")).chain(args.iter().cloned());
  let matches = match command().try_get_matches_from(argv) {
    Ok(matches) => matches,
    Err(error) => error.exit(),
  };
  let args = Args::from_matches(&matches);
  if args.cmd.is_some() && args.watch.is_some() {
    return Err(CliExit::usage(
      "--cmd and --watch are mutually exclusive: the trigger watches the one process --cmd starts",
    ));
  }
  if args.host.is_some() && args.cmd.is_none() {
    return Err(CliExit::usage("--host requires --cmd: it names the host the started command runs on"));
  }
  if args.cwd.is_some() && args.cmd.is_none() {
    return Err(CliExit::usage("--cwd requires --cmd: it names the started command's working directory"));
  }
  let session_id = resolve::resolve_from_env(args.session.as_deref())?;
  let sessions_dir = config::sessions_dir_in(&home_dir()?);
  match &args.cmd {
    Some(cmd) => start_and_register(&sessions_dir, &session_id, &args, cmd),
    None => register_watch(&sessions_dir, &session_id, &args),
  }
}

/// Register the delay, or the watched targets, without starting anything.
fn register_watch(sessions_dir: &Path, session_id: &str, args: &Args) -> Result<(), CliExit> {
  let watch_targets = args.watch.clone().unwrap_or_default();
  let mut payload = Map::new();
  payload.insert("session_id".to_string(), json!(session_id));
  payload.insert("delay_seconds".to_string(), json!(args.max_wait));
  payload.insert("message".to_string(), json!(args.message));
  if let Some(watch) = &args.watch {
    payload.insert("watch_targets".to_string(), Value::Array(watch.iter().map(WatchSpec::to_json).collect()));
  }
  let key = TriggerKey { session_id: session_id.to_string(), message: args.message.clone(), watch_targets };
  let result = post_registration(Value::Object(payload), &key, sessions_dir)?;
  print!("{}", json_out::render(&result, Some(2)));
  Ok(())
}

/// Start `--cmd` detached through the launcher and register its wake on the wrapper's PID.
fn start_and_register(sessions_dir: &Path, session_id: &str, args: &Args, cmd: &str) -> Result<(), CliExit> {
  let started_at = OffsetDateTime::now_utc();
  let started_text = timefmt::isoformat(started_at).replace("+00:00", "Z");
  let launch_id = launcher::mint_launch_id(started_at);
  let (launch, cwd) = match &args.host {
    None => {
      // `args.cwd or os.getcwd()`: an empty --cwd names the current directory.
      let cwd = match args.cwd.as_deref().filter(|dir| !dir.is_empty()) {
        Some(dir) => dir.to_string(),
        None => std::env::current_dir().map_err(io_failure)?.display().to_string(),
      };
      let launch_dir = launch_dir(sessions_dir, session_id, &launch_id);
      let launch = launcher::launch_local(cmd, Path::new(&cwd), &launch_dir.join("log"), &launch_dir.join("exit"))
        .map_err(launch_failure)?;
      (launch, cwd)
    }
    Some(host) => {
      let launch = launcher::launch_remote(host, cmd, args.cwd.as_deref(), &launch_id).map_err(launch_failure)?;
      // The record's "~" names the ssh login directory: without --cwd no local path describes it.
      (launch, args.cwd.clone().filter(|dir| !dir.is_empty()).unwrap_or_else(|| "~".to_string()))
    }
  };

  let key = TriggerKey {
    session_id: session_id.to_string(),
    message: args.message.clone(),
    watch_targets: vec![launch_watch(&launch)],
  };
  let payload = json!({
    "session_id": session_id,
    "delay_seconds": args.max_wait,
    "message": args.message,
    "watch_targets": [launch_target(&launch)],
  });
  let result = match post_registration(payload, &key, sessions_dir) {
    Ok(result) => result,
    Err(failure) => {
      kill_started(&launch)?;
      return Err(CliExit::contract(failure.body, EXIT_VERIFY_REJECTED));
    }
  };
  if key.readback(sessions_dir).is_none() {
    kill_started(&launch)?;
    return Err(CliExit::usage("registration rejected: no pending trigger watching the started pid on readback"));
  }

  let trigger_id = response_field(&result, "trigger_id")?.clone();
  let fire_at = response_field(&result, "fire_at")?.clone();
  let record = json!({
    "launch_id": launch_id,
    "session_id": session_id,
    "host": launch.host,
    "pid": launch.pid,
    "cwd": cwd,
    "cmd": cmd,
    "started_at": started_text,
    "log": launch.log,
    "exit_file": launch.exit_file,
    "trigger_id": trigger_id,
  });
  let Value::Object(record) = record else { unreachable!("the record literal is an object") };
  let dir = launch_dir(sessions_dir, session_id, &launch_id);
  fs::create_dir_all(&dir).map_err(io_failure)?;
  let record_text = pyfmt::json_dumps(&Value::Object(record.clone()), &LAUNCH_RECORD_JSON);
  fs::write(launch_record_path(&dir), format!("{record_text}\n")).map_err(io_failure)?;

  // The output object: trigger_id and fire_at first, then the record's fields (its trigger_id keeps the first slot).
  let mut output = Map::new();
  output.insert("trigger_id".to_string(), trigger_id);
  output.insert("fire_at".to_string(), fire_at);
  for (key, value) in record {
    output.insert(key, value);
  }
  print!("{}", json_out::render(&Value::Object(output), Some(2)));
  Ok(())
}

/// The registration POST under the client contract: a 422 exits 2, and a sent-but-lost call reads back.
fn post_registration(payload: Value, key: &TriggerKey, sessions_dir: &Path) -> Result<Value, CliExit> {
  let readback_key = key.clone();
  let readback_dir = sessions_dir.to_path_buf();
  client::post(
    ENDPOINT,
    payload,
    Some(Box::new(move || Ok(readback_key.readback(&readback_dir)))),
    &[(422, EXIT_VERIFY_REJECTED)],
  )
}

/// Kill a started wrapper whose registration did not land. A failed kill exits 1 with its own message.
fn kill_started(launch: &Launch) -> Result<(), CliExit> {
  launcher::kill_launch(launch).map_err(launch_failure)
}

/// The `watch_targets` entry the POST carries: the target shape plus the two paths its wake reads.
fn launch_target(launch: &Launch) -> Value {
  match &launch.host {
    None => json!({ "kind": LOCAL_PID, "pid": launch.pid, "exit_file": launch.exit_file, "log": launch.log }),
    Some(host) => {
      json!({ "kind": REMOTE_PID, "host": host, "pid": launch.pid, "exit_file": launch.exit_file, "log": launch.log })
    }
  }
}

/// The target the readback matches the stored trigger against: the kind, pid and host of the launch.
fn launch_watch(launch: &Launch) -> WatchSpec {
  let pid = i64::from(launch.pid);
  match &launch.host {
    None => WatchSpec::LocalPid { pid },
    Some(host) => WatchSpec::RemotePid { host: host.clone(), pid },
  }
}

fn launch_dir(sessions_dir: &Path, session_id: &str, launch_id: &str) -> PathBuf {
  sessions_dir.join(session_id).join("launches").join(launch_id)
}

/// The launch record's file name in a launch directory. The launch record is the CLI's own file kind: only this
/// command writes it, and no server module reads it. Its name is the metadata file name the core owns.
const LAUNCH_RECORD_FILE_NAME: &str = charliebot_core::metadata_slot::METADATA_FILE_NAME;

/// The launch record's path in one launch directory.
fn launch_record_path(launch_dir: &Path) -> PathBuf {
  launch_dir.join(LAUNCH_RECORD_FILE_NAME)
}

fn launch_failure(error: LaunchError) -> CliExit {
  match error {
    LaunchError::Ssh(message) => CliExit::contract(json!({ "error": message }), EXIT_SSH_FAILED),
    LaunchError::Io(message) => CliExit::error(message),
  }
}

fn io_failure(error: io::Error) -> CliExit {
  CliExit::error(error.to_string())
}

fn response_field<'a>(response: &'a Value, key: &str) -> Result<&'a Value, CliExit> {
  response.get(key).ok_or_else(|| CliExit::error(format!("the response lacks {key}")))
}

fn home_dir() -> Result<PathBuf, CliExit> {
  charliebot_core::home::charliebot_home_dir().map_err(|error| CliExit::error(error.to_string()))
}

/// `int(text)` for a flag: surrounding whitespace, an optional sign, and digits with single underscores between them.
fn parse_python_int(text: &str) -> Option<i64> {
  let trimmed = text.trim();
  let (negative, unsigned) = match trimmed.strip_prefix('-') {
    Some(rest) => (true, rest),
    None => (false, trimmed.strip_prefix('+').unwrap_or(trimmed)),
  };
  let well_placed = !unsigned.starts_with('_') && !unsigned.ends_with('_') && !unsigned.contains("__");
  let digits: String = unsigned.chars().filter(|c| *c != '_').collect();
  if !well_placed || digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
    return None;
  }
  let magnitude: i64 = digits.parse().ok()?;
  Some(if negative { -magnitude } else { magnitude })
}

fn parse_max_wait(text: &str) -> Result<i64, String> {
  parse_python_int(text).ok_or_else(|| format!("argument --max-wait: invalid int value: {}", pyfmt::str_repr(text)))
}

/// `--message` is a short label: `_validate_message` counts characters, not bytes.
fn parse_message(value: &str) -> Result<String, String> {
  let count = value.chars().count();
  if count > MAX_TRIGGER_MESSAGE_CHARS {
    return Err(format!(
      "--message must be a short label at most {MAX_TRIGGER_MESSAGE_CHARS} characters (got {count})"
    ));
  }
  Ok(value.to_string())
}

/// `_parse_watch_target`: `PID`, `HOST:PID`, `slurm:JOB` or `HOST:slurm:JOB`. A host named `slurm` is not a target.
fn parse_watch_target(raw: &str) -> Result<WatchSpec, String> {
  let Some((scheme, rest)) = raw.split_once(':') else {
    return Ok(WatchSpec::LocalPid { pid: positive_int(raw, raw, "pid")? });
  };
  if scheme == "slurm" {
    return Ok(WatchSpec::SlurmJob { host: None, job_id: positive_int(rest, raw, "slurm job id")? });
  }
  if scheme.is_empty() {
    return Err(format!("--watch host must be non-empty (got {})", pyfmt::str_repr(raw)));
  }
  if let Some(job) = rest.strip_prefix("slurm:") {
    let job_id = positive_int(job, raw, "slurm job id")?;
    return Ok(WatchSpec::SlurmJob { host: Some(scheme.to_string()), job_id });
  }
  Ok(WatchSpec::RemotePid { host: scheme.to_string(), pid: positive_int(rest, raw, "pid")? })
}

/// `_positive_int`: an int above zero, with the watch spec named in the error.
fn positive_int(value: &str, raw: &str, what: &str) -> Result<i64, String> {
  let number =
    parse_python_int(value).ok_or_else(|| format!("--watch {what} must be int (got {})", pyfmt::str_repr(raw)))?;
  if number <= 0 {
    return Err(format!("--watch {what} must be positive (got {})", pyfmt::str_repr(raw)));
  }
  Ok(number)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn python_int_accepts_the_flag_forms_int_accepts() {
    assert_eq!(parse_python_int("60"), Some(60));
    assert_eq!(parse_python_int(" +60 "), Some(60));
    assert_eq!(parse_python_int("-5"), Some(-5));
    assert_eq!(parse_python_int("1_000"), Some(1000));
    assert_eq!(parse_python_int("1__000"), None);
    assert_eq!(parse_python_int("_1"), None);
    assert_eq!(parse_python_int("1_"), None);
    assert_eq!(parse_python_int(""), None);
    assert_eq!(parse_python_int("-"), None);
    assert_eq!(parse_python_int("+-5"), None);
    assert_eq!(parse_python_int("12x"), None);
  }

  #[test]
  fn watch_specs_take_the_python_dict_shape() {
    assert_eq!(parse_watch_target("12345"), Ok(WatchSpec::LocalPid { pid: 12345 }));
    assert_eq!(
      parse_watch_target("neptune:12345"),
      Ok(WatchSpec::RemotePid { host: "neptune".to_string(), pid: 12345 })
    );
    assert_eq!(parse_watch_target("slurm:98765"), Ok(WatchSpec::SlurmJob { host: None, job_id: 98765 }));
    assert_eq!(
      parse_watch_target("neptune:slurm:98765"),
      Ok(WatchSpec::SlurmJob { host: Some("neptune".to_string()), job_id: 98765 })
    );
    assert!(parse_watch_target("0").is_err());
    assert!(parse_watch_target(":12").is_err());
    assert!(parse_watch_target("neptune:abc").is_err());
  }

  #[test]
  fn message_limit_counts_characters() {
    assert!(parse_message(&"中".repeat(MAX_TRIGGER_MESSAGE_CHARS)).is_ok());
    assert!(parse_message(&"中".repeat(MAX_TRIGGER_MESSAGE_CHARS + 1)).is_err());
  }
}
