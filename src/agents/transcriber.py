"""Record-then-upload speech transcription with a dual engine: sherpa (CPU) or qwen3_hf (GPU).

The default 'sherpa' engine runs the int8 sherpa-onnx Qwen3-ASR pipeline on CPU. The
'qwen3_hf' engine (cfg.voice.engine) runs the official transformers Qwen3-ASR weights on
NVIDIA GPUs; it needs the gpu-voice dependency group, which only GPU hosts install, so
every torch/transformers import here is lazy and branch-local.

Decoding is offline: transcribe_pcm_offline takes a complete recording, segments it with
the bundle's VAD, and decodes each segment in one shot. There is no incremental state.
"""

from __future__ import annotations

import hashlib
import os
import shutil
import tarfile
import threading
import urllib.request
import uuid
from dataclasses import dataclass, replace
from pathlib import Path
from types import SimpleNamespace
from typing import BinaryIO

import numpy as np
import structlog

from src.core.config import CharlieBotConfig
from src.core.json_utils import atomic_write_stream
from src.core.timeouts import HTTP_MODEL_DOWNLOAD_TIMEOUT

log = structlog.get_logger()

SAMPLE_RATE = 16_000
MAX_RECORDING_SECONDS = 5 * 60
MAX_RECORDING_SAMPLES = SAMPLE_RATE * MAX_RECORDING_SECONDS
# A segment's decode starts where the previous one's ended, pause included, so quiet
# speech the detector labels silence still reaches the model. The cap bounds one
# decode at 5s pause + the detector's 20s max_speech_duration + 0.4s tail = 25.4s,
# about 330 audio positions plus 256 new tokens, inside the CPU engine's
# max_total_len=1024.
SEGMENT_DECODE_PAUSE_SAMPLES = 5 * SAMPLE_RATE
# Padding after a segment's speech end, so a word tail just past the cut still decodes.
SEGMENT_DECODE_PAD_SAMPLES = 6_400
# The offline path feeds the VAD in 128 ms steps, the browser worklet's capture-chunk
# cadence. sherpa marks a segment's start at most 2*WindowSize +
# min_speech_duration = 2*512 + 4000 = 5024 samples (~0.31 s) before the end of the
# feed where speech is first detected, so a feed larger than that clips the sentence
# onset (one whole-recording feed loses everything before tail - 5024); at 128 ms any
# onset alignment stays inside the back-dated window, while a single large feed drops
# a replayed sentence's opening.
OFFLINE_VAD_FEED_SAMPLES = 2048

QWEN3_ASR_DIR_NAME = "sherpa-onnx-qwen3-asr-0.6B-int8-2026-03-25"
QWEN3_ASR_ARCHIVE_NAME = f"{QWEN3_ASR_DIR_NAME}.tar.bz2"
QWEN3_ASR_URL = (
    "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/"
    "sherpa-onnx-qwen3-asr-0.6B-int8-2026-03-25.tar.bz2")
QWEN3_ASR_SHA256 = "393f8a14e2f5fb96746aaab342997a40641001fbd5bf9592a080a8329178ee96"

SILERO_VAD_NAME = "silero_vad.onnx"
SILERO_VAD_URL = "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx"
SILERO_VAD_SHA256 = "9e2449e1087496d8d4caba907f23e0bd3f78d91fa552479bb9c23ac09cbb1fd6"


class SpeechModelsNotReadyError(RuntimeError):
  """Raised when a voice request arrives before model provisioning is complete."""


@dataclass(frozen=True)
class VoiceModelPaths:
  cache_dir: Path
  qwen3_archive: Path
  qwen3_dir: Path
  qwen3_conv_frontend: Path
  qwen3_encoder: Path
  qwen3_decoder: Path
  qwen3_tokenizer: Path
  silero_vad: Path
  # qwen3_hf engine only: local snapshot dir of cfg.voice.model_id under cache_dir,
  # published by ensure_models_cached after snapshot_download. None for the sherpa engine.
  hf_snapshot: Path | None = None


