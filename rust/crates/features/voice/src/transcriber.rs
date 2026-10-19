//! The sherpa decode of a finished recording: the VAD windows, the recognizer pool and the segment join.
//!
//! Port of `src/features/voice/transcriber.py` for the CPU sherpa engine. The qwen3_hf engine is not ported.

use std::cmp::Reverse;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, bail};
use charliebot_core::config::CharlieBotConfig;
use sherpa_onnx::{
  OfflineModelConfig, OfflineQwen3ASRModelConfig, OfflineRecognizer, OfflineRecognizerConfig, SileroVadModelConfig,
  TenVadModelConfig, VadModelConfig, VoiceActivityDetector,
};

use crate::audio::SAMPLE_RATE;
use crate::config::{VoiceEngine, VoiceSettings};
use crate::provisioning::{QWEN3_ASR_DIR_NAME, SpeechModelsNotReadyError, VoiceModelPaths, get_ready_model_paths};
use crate::transcription::python_whitespace;

/// A segment's decode keeps 5 s of pause before it, so quiet speech the detector labels silence still reaches the model.
const SEGMENT_DECODE_PAUSE_SAMPLES: usize = 5 * SAMPLE_RATE;
/// The padding after a segment's speech end, so a word tail just past the cut still decodes.
const SEGMENT_DECODE_PAD_SAMPLES: usize = 6_400;
/// The VAD is fed in 128 ms steps, the browser worklet's capture cadence, so a sentence onset stays inside the window.
const OFFLINE_VAD_FEED_SAMPLES: usize = 2_048;
const DECODE_WORKER_CAP: usize = 4;
const MAX_TOTAL_LEN: i32 = 1024;
const MAX_NEW_TOKENS: i32 = 256;
const WARMUP_SECONDS: f64 = 0.5;
const WARMUP_FREQUENCY_HZ: f64 = 440.0;

/// One closed speech segment and its padded decode window, in samples: `start` and `end` are the detector's speech
/// bounds, `left` and `right` the window the decode reads. Windows never overlap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DecodeWindow {
  pub(crate) start: usize,
  pub(crate) end: usize,
  pub(crate) left: usize,
  pub(crate) right: usize,
}

/// The windows and the joined text of one offline decode.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct OfflineDecode {
  pub(crate) windows: Vec<DecodeWindow>,
  pub(crate) text: String,
}

/// The resident bundle: one recognizer per pool worker, each behind its own lock, and the VAD configuration.
pub(crate) struct SpeechModelBundle {
  recognizers: Vec<Mutex<OfflineRecognizer>>,
  vad_config: VadModelConfig,
}

/// One pool worker's recognizer: decodes a window of 16 kHz mono float samples into its text.
pub(crate) trait WindowDecoder: Send {
  fn decode_window(&mut self, samples: &[f32]) -> anyhow::Result<String>;
}

impl WindowDecoder for OfflineRecognizer {
  fn decode_window(&mut self, samples: &[f32]) -> anyhow::Result<String> {
    let stream = self.create_stream();
    stream.accept_waveform(SAMPLE_RATE as i32, samples);
    self.decode(&stream);
    let result = stream.get_result().context("the recognizer returned no result")?;
    Ok(result.text.split(python_whitespace).filter(|part| !part.is_empty()).collect::<Vec<_>>().join(" "))
  }
}

fn logical_cpus() -> usize {
  // Python's `os.cpu_count() or 1`.
  let cpus = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
  usize::try_from(cpus).ok().filter(|cpus| *cpus > 0).unwrap_or(1)
}

/// The pool's worker count: one recognizer per two logical CPUs, capped.
fn decode_workers(cpus: usize) -> usize {
  (cpus / 2).clamp(1, DECODE_WORKER_CAP)
}

/// Each recognizer's ONNX thread count: the CPUs split across the workers, capped.
fn decode_threads(cpus: usize, workers: usize) -> usize {
  (cpus / workers).clamp(1, DECODE_WORKER_CAP)
}

