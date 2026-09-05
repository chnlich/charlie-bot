"""Local streaming-style speech transcription with a dual engine: sherpa (CPU) or qwen3_hf (GPU).

The default 'sherpa' engine runs the int8 sherpa-onnx Qwen3-ASR pipeline on CPU. The
'qwen3_hf' engine (cfg.voice_engine) runs the official transformers Qwen3-ASR weights on
NVIDIA GPUs; it needs the gpu-voice dependency group, which only GPU hosts install, so
every torch/transformers import here is lazy and branch-local.
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
# Partial refresh re-decodes the open VAD segment, and decode cost grows with the
# segment. Measured on this host (x86_64 WSL2, RTX 3080 box; measurements in
# ab_voice_decode_v1.py, session artifacts): the CPU sherpa engine decodes a 10.1s
# segment in ~3.1s and a 26.6s segment in ~7.9s (num_threads=4); the GPU qwen3_hf
# engine decodes the 26.6s segment in ~2.0-2.3s. At a 2s refresh the 20s-cap segment
# would pin the decode thread (duty > 100%), and even at 5s the duty peaks near half
# on the longest segments, so partials refresh every 5s of speech.
LIVE_DECODE_INTERVAL_SAMPLES = SAMPLE_RATE * 5
LIVE_DECODE_MIN_SAMPLES = SAMPLE_RATE // 2
SEGMENT_DECODE_PAD_SAMPLES = 6_400
VAD_BUFFER_SECONDS = MAX_RECORDING_SECONDS + 10

QWEN3_ASR_DIR_NAME = "sherpa-onnx-qwen3-asr-0.6B-int8-2026-03-25"
QWEN3_ASR_ARCHIVE_NAME = f"{QWEN3_ASR_DIR_NAME}.tar.bz2"
QWEN3_ASR_URL = (
    "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/"
    "sherpa-onnx-qwen3-asr-0.6B-int8-2026-03-25.tar.bz2")
QWEN3_ASR_SHA256 = "393f8a14e2f5fb96746aaab342997a40641001fbd5bf9592a080a8329178ee96"

SILERO_VAD_NAME = "silero_vad.onnx"
SILERO_VAD_URL = "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx"
SILERO_VAD_SHA256 = "9e2449e1087496d8d4caba907f23e0bd3f78d91fa552479bb9c23ac09cbb1fd6"


class SpeechModelsNotReady(RuntimeError):
  """Raised when a voice stream starts before model provisioning is complete."""


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
  # qwen3_hf engine only: local snapshot dir of cfg.voice_model_id under cache_dir,
  # published by ensure_models_cached after snapshot_download. None for the sherpa engine.
  hf_snapshot: Path | None = None


@dataclass
class TranscriptionUpdate:
  text: str | None
  cap_reached: bool = False


@dataclass
class _SpeechModelBundle:
  recognizer: object
  vad_config: object
  decode_lock: threading.Lock
  # The engine whose decoder actually produced `recognizer` — 'sherpa' after a GPU
  # fallback even when cfg.voice_engine is 'qwen3_hf' — and the model id behind it.
  engine: str
  model_id: str


_state_lock = threading.Lock()
_provisioning_started = False
_ready_paths: VoiceModelPaths | None = None
_provisioning_error: str | None = None
_bundle: _SpeechModelBundle | None = None
# The cfg.voice_engine the cached _bundle answers for; a bundle built as a fallback
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


def start_model_provisioning(cfg: CharlieBotConfig):
  """Start non-blocking speech model provisioning and verification."""
  import asyncio

  global _provisioning_started, _provisioning_error
  with _state_lock:
    if _provisioning_started:
      return None
    _provisioning_started = True
    _provisioning_error = None

  async def _run() -> None:
    global _ready_paths, _provisioning_error, _provisioning_started
    try:
      paths = await asyncio.to_thread(ensure_models_cached, cfg)
    except asyncio.CancelledError:
      with _state_lock:
        _provisioning_started = False
      raise
    except Exception as exc:
      with _state_lock:
        _provisioning_error = str(exc)
        _provisioning_started = False
      log.exception("speech_model_provisioning_failed")
      return
    with _state_lock:
      _ready_paths = paths
    log.info("speech_models_ready", cache_dir=str(paths.cache_dir))

  return asyncio.create_task(_run(), name="speech-model-provisioning")


def get_ready_model_paths() -> VoiceModelPaths:
  with _state_lock:
    paths = _ready_paths
    error = _provisioning_error
  if paths is not None:
    return paths
  if error:
    raise SpeechModelsNotReady(f"speech models are not ready: {error}")
  raise SpeechModelsNotReady("speech models are still downloading")


def ensure_models_cached(cfg: CharlieBotConfig) -> VoiceModelPaths:
  """Download missing model artifacts for the configured engine, verify, and publish readiness."""
  if cfg.voice_engine == "qwen3_hf":
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
  """Return the local snapshot dir of cfg.voice_model_id, downloading it when missing.

  Uses huggingface_hub (ships with transformers in the gpu-voice group), so the import
  stays inside the qwen3_hf paths. Download failures raise into the provisioning error
  path, preserving the SpeechModelsNotReady behavior while weights are on their way.
  """
  from huggingface_hub import snapshot_download

  cache_dir = cfg.charliebot_home / "models"
  cache_dir.mkdir(parents=True, exist_ok=True)
  snapshot = Path(
      snapshot_download(
          repo_id=cfg.voice_model_id,
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


def models_are_cached(cfg: CharlieBotConfig) -> bool:
  paths = voice_model_paths(cfg)
  if not paths.silero_vad.is_file():
    return False
  if cfg.voice_engine == "qwen3_hf":
    return _qwen3_hf_snapshot_cached(cfg, paths) is not None
  return all(path.is_file() for path in _qwen3_model_files(paths))


def _qwen3_hf_snapshot_cached(cfg: CharlieBotConfig, paths: VoiceModelPaths) -> Path | None:
  """The locally complete HF snapshot dir, or None when it is not (fully) downloaded."""
  from huggingface_hub import snapshot_download
  from huggingface_hub.errors import LocalEntryNotFoundError

  try:
    snapshot = Path(
        snapshot_download(repo_id=cfg.voice_model_id, cache_dir=str(paths.cache_dir), local_files_only=True))
  except LocalEntryNotFoundError:
    return None
  return snapshot if _snapshot_complete(snapshot) else None


def create_transcription_session(cfg: CharlieBotConfig) -> SimulatedStreamingTranscriptionSession:
  paths = get_ready_model_paths()
  bundle = _get_model_bundle(cfg, paths)
  return SimulatedStreamingTranscriptionSession(bundle)


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
  if bundle is not None and bundle_engine == cfg.voice_engine:
    return bundle

  if cfg.voice_engine == "qwen3_hf":
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
    if _bundle is None or _bundle_engine != cfg.voice_engine:
      _bundle = bundle
      _bundle_engine = cfg.voice_engine
    return _bundle


def create_sherpa_bundle(paths: VoiceModelPaths) -> _SpeechModelBundle:
  """Build the CPU sherpa-onnx bundle. Also the fallback decoder when the GPU engine fails."""
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
      model_id=cfg.voice_model_id,
  )


def _create_vad_config(paths: VoiceModelPaths) -> object:
  import sherpa_onnx

  vad_config = sherpa_onnx.VadModelConfig()
  vad_config.silero_vad.model = str(paths.silero_vad)
  vad_config.silero_vad.threshold = 0.5
  vad_config.silero_vad.min_silence_duration = 0.35
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


class SimulatedStreamingTranscriptionSession:
  """Buffers PCM, runs VAD continuously, and re-decodes the open segment."""

  def __init__(self, bundle: _SpeechModelBundle) -> None:
    import sherpa_onnx

    self._bundle = bundle
    self._vad = sherpa_onnx.VoiceActivityDetector(bundle.vad_config, buffer_size_in_seconds=VAD_BUFFER_SECONDS)
    self._pcm_chunks: list[bytes] = []
    self._frozen_segments: list[str] = []
    self._fed_samples = 0
    self._last_partial = ""
    self._last_live_text = ""
    self._last_live_decode_at = -LIVE_DECODE_INTERVAL_SAMPLES
    self._saw_speech = False
    self._decoded_region_end = 0
    self._finished = False

  @property
  def audio_bytes(self) -> bytes:
    return b"".join(self._pcm_chunks)

  def accept_pcm(self, pcm: bytes) -> TranscriptionUpdate:
    if self._finished:
      raise RuntimeError("voice transcription session already finished")
    if len(pcm) % 2 != 0:
      raise ValueError("invalid PCM frame: byte length must be even")
    if self._fed_samples >= MAX_RECORDING_SAMPLES:
      return TranscriptionUpdate(text=None, cap_reached=True)

    samples_to_accept = min(len(pcm) // 2, MAX_RECORDING_SAMPLES - self._fed_samples)
    accepted_bytes = pcm[:samples_to_accept * 2]
    if accepted_bytes:
      self._pcm_chunks.append(accepted_bytes)
      samples = np.frombuffer(accepted_bytes, dtype="<i2").astype(np.float32) / 32768.0
      self._vad.accept_waveform(samples)
      self._fed_samples += samples.size

    closed_changed = self._drain_closed_segments()
    if closed_changed:
      self._last_live_text = ""
    live_text = self._decode_live_segment_if_due()
    if live_text is not None:
      self._last_live_text = live_text
    partial = self._compose_partial(self._last_live_text)
    cap_reached = self._fed_samples >= MAX_RECORDING_SAMPLES
    if closed_changed or (partial and partial != self._last_partial):
      self._last_partial = partial
      return TranscriptionUpdate(text=partial, cap_reached=cap_reached)
    return TranscriptionUpdate(text=None, cap_reached=cap_reached)

  def finish(self) -> str:
    if self._finished:
      raise RuntimeError("voice transcription session already finished")
    self._finished = True
    self._vad.flush()
    self._drain_closed_segments()
    if not self._saw_speech:
      return ""
    return _join_segments(*self._frozen_segments)

  def _drain_closed_segments(self) -> bool:
    changed = False
    while not self._vad.empty():
      segment = self._vad.front
      segment_start = int(segment.start)
      segment_end = segment_start + len(segment.samples)
      samples, decoded_right = self._raw_samples_for_decode(segment_start, segment_end)
      self._saw_speech = True
      text = _decode_samples(self._bundle, samples)
      self._decoded_region_end = decoded_right
      if text:
        self._frozen_segments.append(text)
      self._vad.pop()
      changed = True
    return changed

  def _decode_live_segment_if_due(self) -> str | None:
    if not self._vad.is_speech_detected():
      return ""
    self._saw_speech = True
    segment = self._vad.current_segment
    segment_start = int(segment.start)
    if self._fed_samples - segment_start < LIVE_DECODE_MIN_SAMPLES:
      return None
    if self._fed_samples - self._last_live_decode_at < LIVE_DECODE_INTERVAL_SAMPLES:
      return None
    self._last_live_decode_at = self._fed_samples
    samples, _ = self._raw_samples_for_decode(segment_start, self._fed_samples)
    return _decode_samples(self._bundle, samples)

  def _raw_samples_for_decode(self, start_sample: int, end_sample: int) -> tuple[np.ndarray, int]:
    left = max(self._decoded_region_end, start_sample - SEGMENT_DECODE_PAD_SAMPLES)
    right = min(self._fed_samples, end_sample + SEGMENT_DECODE_PAD_SAMPLES)
    if right <= left:
      return np.empty(0, dtype=np.float32), left
    raw_samples = np.frombuffer(self.audio_bytes, dtype="<i2", count=right - left, offset=left * 2)
    return raw_samples.astype(np.float32) / 32768.0, right

  def _compose_partial(self, live_text: str) -> str:
    return _join_segments(*self._frozen_segments, live_text)
