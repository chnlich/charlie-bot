//! The `/ws/terminal` route and its per-profile tmux and PTY relay.

mod entry;
mod pty;
mod terminal;

pub use entry::register;

#[cfg(test)]
mod tests;
