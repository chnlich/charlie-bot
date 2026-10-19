//! The decode of a finished recording: the VAD windows, the recognizer pool and the segment join.
//!
//! Port of `src/features/voice/transcriber.py`. The pool holds sherpa recognizers on the CPU engine and the qwen3_hf
//! helper's recognizer on the GPU engine. Both decode one window at a time and hand back its text.

use std::cmp::Reverse;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, bail};
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::python_helper::{LongLived, PythonHelper};
use charliebot_core::pystr;
use sherpa_onnx::{
  OfflineModelConfig, OfflineQwen3ASRModelConfig, OfflineRecognizer, OfflineRecognizerConfig, SileroVadModelConfig,
  TenVadModelConfig, VadModelConfig, VoiceActivityDetector,
};

use crate::audio::SAMPLE_RATE;
use crate::config::{VoiceEngine, VoiceSettings};
use crate::provisioning::{QWEN3_ASR_DIR_NAME, VoiceModelPaths, ensure_sherpa_paths, get_ready_model_paths};
use crate::qwen3_hf::{HELPER_SCRIPT, Qwen3HfRecognizer};

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
  /// The qwen3_hf pool has one recognizer, whose helper decodes one request at a time.
  recognizers: Vec<Mutex<Recognizer>>,
  vad_config: VadModelConfig,
  /// The engine whose recognizers the pool holds: sherpa after a qwen3_hf fallback, even when `voice.engine` asks for
  /// qwen3_hf.
  engine: VoiceEngine,
  /// The model behind the pool: the sherpa archive's directory, or `voice.model_id` for qwen3_hf.
  model_id: String,
  /// The qwen3_hf helpers the server stops on shutdown. They sit outside the pool locks: a decode in flight holds its
  /// lock until its reply arrives, and the stop must not wait for that reply.
  helpers: Vec<Arc<LongLived>>,
}

/// One pool worker's recognizer: the sherpa recognizer on the CPU engine, or the qwen3_hf helper's on the GPU engine.
pub(crate) enum Recognizer {
  Sherpa(OfflineRecognizer),
  Qwen3Hf(Qwen3HfRecognizer),
}

/// One pool worker's recognizer: decodes a window of 16 kHz mono float samples into its text.
pub(crate) trait WindowDecoder: Send {
  fn decode_window(&mut self, samples: &[f32]) -> anyhow::Result<String>;
}

impl WindowDecoder for Recognizer {
  fn decode_window(&mut self, samples: &[f32]) -> anyhow::Result<String> {
    match self {
      Recognizer::Sherpa(recognizer) => recognizer.decode_window(samples),
      Recognizer::Qwen3Hf(recognizer) => recognizer.decode_window(samples),
    }
  }
}

impl WindowDecoder for OfflineRecognizer {
  fn decode_window(&mut self, samples: &[f32]) -> anyhow::Result<String> {
    let stream = self.create_stream();
    stream.accept_waveform(SAMPLE_RATE as i32, samples);
    self.decode(&stream);
    let result = stream.get_result().context("the recognizer returned no result")?;
    Ok(normalize_text(&result.text))
  }
}

/// `_decode_samples`' whitespace normalization: the text's words, joined by single spaces.
pub(crate) fn normalize_text(text: &str) -> String {
  text.split(pystr::is_python_whitespace).filter(|part| !part.is_empty()).collect::<Vec<_>>().join(" ")
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
    .map(|part| part.trim_matches(pystr::is_python_whitespace))
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
    .map(|_| create_recognizer(paths, hotwords, threads).map(|recognizer| Mutex::new(Recognizer::Sherpa(recognizer))))
    .collect::<anyhow::Result<Vec<_>>>()?;
  Ok(SpeechModelBundle {
    recognizers,
    vad_config: create_vad_config(paths),
    engine: VoiceEngine::Sherpa,
    model_id: QWEN3_ASR_DIR_NAME.to_string(),
    helpers: Vec::new(),
  })
}

/// The GPU bundle: one qwen3_hf helper, which loads the transformers model when it starts. The helper's text decodes
/// through the same VAD windows as the sherpa pool.
pub(crate) fn qwen3_hf_bundle(
  helper: PythonHelper,
  script: &Path,
  paths: &VoiceModelPaths,
  model_id: &str,
) -> anyhow::Result<SpeechModelBundle> {
  let recognizer = Qwen3HfRecognizer::start(helper, script)?;
  Ok(SpeechModelBundle {
    helpers: vec![recognizer.helper()],
    recognizers: vec![Mutex::new(Recognizer::Qwen3Hf(recognizer))],
    vad_config: create_vad_config(paths),
    engine: VoiceEngine::Qwen3Hf,
    model_id: model_id.to_string(),
  })
}

