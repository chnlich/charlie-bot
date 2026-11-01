//! The CharlieBot server's foundation: profile home, configuration, credentials, the home writer
//! fence, run tokens, structured logging, and the CPython text renderings the wire formats need.

pub mod approval_digest;
pub mod approval_policy;
pub mod cgroup;
pub mod config;
pub mod credentials;
pub mod event_types;
pub mod fence;
pub mod fsutil;
pub mod git;
pub mod home;
pub mod home_init;
pub mod hot_reload;
pub mod logging;
pub mod logline;
pub mod memo;
pub mod metadata_slot;
pub mod pacific;
pub mod paths;
mod printable_table;
pub mod proc_identity;
pub mod proxy_env;
pub mod regex_classes;
pub mod repo_root;
pub mod run_token;
pub mod ssh;
pub mod str_methods;
pub mod tempdir;
#[cfg(feature = "test-support")]
pub mod test_support;
pub mod text_repr;
pub mod timefmt;
pub mod timeouts;
pub mod v1_sessions;
pub mod web_search;
pub mod yaml;

pub use metadata_slot::{MetadataFile, MetadataSlot, MetadataSlotValidator, SlotField};
pub use repo_root::RepoRoot;
