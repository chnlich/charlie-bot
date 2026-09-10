"""Deployment step for the qwen3_hf GPU voice engine.

``scripts/setup.sh`` runs ``python -m src.core.voice_setup enable`` on hosts with
nvidia-smi, after ``uv sync --group gpu-voice``. The enable flow downloads the official
Qwen3-ASR weights when missing, preflight-asserts the four GPU conditions (imports,
cuda model load, measured decode timing, free VRAM report), and only then flips
``voice.engine: qwen3_hf`` in the deployment config — idempotently, so rerunning setup
neither re-downloads nor rewrites an already-enabled config. Any preflight failure
raises with the failing line and leaves the config untouched.
"""

from __future__ import annotations

import re
import sys
import time
import wave
from pathlib import Path

import numpy as np
import structlog

from src.core.config import CharlieBotConfig, load_config
from src.core.yaml_utils import load_yaml

log = structlog.get_logger()

# GPU decode budget for the preflight's ~10s real recording, measured on this host
# (x86_64 WSL2, RTX 3080 with the ~4.5GB desktop load active) during implementation:
# 10.1s of speech decoded warm in 1.17-1.57s across Qwen3-ASR-0.6B/1.7B in BF16
# (ab_voice_decode_stability_v1.py in the session artifacts). The threshold keeps
# ~1.6x headroom over the worst measured warm decode while still failing a VRAM-
# paging-style pathology (the one 19s outlier seen on a cold first pass). Note the
# measured magnitude is above the plan's hoped-for <1s: the host is WSL2 and the
# Windows desktop holds ~4.5GB of VRAM and ~17% GPU utilization.
PREFLIGHT_DECODE_THRESHOLD_SECONDS = 2.5

# The preflight decode check needs a real user recording near the calibration length:
# 5-15s keeps the threshold comparable, and 10s is the measurement point above.
PREFLIGHT_RECORDING_MIN_SECONDS = 5.0
PREFLIGHT_RECORDING_MAX_SECONDS = 15.0
PREFLIGHT_RECORDING_TARGET_SECONDS = 10.0


def write_voice_engine(home: Path, engine: str = "qwen3_hf") -> str:
  """Idempotently set ``voice.engine`` in ``<home>/config.yaml``; return the action taken.

  Reads the file textually so comments and formatting survive: the ``engine:`` line
  inside the ``voice:`` block is rewritten in place when present, inserted right
  after ``voice:`` when the block lacks one, and the whole block is appended when
  ``voice:`` is missing. Nothing is written when the effective value already matches.
  """
  config_path = home / "config.yaml"
  text = config_path.read_text(encoding="utf-8") if config_path.exists() else ""
  lines = text.splitlines(keepends=True)
  voice_matches = [i for i, line in enumerate(lines) if re.match(r"^voice:\s*$", line)]
  if len(voice_matches) > 1:
    raise ValueError(f"config.yaml defines voice on {len(voice_matches)} lines: {config_path}")
  # Inside the voice block: the blank, comment, and indented lines that follow it.
  engine_matches: list[int] = []
  if voice_matches:
    for j in range(voice_matches[0] + 1, len(lines)):
      line = lines[j]
      stripped = line.strip()
      if stripped and not stripped.startswith("#") and not line[0].isspace():
        break
      if re.match(r"^\s+engine:", line):
        engine_matches.append(j)
  if len(engine_matches) > 1:
    raise ValueError(f"config.yaml defines voice.engine on {len(engine_matches)} lines: {config_path}")
  # Duplicate key lines are broken yaml (safe_load last-wins); the checks above run
  # before the effective-value skip so the idempotent path never blesses them.
  data = load_yaml(config_path, default={})
  if not isinstance(data, dict):
    raise ValueError(f"config must be a top-level mapping: {config_path}")
  if data.get("voice", {}).get("engine") == engine:
    return "skipped"

  if voice_matches:
    if engine_matches:
      original = lines[engine_matches[0]]
      indent = original[:len(original) - len(original.lstrip())]
      ending = original[len(original.rstrip("\r\n")):]
      lines[engine_matches[0]] = f"{indent}engine: {engine}{ending}"
    else:
      if not lines[voice_matches[0]].endswith("\n"):
        lines[voice_matches[0]] += "\n"
      lines.insert(voice_matches[0] + 1, f"  engine: {engine}\n")
    config_path.write_text("".join(lines), encoding="utf-8")
    return "updated"

  if text and not text.endswith("\n"):
    text += "\n"
  config_path.parent.mkdir(parents=True, exist_ok=True)
  config_path.write_text(text + f"voice:\n  engine: {engine}\n", encoding="utf-8")
  return "appended"