/// The bundle of `voice.engine`. A qwen3_hf bundle that fails to build falls back to the CPU bundle: voice stays alive,
/// and the warning names the failure.
fn create_bundle(
  cfg: &CharlieBotConfig,
  repo_root: &Path,
  paths: &VoiceModelPaths,
) -> anyhow::Result<SpeechModelBundle> {
  let settings = cfg.section::<VoiceSettings>("voice");
  match settings.engine {
    VoiceEngine::Sherpa => create_sherpa_bundle(paths, ""),
    VoiceEngine::Qwen3Hf => {
      let gpu = qwen3_hf_bundle(PythonHelper::new(repo_root), Path::new(HELPER_SCRIPT), paths, &settings.model_id);
      fall_back_to_cpu(gpu, || create_sherpa_bundle(&ensure_sherpa_paths(cfg)?, ""))
    }
  }
}

/// `_get_model_bundle`'s fallback: the qwen3_hf bundle, or when it failed to build, the CPU bundle that `cpu` builds.
fn fall_back_to_cpu(
  gpu: anyhow::Result<SpeechModelBundle>,
  cpu: impl FnOnce() -> anyhow::Result<SpeechModelBundle>,
) -> anyhow::Result<SpeechModelBundle> {
  match gpu {
    Ok(bundle) => Ok(bundle),
    Err(error) => {
      tracing::warn!(
        error = %format!("{error:#}"),
        engine = "qwen3_hf",
        fallback = "sherpa",
        "voice_gpu_engine_init_failed"
      );
      cpu()
    }
  }
}

/// The resident bundle, with the engine its config asked for.
struct ResidentBundle {
  requested: VoiceEngine,
  bundle: Arc<SpeechModelBundle>,
}

// The resident bundle is built once per process. The build lock is held across the build, so concurrent first
// requests share one bundle, and the bundle lock is held only for the swap.
static BUNDLE: Mutex<Option<ResidentBundle>> = Mutex::new(None);
static BUNDLE_BUILD_LOCK: Mutex<()> = Mutex::new(());

/// The resident bundle, when it was built for the engine `voice.engine` names now.
fn resident_bundle(requested: VoiceEngine) -> Option<Arc<SpeechModelBundle>> {
  let resident = BUNDLE.lock().expect("the bundle lock is never poisoned");
  resident.as_ref().filter(|resident| resident.requested == requested).map(|resident| Arc::clone(&resident.bundle))
}

/// The resident recognizer and VAD bundle for offline calls. Raises the not-ready error while provisioning is
/// incomplete, which the voice endpoints answer with 503.
pub(crate) fn get_transcription_bundle(
  cfg: &CharlieBotConfig,
  repo_root: &Path,
) -> anyhow::Result<Arc<SpeechModelBundle>> {
  let requested = cfg.section::<VoiceSettings>("voice").engine;
  let paths = get_ready_model_paths()?;
  if let Some(bundle) = resident_bundle(requested) {
    return Ok(bundle);
  }
  let _build = BUNDLE_BUILD_LOCK.lock().expect("the bundle build lock is never poisoned");
  if let Some(bundle) = resident_bundle(requested) {
    return Ok(bundle);
  }
  let bundle = Arc::new(create_bundle(cfg, repo_root, &paths)?);
  tracing::info!(engine = bundle.engine.name(), model_id = bundle.model_id.as_str(), "voice_model_bundle_ready");
  *BUNDLE.lock().expect("the bundle lock is never poisoned") =
    Some(ResidentBundle { requested, bundle: Arc::clone(&bundle) });
  Ok(bundle)
}

/// Stop the resident bundle's qwen3_hf helpers, so the server's shutdown leaves no helper process running. The bundle
/// stays resident: a decode after the stop fails as a decode after the helper's exit does, and nothing restarts it.
pub(crate) fn stop_resident_bundle() {
  if let Some(resident) = BUNDLE.lock().expect("the bundle lock is never poisoned").as_ref() {
    resident.bundle.stop();
  }
}

impl SpeechModelBundle {
  /// Kill the helpers' process groups. A decode in flight fails, and so does every later one.
  fn stop(&self) {
    for helper in &self.helpers {
      helper.stop();
    }
  }
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
  use std::fs;
  use std::io;
  use std::time::Duration;

  use anyhow::anyhow;

  use crate::provisioning::voice_model_paths;
  use crate::qwen3_hf::test_support::{StubTree, wait_until_gone};

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

  /// The sherpa-onnx release that the vendored sys crate pins is the release that uv.lock locks for the Python wheel.
  /// The pin is a constant in a build script, which this crate cannot link to, so the test reads its text.
  #[test]
  fn sherpa_pin_is_the_release_the_python_wheel_locks() {
    let uv_lock =
      fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../../uv.lock")).expect("read uv.lock");
    let locked = uv_lock
      .split("[[package]]\n")
      .find(|block| block.starts_with("name = \"sherpa-onnx-core\"\n"))
      .expect("uv.lock locks sherpa-onnx-core")
      .lines()
      .find_map(|line| line.strip_prefix("version = \"")?.strip_suffix('"'))
      .expect("the sherpa-onnx-core block has a version");
    let build_script = include_str!("../../../../vendor/sherpa-onnx-sys/build.rs");
    let pinned = build_script
      .split("const PIN: Pin = Pin {")
      .nth(1)
      .expect("the build script has the pin table")
      .lines()
      .find_map(|line| line.trim().strip_prefix("version: \"")?.strip_suffix("\","))
      .expect("the pin table has a version");
    assert_eq!(pinned, locked);
  }

