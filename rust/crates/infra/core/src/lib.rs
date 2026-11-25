//! The CharlieBot server's foundation: profile home, configuration, credentials, the OAuth token vocabulary,
//! run tokens, backend type names, run kinds, structured logging, the session memo sentence,
//! the process's TLS crypto provider, the text renderings the wire formats need, and the int reading their configs,
//! flags and headers parse.

pub mod backend_types;
pub mod cgroup;
pub mod config;
pub mod credentials;
pub mod event_types;
pub mod fsutil;
pub mod git;
pub mod headers;
pub mod home;
pub mod home_init;
pub mod hostname;
pub mod hot_reload;
pub mod human_size;
pub mod int_text;
pub mod json_loads;
pub mod list_price;
pub mod logging;
pub mod logline;
pub mod memo;
pub mod metadata_slot;
pub mod model_id;
pub mod ndjson;
pub mod oauth;
pub mod os_error;
pub mod pacific;
pub mod parallel_walk;
pub mod paths;
pub mod posix_path;
mod printable_table;
pub mod proc_identity;
pub mod proc_status;
pub mod providers;
pub mod proxy_env;
pub mod regex_classes;
pub mod reply;
pub mod repo_root;
pub mod run_kind;
pub mod run_token;
pub mod session_memo;
pub mod ssh;
pub mod str_methods;
pub mod subagent;
pub mod tempdir;
#[cfg(feature = "test-support")]
pub mod test_support;
pub mod text_repr;
pub mod timefmt;
pub mod timeouts;
pub mod tls;
pub mod ui_text;
pub mod utc_datetime;
pub mod web_search;

pub use metadata_slot::{MetadataFile, MetadataSlot, MetadataSlotValidator, SlotField};
pub use repo_root::RepoRoot;
