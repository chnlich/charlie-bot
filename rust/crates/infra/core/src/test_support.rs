//! The tracing capture that every crate's tests share. A test that reads rendered log lines uses [`LogCapture`]; a test
//! that reads event fields implements [`EventSink`] and installs it with [`install_events`].
//!
//! One global subscriber serves every thread and never changes; a capture is a thread-local sink that the subscriber's
//! fmt writer or event layer consults. A scoped `set_default` per capture would not do: tracing-core 0.1.36 has one
//! registered dispatcher. That dispatcher computes a new call site's cached interest from the registering thread's
//! default alone. A call site first reached on a thread with no subscriber then caches `never` for every thread.

use std::cell::RefCell;
use std::io::{self, Write};
use std::sync::{Arc, Mutex, Once};

use tracing::Event;
use tracing::Subscriber;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

thread_local! {
  /// The capture this thread installed. One thread holds one capture at a time; a thread without one logs nowhere.
  static CURRENT: RefCell<Option<Capture>> = const { RefCell::new(None) };
}

static SUBSCRIBER: Once = Once::new();

/// What a thread installed: rendered lines into a [`LogCapture`], or raw events into an [`EventSink`].
#[derive(Clone)]
enum Capture {
  Text(LogCapture),
  Events(Arc<dyn EventSink>),
}

/// The text one test thread logs between [`LogCapture::install`] and the drop of its guard.
#[derive(Clone, Default)]
pub struct LogCapture(Arc<Mutex<Vec<u8>>>);

impl LogCapture {
  /// Routes the current thread's events into this capture until the returned guard drops.
  pub fn install(&self) -> CaptureGuard {
    install_capture(Capture::Text(self.clone()))
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

/// A test's receiver of structured events: the subscriber hands each event on the installing thread to the sink while
/// it emits, and the sink visits the fields it reads.
pub trait EventSink {
  /// Records one event.
  fn event(&self, event: &Event<'_>);
}

/// Routes the current thread's events into `sink` until the returned guard drops.
pub fn install_events(sink: Arc<dyn EventSink>) -> CaptureGuard {
  install_capture(Capture::Events(sink))
}

fn install_capture(capture: Capture) -> CaptureGuard {
  SUBSCRIBER.call_once(|| {
    let subscriber = tracing_subscriber::registry()
      .with(
        tracing_subscriber::fmt::layer()
          .with_ansi(false)
          .with_writer(ThreadText)
          .with_filter(tracing_subscriber::filter::LevelFilter::INFO),
      )
      .with(EventDispatch);
    tracing::subscriber::set_global_default(subscriber).expect("the capture subscriber is the only global default");
  });
  // A thread that reached a call site before the global default existed cached `never` there, and that store can
  // land after the `set_global_default` runs. Rebuilding on every install, before the test emits, recovers such a
  // call site.
  tracing::callsite::rebuild_interest_cache();
  CaptureGuard(CURRENT.with(|slot| slot.replace(Some(capture))))
}

/// Restores the capture that was current on this thread before the install.
pub struct CaptureGuard(Option<Capture>);

impl Drop for CaptureGuard {
  fn drop(&mut self) {
    CURRENT.with(|slot| slot.replace(self.0.take()));
  }
}

/// The writer the fmt layer asks for on each event: the emitting thread's text capture, or nowhere if it has none.
struct ThreadText;

impl<'a> MakeWriter<'a> for ThreadText {
  type Writer = Box<dyn Write>;

  fn make_writer(&'a self) -> Self::Writer {
    match CURRENT.with(|slot| slot.borrow().clone()) {
      Some(Capture::Text(capture)) => Box::new(capture),
      _ => Box::new(io::sink()),
    }
  }
}

/// The layer that hands each event to the emitting thread's event sink, if it installed one.
struct EventDispatch;

impl<S: Subscriber> Layer<S> for EventDispatch {
  fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
    let sink = CURRENT.with(|slot| match slot.borrow().as_ref() {
      Some(Capture::Events(sink)) => Some(Arc::clone(sink)),
      _ => None,
    });
    if let Some(sink) = sink {
      sink.event(event);
    }
  }
}
