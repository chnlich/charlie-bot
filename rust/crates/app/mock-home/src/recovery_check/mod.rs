//! The startup lab's recovery check (`scripts/release/startup-lab.sh verify-*`). It starts two builds once each on the
//! same seed, a copy of a task-tree home with the sessions of [`seed`] added, and compares what each server wrote and
//! logged through the digests of [`digest`].

pub mod digest;
pub mod seed;
