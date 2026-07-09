"""Local streaming-style speech transcription using sherpa-onnx."""

from __future__ import annotations

import hashlib
import os
import shutil
import tarfile
import threading
import urllib.request
import uuid
from dataclasses import dataclass
from pathlib import Path

import numpy as np
import structlog

from src.core.config import CharlieBotConfig

log = structlog.get_logger()

SAMPLE_RATE = 16_000
MAX_RECORDING_SECONDS = 5 * 60
MAX_RECORDING_SAMPLES = SAMPLE_RATE * MAX_RECORDING_SECONDS
LIVE_DECODE_INTERVAL_SAMPLES = SAMPLE_RATE
LIVE_DECODE_MIN_SAMPLES = SAMPLE_RATE // 2
VAD_BUFFER_SECONDS = MAX_RECORDING_SECONDS + 10

SENSEVOICE_DIR_NAME = "sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2025-09-09"
SENSEVOICE_ARCHIVE_NAME = f"{SENSEVOICE_DIR_NAME}.tar.bz2"
SENSEVOICE_URL = (
    "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/"
    "sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2025-09-09.tar.bz2"
)
SENSEVOICE_SHA256 = "7305f7905bfcf77fa0b39388a313f3da35c68d971661a65475b56fb2162c8e63"

SILERO_VAD_NAME = "silero_vad.onnx"
SILERO_VAD_URL = "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx"
SILERO_VAD_SHA256 = "9e2449e1087496d8d4caba907f23e0bd3f78d91fa552479bb9c23ac09cbb1fd6"


class SpeechModelsNotReady(RuntimeError):
  """Raised when a voice stream starts before model provisioning is complete."""


@dataclass(frozen=True)
class VoiceModelPaths:
  cache_dir: Path
  sensevoice_archive: Path
  sensevoice_dir: Path
  sensevoice_model: Path
  sensevoice_tokens: Path
  silero_vad: Path


@dataclass
class TranscriptionUpdate:
  text: str | None
  cap_reached: bool = False


@dataclass
class _SpeechModelBundle:
  recognizer: object
  vad_config: object
  decode_lock: threading.Lock


_state_lock = threading.Lock()
_provisioning_started = False
_ready_paths: VoiceModelPaths | None = None
_provisioning_error: str | None = None
_bundle: _SpeechModelBundle | None = None