  fn synthetic_paths() -> VoiceModelPaths {
    voice_model_paths(Path::new("/synthetic/home/models"))
  }

  fn qwen3_hf_bundle_of(tree: &StubTree, mode: &str) -> anyhow::Result<SpeechModelBundle> {
    qwen3_hf_bundle(tree.helper(mode), &tree.script(), &synthetic_paths(), "synthetic/model")
  }

  #[test]
  fn warm_up_decodes_one_half_second_sine_through_the_helper() {
    let tree = StubTree::new();
    let bundle = qwen3_hf_bundle_of(&tree, "ready").expect("the helper is ready");

    warm_up_bundle(&bundle).expect("the warm-up decodes");

    assert_eq!(tree.decoded_sample_counts(), vec![8_000]);
    assert_eq!(bundle.engine, VoiceEngine::Qwen3Hf);
    assert_eq!(bundle.model_id, "synthetic/model");
  }

  #[test]
  fn concurrent_windows_share_the_helper_and_each_gets_its_own_text() {
    let tree = StubTree::new();
    let bundle = qwen3_hf_bundle_of(&tree, "ready").expect("the helper is ready");

    thread::scope(|scope| {
      for marker in 1..=4 {
        let bundle = &bundle;
        scope.spawn(move || {
          for _ in 0..3 {
            let window = [marker as f32 / 1000.0; 8];
            let text = decode_samples(&bundle.recognizers[0], &window).expect("the window decodes");
            assert_eq!(text, format!("marker {marker}"));
          }
        });
      }
    });
  }

  #[test]
  fn stopping_the_bundle_kills_the_helpers_children_and_fails_later_decodes() {
    let tree = StubTree::new();
    let bundle = qwen3_hf_bundle_of(&tree, "ready").expect("the helper is ready");

    bundle.stop();

    wait_until_gone(tree.child_pid());
    let error = decode_samples(&bundle.recognizers[0], &[0.5]).expect_err("the stopped helper cannot decode");
    assert_eq!(error.to_string(), "Python helper qwen3_hf stopped answering: the server stopped it");
  }

  /// The log lines `run` writes, as the server's formatter renders them.
  fn logged_by(run: impl FnOnce()) -> String {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&buffer);
    let subscriber =
      tracing_subscriber::fmt().with_ansi(false).with_writer(move || LogSink(Arc::clone(&sink))).finish();
    tracing::subscriber::with_default(subscriber, run);
    let bytes = buffer.lock().expect("the log buffer is never poisoned").clone();
    String::from_utf8(bytes).expect("the log is UTF-8")
  }

  /// A log writer that appends to a buffer the test reads back.
  struct LogSink(Arc<Mutex<Vec<u8>>>);

  impl io::Write for LogSink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
      self.0.lock().expect("the log buffer is never poisoned").extend_from_slice(bytes);
      Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
      Ok(())
    }
  }

  #[test]
  fn init_error_falls_back_to_the_cpu_bundle_and_logs_the_gpu_failure() {
    let tree = StubTree::new();
    let gpu = qwen3_hf_bundle_of(&tree, "init_error");
    let mut outcome = None;

    let logs = logged_by(|| outcome = Some(fall_back_to_cpu(gpu, || Err(anyhow!("the CPU bundle was built")))));

    let error = outcome.expect("the fallback ran").err().expect("the CPU builder's result is returned");
    assert_eq!(error.to_string(), "the CPU bundle was built");
    assert!(logs.contains("voice_gpu_engine_init_failed"), "{logs}");
    assert!(logs.contains("error=synthetic init failure"), "{logs}");
    assert!(logs.contains("engine=\"qwen3_hf\""), "{logs}");
    assert!(logs.contains("fallback=\"sherpa\""), "{logs}");
  }

  #[test]
  fn exit_before_ready_falls_back_to_the_cpu_bundle_and_logs_the_exit() {
    let tree = StubTree::new();
    let gpu = qwen3_hf_bundle_of(&tree, "exit_before_ready");
    let mut outcome = None;

    let logs = logged_by(|| outcome = Some(fall_back_to_cpu(gpu, || Err(anyhow!("the CPU bundle was built")))));

    assert_eq!(
      outcome.expect("the fallback ran").err().expect("the CPU bundle is returned").to_string(),
      "the CPU bundle was built"
    );
    assert!(logs.contains("voice_gpu_engine_init_failed"), "{logs}");
    assert!(logs.contains("exited with code 3"), "{logs}");
  }
}