def pick_preflight_recording(sessions_dir: Path) -> Path:
  """Pick the real voice recording closest to 10s for the preflight decode check."""
  candidates: list[tuple[float, Path]] = []
  for wav_path in sorted(sessions_dir.glob("*/voice/*.wav")):
    try:
      with wave.open(str(wav_path), "rb") as wav:
        if wav.getframerate() != 16_000 or wav.getnchannels() != 1 or wav.getsampwidth() != 2:
          continue
        duration = wav.getnframes() / wav.getframerate()
    except (wave.Error, OSError):
      continue  # unreadable/foreign wav files are filtered out of the pick, not fatal
    if PREFLIGHT_RECORDING_MIN_SECONDS <= duration <= PREFLIGHT_RECORDING_MAX_SECONDS:
      candidates.append((abs(duration - PREFLIGHT_RECORDING_TARGET_SECONDS), wav_path))
  if not candidates:
    raise RuntimeError(
        f"preflight (c) needs a real {PREFLIGHT_RECORDING_MIN_SECONDS:.0f}-"
        f"{PREFLIGHT_RECORDING_MAX_SECONDS:.0f}s voice recording under {sessions_dir}/*/voice/; none found")
  return min(candidates)[1]


def _load_wav_samples(path: Path) -> np.ndarray:
  with wave.open(str(path), "rb") as wav:
    if wav.getframerate() != 16_000 or wav.getnchannels() != 1 or wav.getsampwidth() != 2:
      raise ValueError(f"preflight recording must be 16kHz mono int16 wav: {path}")
    frames = wav.readframes(wav.getnframes())
  return np.frombuffer(frames, dtype="<i2").astype(np.float32) / 32768.0


def run_gpu_preflight(
    cfg: CharlieBotConfig,
    *,
    decode_threshold_seconds: float = PREFLIGHT_DECODE_THRESHOLD_SECONDS,
) -> dict:
  """Assert the four GPU-voice preflight conditions; raise on the first failure.

  (a) torch and transformers import (an ImportError is the failure); (b) the model
  loads onto cuda; (c) one real ~10s recording decodes on GPU under the measured
  threshold with non-empty text; (d) free VRAM headroom after load is reported.
  Returns the report dict printed by the setup log.
  """
  import torch
  import transformers

  import src.agents.transcriber as transcriber

  if not torch.cuda.is_available():
    raise RuntimeError("preflight (b) failed: torch imports but torch.cuda.is_available() is False")
  log.info(
      "preflight_imports_ok",
      check="(a)",
      torch=torch.__version__,
      transformers=transformers.__version__,
      device=torch.cuda.get_device_name(0),
  )

  snapshot = transcriber.ensure_qwen3_hf_snapshot(cfg)
  bundle = transcriber.create_qwen3_hf_bundle(cfg, transcriber.voice_model_paths(cfg))
  log.info("preflight_model_loaded", check="(b)", snapshot=str(snapshot), model_id=bundle.model_id)

  wav_path = pick_preflight_recording(cfg.sessions_dir)
  samples = _load_wav_samples(wav_path)
  transcriber._decode_samples(bundle, samples)  # cold pass: CUDA kernel + allocator warmup
  started = time.perf_counter()
  text = transcriber._decode_samples(bundle, samples)
  decode_seconds = time.perf_counter() - started
  audio_seconds = len(samples) / transcriber.SAMPLE_RATE
  if not text:
    raise RuntimeError(f"preflight (c) failed: GPU decode of {wav_path.name} returned empty text")
  if decode_seconds >= decode_threshold_seconds:
    raise RuntimeError(
        f"preflight (c) failed: {audio_seconds:.1f}s audio decoded in {decode_seconds:.3f}s, "
        f"threshold {decode_threshold_seconds:.3f}s")

  free_bytes, total_bytes = torch.cuda.mem_get_info()
  report = {
      "recording": str(wav_path),
      "audio_seconds": round(audio_seconds, 2),
      "decode_seconds": round(decode_seconds, 3),
      "threshold_seconds": decode_threshold_seconds,
      "free_vram_gib": round(free_bytes / 2**30, 2),
      "total_vram_gib": round(total_bytes / 2**30, 2),
  }
  log.info("preflight_passed", check="(c)(d)", **report)
  return report


def enable(cfg: CharlieBotConfig | None = None) -> dict:
  """Run the full GPU voice engine enable flow and flip the deployment config.

  Idempotent: cached weights are reused and an already-enabled config is skipped.
  Raises on any preflight failure with the config untouched.
  """
  if cfg is None:
    cfg = load_config()
  report = run_gpu_preflight(cfg)
  action = write_voice_engine(cfg.charliebot_home)
  report["config_write"] = action
  log.info("voice_engine_enabled", engine="qwen3_hf", model_id=cfg.voice.model_id, config_write=action)
  return report


def main() -> None:
  if len(sys.argv) != 2 or sys.argv[1] != "enable":
    raise SystemExit("usage: python -m src.core.voice_setup enable")
  report = enable()
  for key, value in report.items():
    print(f"  {key}: {value}")


if __name__ == "__main__":
  main()