fn create_vad_config(paths: &VoiceModelPaths) -> VadModelConfig {
  VadModelConfig {
    silero_vad: SileroVadModelConfig {
      model: Some(paths.silero_vad.display().to_string()),
      threshold: 0.5,
      // A measured 0.93 s mid-sentence pause split a phrase; only pauses of 1.0 s or more end a segment.
      min_silence_duration: 1.0,
      min_speech_duration: 0.25,
      window_size: 512,
      max_speech_duration: 20.0,
    },
    // The ten-VAD fields Python's VadModelConfig() defaults to; the silero model is the one in use.
    ten_vad: TenVadModelConfig {
      model: None,
      threshold: 0.5,
      min_silence_duration: 0.5,
      min_speech_duration: 0.25,
      window_size: 256,
      max_speech_duration: 20.0,
    },
    sample_rate: SAMPLE_RATE as i32,
    num_threads: 1,
    provider: Some("cpu".to_string()),
    debug: false,
  }
}

fn open_vad(config: &VadModelConfig, buffer_seconds: f32) -> anyhow::Result<VoiceActivityDetector> {
  VoiceActivityDetector::create(config, buffer_seconds).context("create the Silero voice activity detector")
}

/// Feed the VAD the whole recording in fixed steps, flush it, and return the closed speech segments as (start, end).
fn offline_vad_segments(vad: &VoiceActivityDetector, feed: &[f32]) -> anyhow::Result<Vec<(usize, usize)>> {
  for chunk in feed.chunks(OFFLINE_VAD_FEED_SAMPLES) {
    vad.accept_waveform(chunk);
  }
  vad.flush();
  let mut segments = Vec::new();
  while !vad.is_empty() {
    let segment = vad.front().context("the detector reports a segment that is not there")?;
    let start = usize::try_from(segment.start()).context("the detector reports a negative segment start")?;
    segments.push((start, start + segment.n() as usize));
    vad.pop();
  }
  Ok(segments)
}

/// Segment a recording through the opened VAD and give each segment its decode window. The left edge is clipped
/// against the previous window's right edge, so windows never overlap.
pub(crate) fn offline_decode_windows(vad: &VoiceActivityDetector, feed: &[f32]) -> anyhow::Result<Vec<DecodeWindow>> {
  let mut windows = Vec::new();
  let mut decoded_region_end = 0;
  for (start, end) in offline_vad_segments(vad, feed)? {
    let left = decoded_region_end.max(start.saturating_sub(SEGMENT_DECODE_PAUSE_SAMPLES));
    let right = feed.len().min(end + SEGMENT_DECODE_PAD_SAMPLES);
    windows.push(DecodeWindow { start, end, left, right });
    decoded_region_end = right;
  }
  Ok(windows)
}

/// Decode one window with the pool's recognizers, longest padded span claimed first, and join the texts in window
/// order: the recognizer that frees first takes the next window, so completion order never reaches the transcript.
fn decode_windows<D: WindowDecoder>(
  decoders: &[Mutex<D>],
  feed: &[f32],
  windows: &[DecodeWindow],
) -> anyhow::Result<String> {
  // The span is the one pre-decode cost signal, and longest-first is the claim order that keeps no recognizer idle
  // while a long window waits.
  let mut claim_order: Vec<usize> = (0..windows.len()).collect();
  claim_order.sort_by_key(|&index| Reverse(windows[index].right - windows[index].left));
  let next_claim = AtomicUsize::new(0);
  let claim = || claim_order.get(next_claim.fetch_add(1, Ordering::Relaxed)).copied();
  let workers = decoders.len().min(windows.len());
  let decoded: Vec<anyhow::Result<Vec<(usize, String)>>> = thread::scope(|scope| {
    let handles: Vec<_> = (0..workers)
      .map(|worker| {
        let claim = &claim;
        scope.spawn(move || -> anyhow::Result<Vec<(usize, String)>> {
          let mut texts = Vec::new();
          while let Some(index) = claim() {
            let window = windows[index];
            texts.push((index, decode_samples(&decoders[worker], &feed[window.left..window.right])?));
          }
          Ok(texts)
        })
      })
      .collect();
    handles.into_iter().map(|handle| handle.join().expect("a decode thread does not panic")).collect()
  });
  let mut texts = vec![String::new(); windows.len()];
  for worker in decoded {
    for (index, text) in worker? {
      texts[index] = text;
    }
  }
  Ok(join_segments(&texts))
}

