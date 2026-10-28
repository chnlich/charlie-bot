//! The layout of one session directory, `sessions/<id>/`: the paths the store builds inside it.
//!
//! Each function takes the session directory, so a caller holding only the sessions root joins the session id first.
//! This module owns the directory names; `run_records` re-exports the ones it uses. The metadata file name is owned by
//! `charliebot_core::metadata_slot`.

use std::path::{Path, PathBuf};

use charliebot_core::metadata_slot::METADATA_FILE_NAME;

pub const DATA_DIR_NAME: &str = "data";
pub const MASTER_RUNS_DIR_NAME: &str = "master_runs";
pub const THREADS_DIR_NAME: &str = "threads";
pub const RUNS_DIR_NAME: &str = "runs";
pub const TRIGGERS_DIR_NAME: &str = "triggers";
pub const ARCHIVES_DIR_NAME: &str = "archives";
pub const CHAT_EVENTS_FILE_NAME: &str = "chat_events.jsonl";

/// The session's metadata file.
pub fn metadata_path(session_dir: &Path) -> PathBuf {
  session_dir.join(METADATA_FILE_NAME)
}

/// The session's `data` directory: its live chat events, archives, runs and legacy master runs.
pub fn data_dir(session_dir: &Path) -> PathBuf {
  session_dir.join(DATA_DIR_NAME)
}

/// The directory that holds each run of the session.
pub fn runs_dir(session_dir: &Path) -> PathBuf {
  data_dir(session_dir).join(RUNS_DIR_NAME)
}

/// The directory of the session's legacy master runs.
pub fn master_runs_dir(session_dir: &Path) -> PathBuf {
  data_dir(session_dir).join(MASTER_RUNS_DIR_NAME)
}

/// The directory of the session's archived chat-event files.
pub fn archives_dir(session_dir: &Path) -> PathBuf {
  data_dir(session_dir).join(ARCHIVES_DIR_NAME)
}

/// The live chat-event log of the session.
pub fn chat_events_path(session_dir: &Path) -> PathBuf {
  data_dir(session_dir).join(CHAT_EVENTS_FILE_NAME)
}

/// The directory of the session's legacy threads.
pub fn threads_dir(session_dir: &Path) -> PathBuf {
  session_dir.join(THREADS_DIR_NAME)
}

/// The directory of the session's delayed triggers.
pub fn triggers_dir(session_dir: &Path) -> PathBuf {
  session_dir.join(TRIGGERS_DIR_NAME)
}