@dataclass
class _SpeechModelBundle:
  recognizer: object
  vad_config: object
  decode_lock: threading.Lock
  # The engine whose decoder actually produced `recognizer` — 'sherpa' after a GPU
  # fallback even when cfg.voice.engine is 'qwen3_hf' — and the model id behind it.
  engine: str
  model_id: str


_state_lock = threading.Lock()
# Dedicated build lock for _get_model_bundle: a ~7 s model build holds this, never
# _state_lock, so readiness readers (get_ready_model_paths) never wait on a build.
_bundle_build_lock = threading.Lock()
_provisioning_started = False
_ready_paths: VoiceModelPaths | None = None
_provisioning_error: str | None = None
_bundle: _SpeechModelBundle | None = None
# The cfg.voice.engine the cached _bundle answers for; a bundle built as a fallback
# keeps the requesting engine here so the cache check still hits on the next session.
_bundle_engine: str | None = None


def reset_bundle_cache_for_tests() -> None:
  """Clear the process-wide bundle cache; the voice test suites call this around each test.

  A stub (sherpa suite) or GPU (qwen3_hf suite) bundle left in the cache would
  otherwise leak into whichever suite runs next in the same process.
  """
  global _bundle, _bundle_engine
  with _state_lock:
    _bundle = None
    _bundle_engine = None


def voice_model_paths(cfg: CharlieBotConfig) -> VoiceModelPaths:
  cache_dir = cfg.charliebot_home / "models"
  qwen3_dir = cache_dir / QWEN3_ASR_DIR_NAME
  return VoiceModelPaths(
      cache_dir=cache_dir,
      qwen3_archive=cache_dir / QWEN3_ASR_ARCHIVE_NAME,
      qwen3_dir=qwen3_dir,
      qwen3_conv_frontend=qwen3_dir / "conv_frontend.onnx",
      qwen3_encoder=qwen3_dir / "encoder.int8.onnx",
      qwen3_decoder=qwen3_dir / "decoder.int8.onnx",
      qwen3_tokenizer=qwen3_dir / "tokenizer",
      silero_vad=cache_dir / SILERO_VAD_NAME,
  )


def provision_models(cfg: CharlieBotConfig) -> None:
  """Provision the speech models once per process; failures park the error.

  Runs on a worker thread (server._provision_speech_models): this module's numpy
  import and everything torch-adjacent behind it must stay off the event loop's
  startup path, so the caller imports this module inside its thread. A provisioning
  failure lands in _provisioning_error for get_ready_model_paths to raise.
  """
  global _provisioning_started, _provisioning_error, _ready_paths
  with _state_lock:
    if _provisioning_started:
      return
    _provisioning_started = True
    _provisioning_error = None
  try:
    paths = ensure_models_cached(cfg)
  except Exception as exc:
    with _state_lock:
      _provisioning_error = str(exc)
      _provisioning_started = False
    log.exception("speech_model_provisioning_failed")
    return
  with _state_lock:
    _ready_paths = paths
  log.info("speech_models_ready", cache_dir=str(paths.cache_dir))


def get_ready_model_paths() -> VoiceModelPaths:
  with _state_lock:
    paths = _ready_paths
    error = _provisioning_error
  if paths is not None:
    return paths
  if error:
    raise SpeechModelsNotReadyError(f"speech models are not ready: {error}")
  raise SpeechModelsNotReadyError("speech models are still downloading")


def ensure_models_cached(cfg: CharlieBotConfig) -> VoiceModelPaths:
  """Download missing model artifacts for the configured engine, verify, and publish readiness."""
  if cfg.voice.engine == "qwen3_hf":
    paths = voice_model_paths(cfg)
    paths.cache_dir.mkdir(parents=True, exist_ok=True)
    _ensure_artifact(paths.silero_vad, SILERO_VAD_URL, SILERO_VAD_SHA256)
    paths = replace(paths, hf_snapshot=ensure_qwen3_hf_snapshot(cfg))
    global _ready_paths
    with _state_lock:
      _ready_paths = paths
    return paths
  return _ensure_sherpa_paths_cached(cfg)


