"""Transcribe one audio file to text through the charlie-bot online voice backend.

Companion to skills/voice-notes/SKILL.md. Resamples any container PyAV reads
into the 16 kHz mono PCM the recording transcription takes, sends the whole clip
through the same online call the voice upload uses (the default backend,
voice.default_backend), and prints the transcript to stdout. The backend's
credential comes from the profile's credentials.yaml, so no model is downloaded
or loaded on the box.

Run from the repo root so uv picks the project environment (httpx, websockets);
--with av adds the container decoder for the invocation:

    uv run --with av python3 skills/voice-notes/scripts/decode_audio.py voice.m4a

Raw 16 kHz mono s16 PCM files (*.pcm, *.s16) decode without the av import.
The script also locates the checkout from any cwd: it walks its own parents
and the cwd for src/features/voice/transcription/recording.py, then honors CHARLIEBOT_REPO.
"""

from __future__ import annotations

import asyncio
import os
import sys
from pathlib import Path

PCM_SUFFIXES = {".pcm", ".s16"}


def repo_root() -> Path:
  """Locate the checkout: script parents and cwd first, CHARLIEBOT_REPO last."""
  for base in [*Path(__file__).resolve().parents, Path.cwd()]:
    if (base / "src/features/voice/transcription/recording.py").is_file():
      return base
  override = Path(os.environ.get("CHARLIEBOT_REPO", ""))
  if (override / "src/features/voice/transcription/recording.py").is_file():
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
  from src.app.registrations import register_all
  from src.features.voice.transcription import recording
  from src.infra.config import get_config

  # The packages register their config sections and backend option models before the first parse.
  register_all()
  cfg = get_config()
  pcm = load_pcm(Path(sys.argv[1]))
  transcript = asyncio.run(recording.transcribe_recording(pcm, None, cfg))
  print(transcript.text)
  return 0


if __name__ == "__main__":
  sys.exit(main())
