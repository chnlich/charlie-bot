//! The `charlie-code` command-line option names the workspace passes or checks.
//!
//! Two readers name these options: this crate's backend builds the child command, and `preflight-charlie-code`
//! checks the installed CLI's `--help` for them. A renamed option must reach every reader, so the names live here
//! once.

/// The `--json` flag: one JSON event per line on stdout.
pub const JSON: &str = "--json";
/// The model id the endpoint serves.
pub const MODEL: &str = "--model";
/// The OpenAI-compatible endpoint base.
pub const API_BASE: &str = "--api-base";
/// The model's context window, in tokens.
pub const CONTEXT_WINDOW: &str = "--context-window";
/// The file that carries the task text; the text never rides argv.
pub const TASK_FILE: &str = "--task-file";
/// The file that carries the system instructions.
pub const INSTRUCTIONS_FILE: &str = "--instructions-file";
/// The `--no-stream` flag, which turns off the streamed event output.
pub const NO_STREAM: &str = "--no-stream";
/// The run's wall-clock limit, in seconds.
pub const TIMEOUT_SECONDS: &str = "--timeout-seconds";
/// The sampling top-p.
pub const TOP_P: &str = "--top-p";
/// The sampling temperature.
pub const TEMPERATURE: &str = "--temperature";
/// The `--resume` flag: the id of the session to resume.
pub const RESUME: &str = "--resume";
/// The `--image` flag: an image path to attach to the prompt.
pub const IMAGE: &str = "--image";