def ensure_qwen3_hf_snapshot(cfg: CharlieBotConfig) -> Path:
  """Return the local snapshot dir of cfg.voice.model_id, downloading it when missing.

  Uses huggingface_hub (ships with transformers in the gpu-voice group), so the import
  stays inside the qwen3_hf paths. Download failures raise into the provisioning error
  path, preserving the SpeechModelsNotReadyError behavior while weights are on their way.
  """
  from huggingface_hub import snapshot_download

  cache_dir = cfg.charliebot_home / "models"
  cache_dir.mkdir(parents=True, exist_ok=True)
  snapshot = Path(
      snapshot_download(
          repo_id=cfg.voice.model_id,
          cache_dir=str(cache_dir),
          etag_timeout=HTTP_MODEL_DOWNLOAD_TIMEOUT,
      ))
  _verify_hf_snapshot(snapshot)
  return snapshot


def _snapshot_complete(snapshot: Path) -> bool:
  """Whether a downloaded HF snapshot dir carries the weights and config to load."""
  return (snapshot / "config.json").is_file() and any(snapshot.glob("*.safetensors"))


def _verify_hf_snapshot(snapshot: Path) -> None:
  if not _snapshot_complete(snapshot):
    raise RuntimeError(f"qwen3_hf snapshot is incomplete (no config.json or safetensors): {snapshot}")


def _ensure_sherpa_paths_cached(cfg: CharlieBotConfig) -> VoiceModelPaths:
  """Download/verify the sherpa CPU artifacts and publish them as the ready paths.

  Also the fallback provisioning path when the GPU engine fails after a qwen3_hf
  provision: the CPU archive is only fetched here, on demand.
  """
  paths = voice_model_paths(cfg)
  paths.cache_dir.mkdir(parents=True, exist_ok=True)

  _ensure_artifact(paths.qwen3_archive, QWEN3_ASR_URL, QWEN3_ASR_SHA256)
  _ensure_artifact(paths.silero_vad, SILERO_VAD_URL, SILERO_VAD_SHA256)
  _ensure_qwen3_extracted(paths)
  _verify_model_files(paths)

  global _ready_paths
  with _state_lock:
    _ready_paths = paths
  return paths


def _open_vad(vad_config: object, buffer_seconds: float) -> object:
  """One VAD instance over the bundle's config; the seam tests stub to feed fake segments."""
  import sherpa_onnx

  return sherpa_onnx.VoiceActivityDetector(vad_config, buffer_size_in_seconds=buffer_seconds)


def get_transcription_bundle(cfg: CharlieBotConfig) -> _SpeechModelBundle:
  """The resident recognizer + VAD bundle for offline transcription calls.

  Raises SpeechModelsNotReadyError while provisioning is incomplete; the voice
  endpoints map that to 503 and decode_audio.py lets it fail loudly.
  """
  return _get_model_bundle(cfg, get_ready_model_paths())


def _offline_vad_segments(vad: object, samples: np.ndarray) -> list[tuple[int, int]]:
  """Feed the VAD the whole recording in OFFLINE_VAD_FEED_SAMPLES steps, flush, and
  return the closed speech segments as (start, end) sample offsets."""
  feed = samples.astype(np.float32) / 32768.0
  for pos in range(0, feed.size, OFFLINE_VAD_FEED_SAMPLES):
    vad.accept_waveform(feed[pos:pos + OFFLINE_VAD_FEED_SAMPLES])
  vad.flush()
  segments: list[tuple[int, int]] = []
  while not vad.empty():
    segment = vad.front
    start = int(segment.start)
    segments.append((start, start + len(segment.samples)))
    vad.pop()
  return segments


def offline_decode_windows(vad: object, samples: np.ndarray) -> list[tuple[int, int, int, int]]:
  """Segment a recording through the opened VAD and return each segment's decode window.

  One entry per closed speech segment: ``(start, end, left, right)`` sample offsets,
  where ``start``/``end`` are the detector's speech bounds and ``left``/``right`` the
  padded decode window (5 s of pause before the segment, 0.4 s tail after, the left
  edge clipped against the previous window's right edge so windows never overlap).
  The single owner of the window arithmetic: transcribe_pcm_offline and the replay
  evaluation decode byte-identical spans.
  """
  windows: list[tuple[int, int, int, int]] = []
  decoded_region_end = 0
  for start, end in _offline_vad_segments(vad, samples):
    left = max(decoded_region_end, start - SEGMENT_DECODE_PAUSE_SAMPLES)
    right = min(samples.size, end + SEGMENT_DECODE_PAD_SAMPLES)
    windows.append((start, end, left, right))
    decoded_region_end = right
  return windows


