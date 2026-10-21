//! The tracing capture of the charliebot-tasks tests. Each test thread's events land in its own [`LogCapture`].
//!
//! One global subscriber serves every thread and never changes. A capture is a thread-local sink that the subscriber's
//! writer consults. A scoped `set_default` per capture would not do: while tracing-core 0.1.36 has one registered
//! dispatcher, it computes a new call site's cached interest from the registering thread's default alone. A call site
//! first reached on a thread with no subscriber then caches `never` for every thread.

use std::cell::RefCell;
use std::io::{self, Write};
use std::sync::{Arc, Mutex, Once};

use tracing_subscriber::fmt::MakeWriter;

thread_local! {
  static CAPTURE: RefCell<Option<LogCapture>> = const { RefCell::new(None) };
}

static SUBSCRIBER: Once = Once::new();

/// The text one test thread logs between [`LogCapture::install`] and the drop of its guard.
#[derive(Clone, Default)]
pub struct LogCapture(Arc<Mutex<Vec<u8>>>);

impl LogCapture {
  /// Routes the current thread's events into this capture until the returned guard drops.
  pub fn install(&self) -> CaptureGuard {
    SUBSCRIBER.call_once(|| {
      let subscriber = tracing_subscriber::fmt()
        .with_writer(ThreadCapture)
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
      tracing::subscriber::set_global_default(subscriber).expect("the capture subscriber is the only global default");
      // A thread that reached a call site while the global was still being stored cached `never` there.
      tracing::callsite::rebuild_interest_cache();
    });
    CaptureGuard(CAPTURE.with(|slot| slot.replace(Some(self.clone()))))
  }

  /// The text captured so far.
  pub fn text(&self) -> String {
    String::from_utf8_lossy(&self.0.lock().expect("log capture is not poisoned")).into_owned()
  }
}

impl Write for LogCapture {
  fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
    self.0.lock().expect("log capture is not poisoned").extend_from_slice(buf);
    Ok(buf.len())
  }

  fn flush(&mut self) -> io::Result<()> {
    Ok(())
  }
}

/// Restores the capture that was current on this thread before [`LogCapture::install`].
pub struct CaptureGuard(Option<LogCapture>);

impl Drop for CaptureGuard {
  fn drop(&mut self) {
    CAPTURE.with(|slot| slot.replace(self.0.take()));
  }
}

/// The writer the subscriber asks for on each event: the emitting thread's capture, or nowhere if it has none.
struct ThreadCapture;

impl<'a> MakeWriter<'a> for ThreadCapture {
  type Writer = Box<dyn Write>;

  fn make_writer(&'a self) -> Self::Writer {
    match CAPTURE.with(|slot| slot.borrow().clone()) {
      Some(capture) => Box::new(capture),
      None => Box::new(io::sink()),
    }
  }
}
