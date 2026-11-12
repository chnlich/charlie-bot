//! A hold on building message aggregators, for the node passes of the startup walk.
//!
//! A session's aggregator turns each appended event into the stream deltas that the session channel's subscribers
//! receive. A caller primes it before an append ([`crate::Store::prime_aggregator`]) and announces the event after it.
//! Priming a session without an aggregator reads its whole log and feeds every event to a new aggregator.
//!
//! The walk runs before the server binds its port, so no session channel has a subscriber yet. A future run through
//! [`hold_aggregator_builds`] holds builds. A prime that it makes for a session without an aggregator builds none. The
//! announce that follows skips the session, as it skips any session without a live aggregator. An aggregator that
//! exists already is primed and fed as before. The hold covers the future's own task only, so a task that the future
//! spawns, such as a run follow that outlives the walk, builds as usual. Every held prime's announce therefore ends
//! before the walk does. The first prime after the walk builds the aggregator from the log, which holds the walk's
//! appends.

use std::future::Future;

tokio::task_local! {
  static BUILDS_HELD: ();
}

/// Run `future` with aggregator builds held for its task.
pub async fn hold_aggregator_builds<F: Future>(future: F) -> F::Output {
  BUILDS_HELD.scope((), future).await
}

/// Whether the running task holds aggregator builds.
pub(crate) fn builds_held() -> bool {
  BUILDS_HELD.try_with(|()| ()).is_ok()
}
