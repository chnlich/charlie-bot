use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::process;

use charliebot_cli_common::exit::CliExit;
use charliebot_core::metadata_slot::METADATA_FILE_NAME;
use charliebot_core::proc_identity::PID_START_FIELD;
use charliebot_core::run_token::RUN_TOKEN_ENV;
use charliebot_core::{config, credentials, event_types, home, run_token};
use charliebot_store::chat_events::{self, LineRule};
use charliebot_store::metadata::MetadataReadError;
use charliebot_store::run_records::RunRecord;
use charliebot_store::{layout, metadata, run_identity};
use clap::Args;
use serde_json::{Value, json};

use super::error::{error_line, exit_error};
use crate::store_root;

#[derive(Args)]
pub(super) struct QueryArgs {
  /// Topic to match (repeatable), for full text or with `--index`. It must exist in the vocabulary.
  #[arg(long, action = clap::ArgAction::Append)]
  topic: Vec<String>,
  /// Only entries whose audience contains this value.
  #[arg(long, value_parser = clap::builder::PossibleValuesParser::new(crate::memory::AUDIENCE_ELEMENTS))]
  audience: Option<String>,
  /// Print index lines (`<topic>/<slug> · <title>`) instead of full text: those of the `--topic` topics, or
  /// those of every topic when `--topic` is absent.
  #[arg(long)]
  index: bool,
  /// Only entries in resident topics.
  #[arg(long)]
  resident: bool,
  /// Store root to read.
  #[arg(long)]
  dir: Option<PathBuf>,
}

pub(super) fn query(args: &QueryArgs) -> Result<(), CliExit> {
  if args.topic.is_empty() && !args.index {
    exit_error(
      "error: query needs --topic <topic> for full text, or --index for index lines (--index alone lists every topic)",
    );
  }
  let token = std::env::var(RUN_TOKEN_ENV).unwrap_or_default();
  let fixed_audience = if token.trim().is_empty() {
    None
  } else {
    let home_dir = home::charliebot_home_dir().map_err(|error| CliExit::error(error.to_string()))?;
    Some(run_scoped_audience(&home_dir, token.trim())?)
  };
  if let Some(fixed) = fixed_audience.as_deref()
    && args.audience.as_deref().is_some_and(|audience| audience != fixed)
  {
    exit_error(&format!(
      "error: --audience {} contradicts the run credential's fixed audience '{fixed}'; a run token cannot broaden its own query",
      args.audience.as_deref().expect("the audience mismatch has a requested audience")
    ));
  }
  let audience = fixed_audience.as_deref().or(args.audience.as_deref());
  let memory_dir = match args.dir.as_deref().filter(|path| !path.as_os_str().is_empty()) {
    Some(path) => PathBuf::from(charliebot_core::paths::expanduser(&path.to_string_lossy())),
    None => store_root::memory_dir_in(&home::charliebot_home_dir().map_err(|error| CliExit::error(error.to_string()))?),
  };
  let store = crate::memory::load_store(&memory_dir).map_err(|error| CliExit::error(error.to_string()))?;
  let unknown: Vec<&str> =
    args.topic.iter().map(String::as_str).filter(|topic| !store.topics.contains_key(*topic)).collect();
  if !unknown.is_empty() {
    for value in unknown {
      let topic_hint = value.split_once('/').map(|(topic, _)| topic).filter(|topic| store.topics.contains_key(*topic));
      match topic_hint {
        Some(topic) => {
          error_line(&format!("error: unknown topic: {value} (index lines are topic/slug; try --topic {topic})"))
        }
        None => error_line(&format!("error: unknown topic: {value}")),
      }
    }
    process::exit(1);
  }
  let wanted: BTreeSet<&str> = args.topic.iter().map(String::as_str).collect();
  let residents = crate::memory::resident_topic_names(&store);
  // An empty `wanted` is the `--index` form over every topic: the full-text form always names a topic above.
  let mut matched: Vec<_> = store
    .entries
    .iter()
    .filter(|entry| wanted.is_empty() || entry.topic.as_deref().is_some_and(|topic| wanted.contains(topic)))
    .filter(|entry| audience.is_none_or(|value| crate::memory::audience_allows(entry, value)))
    .filter(|entry| !args.resident || entry.topic.as_deref().is_some_and(|topic| residents.contains(topic)))
    .collect();
  matched.sort_by(|left, right| crate::memory::entry_order_key(left).cmp(&crate::memory::entry_order_key(right)));
  if args.index {
    for entry in matched {
      println!("{}", crate::memory::index_line(entry));
    }
  } else {
    for entry in matched {
      println!("{}", crate::memory::full_text(entry));
    }
  }
  Ok(())
}