/// One window's decode under its recognizer's lock. An empty window decodes to nothing without taking the lock.
fn decode_samples<D: WindowDecoder>(decoder: &Mutex<D>, samples: &[f32]) -> anyhow::Result<String> {
  if samples.is_empty() {
    return Ok(String::new());
  }
  decoder.lock().expect("a recognizer lock is never poisoned").decode_window(samples)
}

fn join_segments(parts: &[String]) -> String {
  parts
    .iter()
    .map(|part| part.trim_matches(python_whitespace))
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join(" ")
}

/// Decode a complete 16 kHz mono PCM16 recording in one pass: the windows and the transcript.
pub(crate) fn decode_pcm_offline(bundle: &SpeechModelBundle, pcm: &[u8]) -> anyhow::Result<OfflineDecode> {
  if !pcm.len().is_multiple_of(2) {
    bail!("invalid PCM frame: byte length must be even");
  }
  let (pairs, _) = pcm.as_chunks::<2>();
  let feed: Vec<f32> = pairs.iter().map(|pair| f32::from(i16::from_le_bytes(*pair)) / 32768.0).collect();
  // The detector's internal buffer covers the whole recording, so it cannot wrap however long the input is.
  let buffer_seconds = (feed.len() as f64 / SAMPLE_RATE as f64 + 10.0) as f32;
  let vad = open_vad(&bundle.vad_config, buffer_seconds)?;
  let windows = offline_decode_windows(&vad, &feed)?;
  let text = decode_windows(&bundle.recognizers, &feed, &windows)?;
  Ok(OfflineDecode { windows, text })
}

/// The transcript of [`decode_pcm_offline`].
pub(crate) fn transcribe_pcm_offline(bundle: &SpeechModelBundle, pcm: &[u8]) -> anyhow::Result<String> {
  Ok(decode_pcm_offline(bundle, pcm)?.text)
}

fn create_recognizer(paths: &VoiceModelPaths, hotwords: &str, threads: usize) -> anyhow::Result<OfflineRecognizer> {
  let mut config = OfflineRecognizerConfig::default();
  config.feat_config.feature_dim = 128;
  config.decoding_method = Some("greedy_search".to_string());
  config.hotwords_score = 1.5;
  config.model_config = OfflineModelConfig {
    qwen3_asr: OfflineQwen3ASRModelConfig {
      conv_frontend: Some(paths.qwen3_conv_frontend.display().to_string()),
      encoder: Some(paths.qwen3_encoder.display().to_string()),
      decoder: Some(paths.qwen3_decoder.display().to_string()),
      tokenizer: Some(paths.qwen3_tokenizer.display().to_string()),
      max_total_len: MAX_TOTAL_LEN,
      max_new_tokens: MAX_NEW_TOKENS,
      hotwords: Some(hotwords.to_string()),
      ..Default::default()
    },
    modeling_unit: Some("cjkchar".to_string()),
    num_threads: threads as i32,
    debug: false,
    provider: Some("cpu".to_string()),
    ..Default::default()
  };
  OfflineRecognizer::create(&config).context("create the Qwen3-ASR recognizer")
}

