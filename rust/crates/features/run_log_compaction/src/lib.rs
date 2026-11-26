//! The `run_log_compaction` cron handler: a finished run's log files become `.zst` once its end is a day old.

mod compaction;
mod entry;

pub use entry::register;