def transcribe_pcm_offline(bundle: _SpeechModelBundle, pcm_bytes: bytes) -> str:
  """Decode a complete 16 kHz mono PCM16 recording in one pass and return the text.

  The bundle's VAD segments the audio offline (production 128 ms feed steps), every
  segment decodes in one shot over the padded window the pipeline has always used
  (5 s of pause before the segment, 0.4 s tail after, the left edge clipped against
  the previous segment's right edge so windows never overlap), and the segment texts
  join in order. No state survives the call.
  """
  if len(pcm_bytes) % 2 != 0:
    raise ValueError("invalid PCM frame: byte length must be even")
  samples = np.frombuffer(pcm_bytes, dtype="<i2")
  # The buffer is sized to the whole recording, so the detector's internal buffer
  # cannot wrap no matter how long the input is.
  vad = _open_vad(bundle.vad_config, samples.size / SAMPLE_RATE + 10)
  texts: list[str] = []
  for _start, _end, left, right in offline_decode_windows(vad, samples):
    text = _decode_samples(bundle, samples[left:right].astype(np.float32) / 32768.0)
    if text:
      texts.append(text)
  return _join_segments(*texts)


def _ensure_artifact(path: Path, url: str, expected_sha256: str) -> None:
  if path.exists():
    actual = _sha256_file(path)
    if actual != expected_sha256:
      raise RuntimeError(f"cached speech model hash mismatch for {path}: {actual}")
    return

  def _write(stream: BinaryIO) -> None:
    with urllib.request.urlopen(url, timeout=HTTP_MODEL_DOWNLOAD_TIMEOUT) as response:
      status = getattr(response, "status", 200)
      if status != 200:
        raise RuntimeError(f"model download failed for {url}: HTTP {status}")
      digest = hashlib.sha256()
      while chunk := response.read(1024 * 1024):
        digest.update(chunk)
        stream.write(chunk)
    actual = digest.hexdigest()
    if actual != expected_sha256:
      raise RuntimeError(f"downloaded speech model hash mismatch for {url}: {actual}")

  # The swap discipline lives in json_utils.atomic_write_stream, so the verify
  # runs inside its write callback: a mismatch raises before the publish.
  atomic_write_stream(path, _write)


def _qwen3_model_files(paths: VoiceModelPaths) -> tuple[Path, ...]:
  return (
      paths.qwen3_conv_frontend,
      paths.qwen3_encoder,
      paths.qwen3_decoder,
      paths.qwen3_tokenizer / "merges.txt",
      paths.qwen3_tokenizer / "tokenizer_config.json",
      paths.qwen3_tokenizer / "vocab.json",
  )


def _ensure_qwen3_extracted(paths: VoiceModelPaths) -> None:
  if all(path.is_file() for path in _qwen3_model_files(paths)):
    return

  tmp_root = paths.cache_dir / f".{QWEN3_ASR_DIR_NAME}.{uuid.uuid4().hex}.extracting"
  try:
    tmp_root.mkdir()
    with tarfile.open(paths.qwen3_archive, "r:bz2") as tar:
      tar.extractall(tmp_root)
    extracted = tmp_root / QWEN3_ASR_DIR_NAME
    if not extracted.is_dir():
      raise RuntimeError(f"Qwen3-ASR archive did not contain {QWEN3_ASR_DIR_NAME}")
    os.replace(extracted, paths.qwen3_dir)
  finally:
    if tmp_root.exists():
      shutil.rmtree(tmp_root)


def _verify_model_files(paths: VoiceModelPaths) -> None:
  missing = [str(path) for path in (*_qwen3_model_files(paths), paths.silero_vad) if not path.is_file()]
  if missing:
    raise RuntimeError("speech model files are missing: " + ", ".join(missing))


