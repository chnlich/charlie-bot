//! `web/features/explain`: the explain button's three routes under `/api/sessions`, the background generation they
//! start, and the results file they keep in each session.
//!
//! A divider's explanation is one tool-capable one-shot agent run over a read-only copy of the session's chat events.
//! The results file `sessions/<sid>/data/explain_results.json` is the persisted truth. The generation tasks are
//! memory-only, and a pending entry older than the one-shot budget is reaped by the first read that meets it.

mod api;
mod entry;
mod explain;
mod prompt;

#[cfg(test)]
mod test_support;

pub use entry::register;
