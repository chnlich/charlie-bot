"""Decode one audio file to text with the charlie-bot voice stack.

Companion to skills/voice-notes/SKILL.md. Resamples any container PyAV reads
into the 16 kHz mono PCM the transcriber takes, streams it through the local
Qwen3-ASR engine, and prints the transcript to stdout. Models auto-provision
into <charliebot_home>/models with pinned sha256 and load from disk later.

Run from the repo root so uv picks the project environment (sherpa-onnx);
--with av adds the container decoder for the invocation:

    uv run --with av python3 skills/voice-notes/scripts/decode_audio.py voice.m4a

Raw 16 kHz mono s16 PCM files (*.pcm, *.s16) decode without the av import.
The script also locates the checkout from any cwd: it walks its own parents
and the cwd for src/agents/transcriber.py, then honors CHARLIEBOT_REPO.
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

PCM_SUFFIXES = {".pcm", ".s16"}
CHUNK_BYTES = 16_000 * 2  # 1.0 s of 16 kHz s16 mono


def repo_root() -> Path:
  """Locate the checkout: script parents and cwd first, CHARLIEBOT_REPO last."""
  for base in [*Path(__file__).resolve().parents, Path.cwd()]:
    if (base / "src/agents/transcriber.py").is_file():
      return base
  override = Path(os.environ.get("CHARLIEBOT_REPO", ""))
  if (override / "src/agents/transcriber.py").is_file():
    return override
  sys.exit("repo checkout not found: run from the repo root or set CHARLIEBOT_REPO")


def load_pcm(path: Path) -> bytes:
  """Return the file as 16 kHz mono s16 PCM; PyAV covers every container."""
  if path.suffix.lower() in PCM_SUFFIXES:
    return path.read_bytes()
  import av

  resampler = av.audio.resampler.AudioResampler(format="s16", layout="mono", rate=16000)
  with av.open(str(path)) as container:
    chunks = [
        out.to_ndarray().astype("<i2").tobytes()
        for frame in container.decode(audio=0)
        for out in resampler.resample(frame)
    ]
  return b"".join(chunks)


def main() -> int:
  if len(sys.argv) != 2:
    sys.exit(__doc__)
  root = repo_root()
  if str(root) not in sys.path:
    sys.path.insert(0, str(root))
  from src.agents.transcriber import create_transcription_session, ensure_models_cached
  from src.core.config import get_config

  cfg = get_config()
  ensure_models_cached(cfg)
  session = create_transcription_session(cfg)
  pcm = load_pcm(Path(sys.argv[1]))
  for pos in range(0, len(pcm), CHUNK_BYTES):
    session.accept_pcm(pcm[pos:pos + CHUNK_BYTES])
  print(session.finish())
  return 0


if __name__ == "__main__":
  sys.exit(main())
