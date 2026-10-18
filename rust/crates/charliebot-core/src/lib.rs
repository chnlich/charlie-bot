//! The CharlieBot server's foundation: profile home, configuration, credentials, the home writer
//! fence, run tokens, structured logging, and the CPython text renderings the wire formats need.

pub mod config;
pub mod credentials;
pub mod event_types;
pub mod fence;
pub mod fsutil;
pub mod home;
pub mod hot_reload;
pub mod logging;
pub mod metadata_slot;
pub mod paths;
pub mod proc_identity;
pub mod pyfmt;
pub mod registry;
pub mod run_token;
pub mod ssh;
pub mod timefmt;
pub mod v1_sessions;

pub use metadata_slot::{MetadataFile, MetadataSlot, SlotField};
