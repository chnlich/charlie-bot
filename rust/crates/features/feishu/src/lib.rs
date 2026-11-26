//! The Feishu package registers the long-connection service, internal routes, config section, session fields and turn
//! contribution for the Feishu thread entrypoint, and the `charliebot feishu` verbs.

mod adapter;
mod api;
pub mod cli;
mod client;
mod commands;
mod config;
mod entry;
mod event;
mod event_types;
mod fields;
mod frame;
mod listener;
pub mod metadata;
mod owners;
mod package;
mod service;
#[cfg(test)]
mod test_support;
pub mod thread_contract;
mod turn_contribution;
mod user_token;

pub use entry::register;
