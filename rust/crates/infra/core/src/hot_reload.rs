//! The file-backed cache that reloads when its file's fingerprint moves (`credentials._HotReloadCache`).
//!
//! `get` re-runs the loader only when the file's `(mtime, size)` differs from both the cached value's
//! and the last failure's. A failed reload keeps the previous value and logs one warning per error
//! text; a failure with nothing cached returns the error. A successful reload clears the failure state,
//! so a later relapse logs again.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::config::ConfigError;

/// The `(mtime, size)` reload key; a file that fails to stat keys as zeros.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Fingerprint {
  mtime_seconds: i64,
  mtime_nanoseconds: i64,
  size: u64,
}

impl Fingerprint {
  fn of(path: &Path) -> Fingerprint {
    use std::os::unix::fs::MetadataExt;
    match std::fs::metadata(path) {
      Ok(metadata) => {
        Fingerprint { mtime_seconds: metadata.mtime(), mtime_nanoseconds: metadata.mtime_nsec(), size: metadata.size() }
      }
      Err(_) => Fingerprint { mtime_seconds: 0, mtime_nanoseconds: 0, size: 0 },
    }
  }
}

struct ReloadState<T> {
  value: Option<Arc<T>>,
  loaded: Option<Fingerprint>,
  failed: Option<Fingerprint>,
  warned: HashSet<String>,
}

type Loader<T> = Box<dyn Fn() -> Result<T, ConfigError> + Send + Sync>;

/// One hot-reloaded file and the value its loader builds from it.
pub struct HotReload<T> {
  file: PathBuf,
  event: &'static str,
  loader: Loader<T>,
  state: Mutex<ReloadState<T>>,
}

impl<T> HotReload<T> {
  /// A cache over `file`; `event` names the warning a failed reload logs.
  pub fn new(file: PathBuf, event: &'static str, loader: Loader<T>) -> HotReload<T> {
    HotReload {
      file,
      event,
      loader,
      state: Mutex::new(ReloadState { value: None, loaded: None, failed: None, warned: HashSet::new() }),
    }
  }

  /// The current value, reloaded first when the file moved since the last load or failure.
  pub fn get(&self) -> Result<Arc<T>, ConfigError> {
    let fingerprint = Fingerprint::of(&self.file);
    let mut state = self.state.lock().expect("the reload state lock is never poisoned");
    let fresh_needed =
      state.value.is_none() || (Some(fingerprint) != state.loaded && Some(fingerprint) != state.failed);
    if fresh_needed {
      match (self.loader)() {
        Ok(fresh) => {
          state.value = Some(Arc::new(fresh));
          state.loaded = Some(fingerprint);
          state.failed = None;
          state.warned.clear();
        }
        Err(error) => {
          if state.warned.insert(error.0.clone()) {
            tracing::warn!(error = %error.0, "{}", self.event);
          }
          if state.value.is_none() {
            return Err(error);
          }
          state.failed = Some(fingerprint);
        }
      }
    }
    Ok(Arc::clone(state.value.as_ref().expect("a value is cached past this point")))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::atomic::{AtomicUsize, Ordering};

  #[test]
  fn reloads_on_change_and_keeps_the_previous_value_on_failure() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("value.txt");
    std::fs::write(&file, "1").unwrap();
    let loads = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&loads);
    let path = file.clone();
    let cache = HotReload::new(
      file.clone(),
      "value_reload_failed",
      Box::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
        std::fs::read_to_string(&path).unwrap().trim().parse::<i64>().map_err(|error| ConfigError(error.to_string()))
      }),
    );
    assert_eq!(*cache.get().unwrap(), 1);
    assert_eq!(*cache.get().unwrap(), 1);
    assert_eq!(loads.load(Ordering::SeqCst), 1);
    std::fs::write(&file, "broken!").unwrap();
    assert_eq!(*cache.get().unwrap(), 1);
    assert_eq!(*cache.get().unwrap(), 1);
    assert_eq!(loads.load(Ordering::SeqCst), 2, "the failed fingerprint is not retried");
    std::fs::write(&file, "22").unwrap();
    assert_eq!(*cache.get().unwrap(), 22);
  }

  #[test]
  fn a_first_load_failure_is_returned() {
    let dir = tempfile::tempdir().unwrap();
    let cache: HotReload<i64> =
      HotReload::new(dir.path().join("x"), "x_reload_failed", Box::new(|| Err(ConfigError("broken".to_string()))));
    assert_eq!(cache.get().unwrap_err().0, "broken");
  }
}
