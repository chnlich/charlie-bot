//! Delayed and watched triggers that admit scheduled task inputs.

mod end_record;
mod entry;
mod held_session;
mod trigger_index;
pub mod triggers;

pub use entry::register;