def _sha256_file(path: Path) -> str:
  digest = hashlib.sha256()
  with path.open("rb") as f:
    for chunk in iter(lambda: f.read(1024 * 1024), b""):
      digest.update(chunk)
  return digest.hexdigest()


def _get_model_bundle(cfg: CharlieBotConfig, paths: VoiceModelPaths) -> _SpeechModelBundle:
  global _bundle, _bundle_engine
  with _state_lock:
    bundle = _bundle
    bundle_engine = _bundle_engine
  if bundle is not None and bundle_engine == cfg.voice.engine:
    return bundle

  # Double-checked single-flight: concurrent first accesses queue on the dedicated
  # build lock, and the in-lock re-check lets the losers return the winner's bundle
  # instead of building a second recognizer. The build never holds _state_lock.
  with _bundle_build_lock:
    with _state_lock:
      bundle = _bundle
      bundle_engine = _bundle_engine
    if bundle is not None and bundle_engine == cfg.voice.engine:
      return bundle

    if cfg.voice.engine == "qwen3_hf":
      try:
        bundle = create_qwen3_hf_bundle(cfg, paths)
      except Exception as exc:
        # GPU engine down (missing packages, no card, load error): keep voice alive on
        # the CPU engine. The ready log below names the actually active engine, and the
        # deployment preflight re-asserts the GPU path, so the downgrade is visible.
        log.warning("voice_gpu_engine_init_failed", error=str(exc), engine="qwen3_hf", fallback="sherpa")
        bundle = create_sherpa_bundle(_ensure_sherpa_paths_cached(cfg))
    else:
      bundle = create_sherpa_bundle(paths)

    log.info("voice_model_bundle_ready", engine=bundle.engine, model_id=bundle.model_id)
    with _state_lock:
      if _bundle is None or _bundle_engine != cfg.voice.engine:
        _bundle = bundle
        _bundle_engine = cfg.voice.engine
      return _bundle


def create_sherpa_bundle(paths: VoiceModelPaths, hotwords: str = "") -> _SpeechModelBundle:
  """Build the CPU sherpa-onnx bundle. Also the fallback decoder when the GPU engine fails.

  ``hotwords`` reaches the recognizer's biasing parameter verbatim; production callers
  pass nothing, so production decoding is unchanged.
  """
  import sherpa_onnx

  # Dense 20s Chinese segments decode to ~140 tokens, so the 128-token sherpa
  # default can truncate; 256 new tokens plus a 1024-position KV cache covers
  # the 20s max VAD segment with headroom.
  recognizer = sherpa_onnx.OfflineRecognizer.from_qwen3_asr(
      conv_frontend=str(paths.qwen3_conv_frontend),
      encoder=str(paths.qwen3_encoder),
      decoder=str(paths.qwen3_decoder),
      tokenizer=str(paths.qwen3_tokenizer),
      num_threads=4,
      sample_rate=SAMPLE_RATE,
      max_total_len=1024,
      max_new_tokens=256,
      hotwords=hotwords,
  )
  return _SpeechModelBundle(
      recognizer=recognizer,
      vad_config=_create_vad_config(paths),
      decode_lock=threading.Lock(),
      engine="sherpa",
      model_id=QWEN3_ASR_DIR_NAME,
  )


def create_qwen3_hf_bundle(cfg: CharlieBotConfig, paths: VoiceModelPaths) -> _SpeechModelBundle:
  """Build the GPU bundle: official transformers Qwen3-ASR weights on cuda in BF16.

  No fallback here — callers that must survive a GPU failure wrap this; the
  deployment preflight calls it directly so a broken GPU path fails setup.
  """
  import torch
  from transformers import AutoModelForMultimodalLM, AutoProcessor

  snapshot = paths.hf_snapshot if paths.hf_snapshot is not None else ensure_qwen3_hf_snapshot(cfg)
  processor = AutoProcessor.from_pretrained(snapshot)
  model = AutoModelForMultimodalLM.from_pretrained(snapshot, dtype=torch.bfloat16)
  model.to("cuda").eval()
  return _SpeechModelBundle(
      recognizer=_Qwen3HfRecognizer(processor, model),
      vad_config=_create_vad_config(paths),
      decode_lock=threading.Lock(),
      engine="qwen3_hf",
      model_id=cfg.voice.model_id,
  )


