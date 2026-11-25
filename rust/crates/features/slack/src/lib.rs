//! `web/features/slack`: the Socket Mode summon listener, thread reply routes, service, session fields, and turn hooks.

mod api;
pub mod cli;
mod colleague;
mod config;
mod download;
mod draft;
mod draft_target;
mod entry;
mod event_types;
pub mod metadata;
mod service;
mod slack_client;
pub mod slack_listener;
pub mod thread_contract;
pub mod token;
mod turn_contribution;

pub use entry::register;

#[cfg(test)]
mod test_support;
