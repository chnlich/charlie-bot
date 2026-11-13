//! The CharlieBot server's foundation: profile home, configuration, credentials, the home writer
//! fence, run tokens, run kinds, structured logging, and the text renderings the wire formats need.

pub mod cgroup;
pub mod config;
pub mod credentials;
pub mod event_types;
pub mod fence;
pub mod fsutil;
pub mod git;
pub mod headers;
pub mod home;
pub mod home_init;
pub mod hot_reload;
pub mod human_size;
pub mod json_loads;
pub mod logging;
pub mod logline;
pub mod memo;
pub mod metadata_slot;
pub mod ndjson;
pub mod os_error;
pub mod pacific;
pub mod parallel_walk;
pub mod paths;
pub mod posix_path;
mod printable_table;
pub mod proc_identity;
pub mod proxy_env;
pub mod regex_classes;
pub mod repo_root;
pub mod run_kind;
pub mod run_token;
pub mod sequence_diff;
pub mod ssh;
pub mod str_methods;
pub mod subagent;
pub mod tempdir;
#[cfg(feature = "test-support")]
pub mod test_support;
pub mod text_repr;
pub mod timefmt;
pub mod timeouts;
pub mod web_search;

pub use metadata_slot::{MetadataFile, MetadataSlot, MetadataSlotValidator, SlotField};
pub use repo_root::RepoRoot;
