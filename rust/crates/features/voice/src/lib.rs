//! Voice input: the live preview relay, the online upload transcription and the turn contribution.

mod api;
mod audio;
mod config;
mod decode;
mod edit_list;
mod entry;
mod file;
mod matroska;
mod opus;
mod recording;
mod recordings;
mod relay;
mod transcription;
mod turn;
mod wav;

pub use entry::register;
pub use file::transcribe_file;