/// Build the CPU sherpa bundle: the pool's recognizers and the VAD configuration. `hotwords` reaches the recognizer's
/// biasing parameter verbatim; production passes nothing.
pub(crate) fn create_sherpa_bundle(paths: &VoiceModelPaths, hotwords: &str) -> anyhow::Result<SpeechModelBundle> {
  let cpus = logical_cpus();
  let workers = decode_workers(cpus);
  let threads = decode_threads(cpus, workers);
  let recognizers = (0..workers)
    .map(|_| create_recognizer(paths, hotwords, threads).map(Mutex::new))
    .collect::<anyhow::Result<Vec<_>>>()?;
  Ok(SpeechModelBundle { recognizers, vad_config: create_vad_config(paths) })
}

// The resident bundle is built once per process. The build lock is held across the build, so concurrent first
// requests share one bundle, and the bundle lock is held only for the swap.
static BUNDLE: Mutex<Option<Arc<SpeechModelBundle>>> = Mutex::new(None);
static BUNDLE_BUILD_LOCK: Mutex<()> = Mutex::new(());

fn resident_bundle() -> Option<Arc<SpeechModelBundle>> {
  BUNDLE.lock().expect("the bundle lock is never poisoned").clone()
}

/// The resident recognizer and VAD bundle for offline calls. Raises the not-ready error while provisioning is
/// incomplete, which the voice endpoints answer with 503.
pub(crate) fn get_transcription_bundle(cfg: &CharlieBotConfig) -> anyhow::Result<Arc<SpeechModelBundle>> {
  if matches!(cfg.section::<VoiceSettings>("voice").engine, VoiceEngine::Qwen3Hf) {
    return Err(
      SpeechModelsNotReadyError::new("speech models are not ready: the qwen3_hf engine is not ported".into()).into(),
    );
  }
  let paths = get_ready_model_paths()?;
  if let Some(bundle) = resident_bundle() {
    return Ok(bundle);
  }
  let _build = BUNDLE_BUILD_LOCK.lock().expect("the bundle build lock is never poisoned");
  if let Some(bundle) = resident_bundle() {
    return Ok(bundle);
  }
  let bundle = Arc::new(create_sherpa_bundle(&paths, "")?);
  tracing::info!(engine = "sherpa", model_id = QWEN3_ASR_DIR_NAME, "voice_model_bundle_ready");
  *BUNDLE.lock().expect("the bundle lock is never poisoned") = Some(Arc::clone(&bundle));
  Ok(bundle)
}