def voice_model_paths(cfg: CharlieBotConfig) -> VoiceModelPaths:
  cache_dir = cfg.charliebot_home / "models"
  sensevoice_dir = cache_dir / SENSEVOICE_DIR_NAME
  return VoiceModelPaths(
      cache_dir=cache_dir,
      sensevoice_archive=cache_dir / SENSEVOICE_ARCHIVE_NAME,
      sensevoice_dir=sensevoice_dir,
      sensevoice_model=sensevoice_dir / "model.int8.onnx",
      sensevoice_tokens=sensevoice_dir / "tokens.txt",
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
    global _ready_paths, _provisioning_error
    try:
      paths = await asyncio.to_thread(ensure_models_cached, cfg)
    except Exception as exc:
      with _state_lock:
        _provisioning_error = str(exc)
      log.exception("speech_model_provisioning_failed")
      return
    with _state_lock:
      _ready_paths = paths
    log.info("speech_models_ready", cache_dir=str(paths.cache_dir))

  return asyncio.create_task(_run(), name="speech-model-provisioning")


def get_ready_model_paths(cfg: CharlieBotConfig) -> VoiceModelPaths:
  with _state_lock:
    paths = _ready_paths
    error = _provisioning_error
  if paths is not None:
    return paths
  if error:
    raise SpeechModelsNotReady(f"speech models are not ready: {error}")
  raise SpeechModelsNotReady("speech models are still downloading")


def ensure_models_cached(cfg: CharlieBotConfig) -> VoiceModelPaths:
  """Download missing model artifacts, verify hashes, and extract SenseVoice."""
  paths = voice_model_paths(cfg)
  paths.cache_dir.mkdir(parents=True, exist_ok=True)

  _ensure_artifact(paths.sensevoice_archive, SENSEVOICE_URL, SENSEVOICE_SHA256)
  _ensure_artifact(paths.silero_vad, SILERO_VAD_URL, SILERO_VAD_SHA256)
  _ensure_sensevoice_extracted(paths)
  _verify_model_files(paths)

  global _ready_paths
  with _state_lock:
    _ready_paths = paths
  return paths


def models_are_cached(cfg: CharlieBotConfig) -> bool:
  paths = voice_model_paths(cfg)
  return paths.sensevoice_model.is_file() and paths.sensevoice_tokens.is_file() and paths.silero_vad.is_file()


def create_transcription_session(cfg: CharlieBotConfig) -> "SimulatedStreamingTranscriptionSession":
  paths = get_ready_model_paths(cfg)
  bundle = _get_model_bundle(paths)
  return SimulatedStreamingTranscriptionSession(bundle)


def _ensure_artifact(path: Path, url: str, expected_sha256: str) -> None:
  if path.exists():
    actual = _sha256_file(path)
    if actual != expected_sha256:
      raise RuntimeError(f"cached speech model hash mismatch for {path}: {actual}")
    return

  tmp_path = path.with_name(f".{path.name}.{uuid.uuid4().hex}.tmp")
  try:
    with urllib.request.urlopen(url, timeout=60) as response:
      status = getattr(response, "status", 200)
      if status != 200:
        raise RuntimeError(f"model download failed for {url}: HTTP {status}")
      with tmp_path.open("wb") as out:
        shutil.copyfileobj(response, out, length=1024 * 1024)
    actual = _sha256_file(tmp_path)
    if actual != expected_sha256:
      raise RuntimeError(f"downloaded speech model hash mismatch for {url}: {actual}")
    os.replace(tmp_path, path)
  finally:
    if tmp_path.exists():
      tmp_path.unlink()


def _ensure_sensevoice_extracted(paths: VoiceModelPaths) -> None:
  if paths.sensevoice_model.is_file() and paths.sensevoice_tokens.is_file():
    return

  tmp_root = paths.cache_dir / f".{SENSEVOICE_DIR_NAME}.{uuid.uuid4().hex}.extracting"
  try:
    tmp_root.mkdir()
    with tarfile.open(paths.sensevoice_archive, "r:bz2") as tar:
      tar.extractall(tmp_root)
    extracted = tmp_root / SENSEVOICE_DIR_NAME
    if not extracted.is_dir():
      raise RuntimeError(f"SenseVoice archive did not contain {SENSEVOICE_DIR_NAME}")
    os.replace(extracted, paths.sensevoice_dir)
  finally:
    if tmp_root.exists():
      shutil.rmtree(tmp_root)


def _verify_model_files(paths: VoiceModelPaths) -> None:
  missing = [
      str(path)
      for path in (paths.sensevoice_model, paths.sensevoice_tokens, paths.silero_vad)
      if not path.is_file()
  ]
  if missing:
    raise RuntimeError("speech model files are missing: " + ", ".join(missing))


def _sha256_file(path: Path) -> str:
  digest = hashlib.sha256()
  with path.open("rb") as f:
    for chunk in iter(lambda: f.read(1024 * 1024), b""):
      digest.update(chunk)
  return digest.hexdigest()


def _get_model_bundle(paths: VoiceModelPaths) -> _SpeechModelBundle:
  global _bundle
  with _state_lock:
    bundle = _bundle
  if bundle is not None:
    return bundle

  import sherpa_onnx

  recognizer = sherpa_onnx.OfflineRecognizer.from_sense_voice(
      model=str(paths.sensevoice_model),
      tokens=str(paths.sensevoice_tokens),
      num_threads=4,
      sample_rate=SAMPLE_RATE,
      language="",
      use_itn=True,
  )
  vad_config = sherpa_onnx.VadModelConfig()
  vad_config.silero_vad.model = str(paths.silero_vad)
  vad_config.silero_vad.threshold = 0.5
  vad_config.silero_vad.min_silence_duration = 0.35
  vad_config.silero_vad.min_speech_duration = 0.25
  vad_config.silero_vad.max_speech_duration = 20.0
  vad_config.sample_rate = SAMPLE_RATE

  bundle = _SpeechModelBundle(recognizer=recognizer, vad_config=vad_config, decode_lock=threading.Lock())
  with _state_lock:
    if _bundle is None:
      _bundle = bundle
    return _bundle


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
    self._last_live_decode_at = -LIVE_DECODE_INTERVAL_SAMPLES
    self._saw_speech = False
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
    live_text = self._decode_live_segment_if_due()
    partial = self._compose_partial(live_text)
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
      samples = np.asarray(segment.samples, dtype=np.float32)
      self._saw_speech = True
      text = _decode_samples(self._bundle, samples)
      if text:
        self._frozen_segments.append(text)
      self._vad.pop()
      changed = True
    return changed

  def _decode_live_segment_if_due(self) -> str:
    if not self._vad.is_speech_detected():
      return ""
    self._saw_speech = True
    segment = self._vad.current_segment
    samples = np.asarray(segment.samples, dtype=np.float32)
    if samples.size < LIVE_DECODE_MIN_SAMPLES:
      return ""
    if self._fed_samples - self._last_live_decode_at < LIVE_DECODE_INTERVAL_SAMPLES:
      return ""
    self._last_live_decode_at = self._fed_samples
    return _decode_samples(self._bundle, samples)

  def _compose_partial(self, live_text: str) -> str:
    return _join_segments(*self._frozen_segments, live_text)
