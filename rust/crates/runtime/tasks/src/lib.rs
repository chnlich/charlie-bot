//! Manager turns and task execution adapters.

pub mod auto_switch;
pub mod autonamer;
mod backend_retry;
pub mod cold_read;
pub mod completion;
pub mod dispatch;
mod entry;
pub mod execution;
pub mod git;
pub mod github_api;
pub mod handoff;
pub mod land;
mod land_push;
pub mod land_queue;
pub mod master_queue;
pub mod master_run;
pub mod master_state;
pub mod master_trigger;
pub mod midturn_delivery;
pub mod prompts;
pub mod recovery;
pub mod review;
pub mod review_text;
pub mod routes;
pub mod run_support;
pub mod session_anchors;
pub mod session_turn;
pub mod skip_proof;
pub mod spawner_backends;
pub mod spawner_prompt;
pub mod task_input_prompt;
pub mod task_sessions;
pub mod text_length;
pub mod tool_line;
pub mod turn_hooks;
pub mod upload;
pub mod verify_trailer;
pub mod web_screenshots;
pub mod worker;

pub use entry::register;

#[cfg(test)]
mod test_fixtures;

/// The fixtures that this crate's integration tests and `charliebot-improve` run against. Off by default; only test
/// dev-dependencies turn the feature on, so a production build carries none of the fake backends or scripts.
#[cfg(feature = "test-fixtures")]
pub mod test_support;