def _create_vad_config(paths: VoiceModelPaths) -> object:
  import sherpa_onnx

  vad_config = sherpa_onnx.VadModelConfig()
  vad_config.silero_vad.model = str(paths.silero_vad)
  vad_config.silero_vad.threshold = 0.5
  # A measured 0.93s mid-sentence pause split a phrase into a 1.35s fragment the
  # model misrecognized; only pauses of 1.0s or more end a segment.
  vad_config.silero_vad.min_silence_duration = 1.0
  vad_config.silero_vad.min_speech_duration = 0.25
  vad_config.silero_vad.max_speech_duration = 20.0
  vad_config.sample_rate = SAMPLE_RATE
  return vad_config


class _Qwen3HfStream:
  """Decode-request carrier mirroring the sherpa stream surface `_decode_samples` drives."""

  def __init__(self) -> None:
    self.samples: np.ndarray | None = None
    self.result = SimpleNamespace(text="")

  def accept_waveform(self, sample_rate: int, samples: np.ndarray) -> None:
    del sample_rate  # decode_stream feeds the processor, whose feature extractor samples at 16k
    self.samples = samples


class _Qwen3HfRecognizer:
  """Runs the transformers Qwen3-ASR generate loop behind the sherpa stream surface."""

  def __init__(self, processor: object, model: object) -> None:
    self._processor = processor
    self._model = model

  def create_stream(self) -> _Qwen3HfStream:
    return _Qwen3HfStream()

  def decode_stream(self, stream: _Qwen3HfStream) -> None:
    import torch

    samples = stream.samples if stream.samples is not None else np.empty(0, dtype=np.float32)
    # The Qwen3-ASR feature extractor defaults to 16k sampling (matching SAMPLE_RATE);
    # passing the rate explicitly — even via the documented audio_kwargs — trips a
    # transformers "kwargs must be in processor_kwargs" warning on every decode.
    inputs = self._processor.apply_transcription_request(audio=samples)
    inputs = inputs.to(self._model.device, self._model.dtype)
    with torch.inference_mode():
      output_ids = self._model.generate(**inputs, max_new_tokens=256, do_sample=False)
    generated_ids = output_ids[:, inputs["input_ids"].shape[1]:]
    stream.result.text = self._processor.decode(generated_ids, return_format="transcription_only")[0]


def _decode_samples(bundle: _SpeechModelBundle, samples: np.ndarray) -> str:
  if samples.size == 0:
    return ""
  contiguous = np.ascontiguousarray(samples, dtype=np.float32)
  with bundle.decode_lock:
    stream = bundle.recognizer.create_stream()
    stream.accept_waveform(SAMPLE_RATE, contiguous)
    bundle.recognizer.decode_stream(stream)
    text = stream.result.text
  return " ".join(text.strip().split())


def _join_segments(*parts: str) -> str:
  return " ".join(part for part in (p.strip() for p in parts) if part)


# The warm decode's shape: the cold cost it pays is kernel compilation and allocation
# keyed to the input's size and dtypes, not its content, so the parameters are
# hard-coded boot constants, not a tuning surface.
WARMUP_SECONDS = 0.5
WARMUP_FREQUENCY_HZ = 440.0


def warm_up_bundle(bundle: _SpeechModelBundle) -> None:
  """Decode one deterministic 0.5 s 440 Hz sine through the bundle and discard the text.

  Serves server._provision_speech_models: it moves the first-decode cold cost (CUDA
  kernel init + memory allocation, measured ~5 s) from the first real request to
  boot. The decoded text carries no signal — a same-shape sine pays the identical
  cold cost — so it is dropped; this is not a transcription correctness check.
  """
  positions = np.arange(int(SAMPLE_RATE * WARMUP_SECONDS), dtype=np.float64)
  samples = np.sin(2 * np.pi * WARMUP_FREQUENCY_HZ * positions / SAMPLE_RATE)
  _decode_samples(bundle, samples.astype(np.float32))
