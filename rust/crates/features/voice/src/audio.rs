use std::collections::VecDeque;
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::{Mutex, Notify};
use tokio::time::Instant;

pub(crate) const SAMPLE_RATE: usize = 16_000;
pub(crate) const PREVIEW_QUEUE_SECONDS: usize = 30;
pub(crate) const MAX_RECORDING_SAMPLES: usize = 5 * 60 * SAMPLE_RATE;

struct QueueState {
  frames: VecDeque<Vec<u8>>,
  queued_bytes: usize,
  ended: bool,
  devices: Option<Value>,
  recording: Vec<u8>,
  end_frame_at: Option<Instant>,
}

pub(crate) struct AudioQueue {
  max_bytes: usize,
  recording_budget_bytes: usize,
  state: Mutex<QueueState>,
  changed: Notify,
}

impl AudioQueue {
  pub(crate) fn new() -> Arc<Self> {
    Self::with_limits(PREVIEW_QUEUE_SECONDS * SAMPLE_RATE * 2, MAX_RECORDING_SAMPLES * 2)
  }

  #[cfg(test)]
  pub(crate) fn new_with_recording_budget_for_test(recording_budget_bytes: usize) -> Arc<Self> {
    Self::with_limits(PREVIEW_QUEUE_SECONDS * SAMPLE_RATE * 2, recording_budget_bytes)
  }

  pub(crate) fn new_recording(max_bytes: usize) -> Arc<Self> {
    Self::with_limits(max_bytes, 0)
  }

  fn with_limits(max_bytes: usize, recording_budget_bytes: usize) -> Arc<Self> {
    Arc::new(Self {
      max_bytes,
      recording_budget_bytes,
      state: Mutex::new(QueueState {
        frames: VecDeque::new(),
        queued_bytes: 0,
        ended: false,
        devices: None,
        recording: Vec::new(),
        end_frame_at: None,
      }),
      changed: Notify::new(),
    })
  }

  pub(crate) async fn put(&self, frame: Vec<u8>) -> bool {
    let mut state = self.state.lock().await;
    if state.ended || state.queued_bytes + frame.len() > self.max_bytes {
      return false;
    }
    state.queued_bytes += frame.len();
    let room = self.recording_budget_bytes.saturating_sub(state.recording.len());
    state.recording.extend_from_slice(&frame[..frame.len().min(room)]);
    state.frames.push_back(frame);
    drop(state);
    self.changed.notify_one();
    true
  }

  pub(crate) async fn end(&self, devices: Option<Value>) {
    let mut state = self.state.lock().await;
    state.ended = true;
    state.devices = devices;
    state.end_frame_at = Some(Instant::now());
    drop(state);
    self.changed.notify_one();
  }

  pub(crate) async fn next_audio(&self) -> Option<Vec<u8>> {
    loop {
      let changed = self.changed.notified();
      {
        let mut state = self.state.lock().await;
        if let Some(frame) = state.frames.pop_front() {
          state.queued_bytes -= frame.len();
          return Some(frame);
        }
        if state.ended {
          return None;
        }
      }
      changed.await;
    }
  }

  pub(crate) async fn ended(&self) -> bool {
    self.state.lock().await.ended
  }

  pub(crate) async fn devices(&self) -> Option<Value> {
    self.state.lock().await.devices.clone()
  }

  pub(crate) async fn recording_pcm(&self) -> Vec<u8> {
    self.state.lock().await.recording.clone()
  }

  pub(crate) async fn recording_seconds(&self) -> f64 {
    self.state.lock().await.recording.len() as f64 / 2.0 / SAMPLE_RATE as f64
  }

  pub(crate) async fn seconds_since_end(&self) -> f64 {
    self
      .state
      .lock()
      .await
      .end_frame_at
      .expect("the recording is sealed before end-frame timing is read")
      .elapsed()
      .as_secs_f64()
  }
}