fn run_scoped_audience(home_dir: &Path, token: &str) -> Result<String, CliExit> {
  let credentials = credentials::load_credentials(home_dir).map_err(|error| CliExit::error(error.to_string()))?;
  let key = credentials.access_key().unwrap_or_default();
  let claims = match run_token::verify_run_token(token, &key) {
    Ok(claims) => claims,
    Err(error) => exit_error(&format!("error: invalid run token: {error}")),
  };
  let sessions_root = config::sessions_dir_in(home_dir);
  let session_dir = sessions_root.join(&claims.session_id);
  let run_path = layout::runs_dir(&session_dir).join(&claims.run_id).join(METADATA_FILE_NAME);
  let identity = run_identity::read_run_identity_sync(&run_path).map_err(|error| CliExit::error(error.to_string()))?;
  let Some(identity) = identity else {
    exit_error(&format!("error: {}", run_identity::RUN_IDENTITY_UNKNOWN_DETAIL));
  };
  let run: RunRecord = serde_json::from_value(json!({
    "id": identity.id,
    "session_id": claims.session_id,
    "pid": identity.pid,
    (PID_START_FIELD): identity.pid_start,
  }))
  .map_err(|error| CliExit::error(error.to_string()))?;
  let events_path = layout::chat_events_path(&session_dir);
  let events = read_run_events(&events_path)?;
  if let Some(refusal) = run_identity::run_identity_refusal(Some(&run), &events) {
    exit_error(&format!("error: {refusal}"));
  }
  let metadata = load_json_metadata(&sessions_root.join(&claims.session_id).join(run_identity::SESSION_METADATA_NAME))?;
  let profile = metadata.as_ref().and_then(|value| value.get("profile")).and_then(Value::as_str);
  match profile {
    Some(name) if name == metadata::TaskProfile::Manager.as_str() => Ok("master".to_string()),
    Some(name) if name == metadata::TaskProfile::Worker.as_str() => Ok("worker".to_string()),
    None | Some(_) => exit_error(&format!(
      "error: run token references session {}, which is not a task-tree node in this instance",
      claims.session_id
    )),
  }
}

fn read_run_events(path: &Path) -> Result<Vec<Value>, CliExit> {
  let lines = match chat_events::read_lines(path, LineRule::Lines) {
    Ok(lines) => lines,
    Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
    Err(error) => return Err(CliExit::error(format!("read {}: {error}", path.display()))),
  };
  let mut run_finished = Vec::new();
  for line in lines {
    let line = line.map_err(|error| CliExit::error(format!("read {}: {error}", path.display())))?;
    // A line that is not JSON, and a JSON value that is not an object, names no event.
    let Ok(event) = line.decode() else { continue };
    if event.get("type").and_then(Value::as_str) == Some(event_types::RUN_FINISHED) {
      run_finished.push(event);
    }
  }
  Ok(run_finished)
}

fn load_json_metadata(path: &Path) -> Result<Option<Value>, CliExit> {
  if !path.exists() {
    return Ok(None);
  }
  match metadata::read_metadata_value(path) {
    Ok(value) => Ok(Some(value)),
    Err(MetadataReadError::Io(error)) => Err(CliExit::error(format!("read {}: {error}", path.display()))),
    // A file that is not JSON names no profile: the read answers None for one.
    Err(MetadataReadError::Json(_)) => Ok(None),
  }
}