/// Decode one deterministic 0.5 s 440 Hz sine through every pool recognizer. The decoded text is dropped: the point
/// is the first-decode cost each recognizer pays, which a same-shape sine pays identically.
pub(crate) fn warm_up_bundle(bundle: &SpeechModelBundle) -> anyhow::Result<()> {
  let sine: Vec<f32> = (0..(SAMPLE_RATE as f64 * WARMUP_SECONDS) as usize)
    .map(|position| {
      (2.0 * std::f64::consts::PI * WARMUP_FREQUENCY_HZ * position as f64 / SAMPLE_RATE as f64).sin() as f32
    })
    .collect();
  for recognizer in &bundle.recognizers {
    decode_samples(recognizer, &sine)?;
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::time::Duration;

  /// Decodes a window's first sample (times 32768) as its marker `w<marker>`, after the delay the test gives that
  /// marker. Every entry is logged, so a test can see which recognizer decoded which window.
  struct StubDecoder {
    delays: &'static [(i32, u64)],
    fail_on: Option<i32>,
    entered: Arc<Mutex<Vec<i32>>>,
  }

  impl WindowDecoder for StubDecoder {
    fn decode_window(&mut self, samples: &[f32]) -> anyhow::Result<String> {
      let marker = (samples[0] * 32768.0).round() as i32;
      self.entered.lock().expect("the entry log is never poisoned").push(marker);
      let delay = self.delays.iter().find(|(window, _)| *window == marker).map_or(0, |(_, millis)| *millis);
      thread::sleep(Duration::from_millis(delay));
      if self.fail_on == Some(marker) {
        bail!("window w{marker} decode failed");
      }
      Ok(format!("w{marker}"))
    }
  }

  fn stub(delays: &'static [(i32, u64)], fail_on: Option<i32>) -> (Mutex<StubDecoder>, Arc<Mutex<Vec<i32>>>) {
    let entered = Arc::new(Mutex::new(Vec::new()));
    (Mutex::new(StubDecoder { delays, fail_on, entered: Arc::clone(&entered) }), entered)
  }

  /// Window k's samples all carry marker k, so the stub's text names its window.
  fn windows_and_feed(count: usize) -> (Vec<DecodeWindow>, Vec<f32>) {
    let windows = (0..count)
      .map(|k| DecodeWindow { start: k * 16, end: (k + 1) * 16, left: k * 16, right: (k + 1) * 16 })
      .collect();
    let mut feed = vec![0.0_f32; count * 16];
    for marker in 0..count {
      feed[marker * 16..(marker + 1) * 16].fill(marker as f32 / 32768.0);
    }
    (windows, feed)
  }

  #[test]
  fn decode_joins_in_window_order_not_completion_order() {
    // Window 0 decodes slowest, so completion order is w4..w0; the transcript still reads w0..w4.
    let (windows, feed) = windows_and_feed(5);
    let (first, first_entries) = stub(&[(0, 300)], None);
    let (second, second_entries) = stub(&[(0, 300)], None);
    let text = decode_windows(&[first, second], &feed, &windows).expect("the decode succeeds");
    assert_eq!(text, "w0 w1 w2 w3 w4");
    assert!(!first_entries.lock().unwrap().is_empty());
    assert!(!second_entries.lock().unwrap().is_empty());
  }

  #[test]
  fn a_single_recognizer_decodes_every_window_in_order() {
    let (windows, feed) = windows_and_feed(4);
    let (solo, _) = stub(&[], None);
    assert_eq!(decode_windows(&[solo], &feed, &windows).expect("the decode succeeds"), "w0 w1 w2 w3");
  }

  #[test]
  fn concurrent_decodes_never_share_a_recognizer() {
    // Two simultaneous requests share one pool. While the first call's w0 holds its recognizer, the second call's
    // window on that recognizer waits on the lock: the holder's entry log keeps a single entry.
    let (windows, feed) = windows_and_feed(2);
    let (first, first_entries) = stub(&[(0, 200)], None);
    let (second, second_entries) = stub(&[(0, 200)], None);
    let decoders = [first, second];
    thread::scope(|scope| {
      let first_call = scope.spawn(|| decode_windows(&decoders, &feed, &windows));
      let holder = loop {
        if first_entries.lock().unwrap().contains(&0) {
          break Arc::clone(&first_entries);
        }
        if second_entries.lock().unwrap().contains(&0) {
          break Arc::clone(&second_entries);
        }
        thread::sleep(Duration::from_millis(1));
      };
      let second_call = scope.spawn(|| decode_windows(&decoders, &feed, &windows));
      thread::sleep(Duration::from_millis(50));
      assert_eq!(holder.lock().unwrap().iter().filter(|marker| **marker == 0).count(), 1);
      assert_eq!(first_call.join().expect("no panic").expect("the first decode succeeds"), "w0 w1");
      assert_eq!(second_call.join().expect("no panic").expect("the second decode succeeds"), "w0 w1");
    });
  }

  #[test]
  fn a_failing_window_fails_the_whole_call() {
    let (windows, feed) = windows_and_feed(3);
    let (first, _) = stub(&[], Some(2));
    let (second, _) = stub(&[], Some(2));
    let error = decode_windows(&[first, second], &feed, &windows).expect_err("the failing window fails the call");
    assert!(error.to_string().contains("window w2 decode failed"), "unexpected error: {error}");
  }
}
