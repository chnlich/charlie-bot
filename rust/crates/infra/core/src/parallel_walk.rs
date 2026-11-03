//! The parallel map that the startup walks split their per-session reads across.
//!
//! A startup walk reads every session directory of the home (stats, metadata, trigger files, event logs), and each read
//! touches only its own session's files. The walk maps those reads across threads and applies the results in the
//! items' order, so its verdicts and its first error are the ones a sequential walk gives.

/// The most threads one walk splits across. On the 7,160-session mock home sixteen threads left the absence-proof scan
/// at about the walk's fixed per-session cost, so more threads gained nothing.
pub const WORKERS: usize = 16;

/// The batch size below which a walk runs on the calling thread: starting a thread costs more than a handful of small
/// reads, so only a batch a production home produces takes the parallel path.
pub const MIN_ITEMS: usize = 64;

/// `job` applied to each item, in the items' order.
///
/// The items split across the available cores when the batch holds [`MIN_ITEMS`] or more. Each worker takes the next
/// unclaimed item, so one large session's read never holds back a run of others behind it. A worker checks
/// `should_stop` before each item and stops when it answers true; the result is then `None`.
pub fn map_until<T: Sync, R: Send>(
  items: &[T],
  should_stop: &(dyn Fn() -> bool + Sync),
  job: impl Fn(&T) -> R + Sync,
) -> Option<Vec<R>> {
  let workers = std::thread::available_parallelism().map_or(1, |count| count.get()).min(WORKERS);
  if items.len() < MIN_ITEMS || workers < 2 {
    let mut results = Vec::with_capacity(items.len());
    for item in items {
      if should_stop() {
        return None;
      }
      results.push(job(item));
    }
    return Some(results);
  }
  let next = std::sync::atomic::AtomicUsize::new(0);
  let slots: Vec<std::sync::Mutex<Option<R>>> = items.iter().map(|_| std::sync::Mutex::new(None)).collect();
  std::thread::scope(|scope| {
    for _ in 0..workers {
      scope.spawn(|| {
        loop {
          let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
          if index >= items.len() || should_stop() {
            return;
          }
          let result = job(&items[index]);
          *slots[index].lock().expect("walk slot mutex is not poisoned") = Some(result);
        }
      });
    }
  });
  slots.into_iter().map(|slot| slot.into_inner().expect("walk slot mutex is not poisoned")).collect()
}

/// [`map_until`] for a walk that no stop request ends.
pub fn map<T: Sync, R: Send>(items: &[T], job: impl Fn(&T) -> R + Sync) -> Vec<R> {
  map_until(items, &|| false, job).expect("a walk without a stop request maps every item")
}
