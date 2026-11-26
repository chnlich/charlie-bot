//! `web/features/discord`: registers the gateway service, internal routes, config section, session fields and turn
//! contribution for the Discord thread entrypoint.

mod api;
pub mod cli;
mod config;
pub mod discord_client;
mod discord_commands;
pub mod discord_listener;
mod entry;
mod event_types;
pub mod metadata;
mod service;
pub mod thread_contract;
mod turn_contribution;

pub use entry::register;

#[cfg(test)]
mod test_support;
