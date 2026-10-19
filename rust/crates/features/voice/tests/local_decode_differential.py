"""Differential: the Rust offline decode against Python's transcribe_pcm_offline, on the same model files.

Run it on host3 from the worktree root, after `cargo-slot.sh build -p charliebot-voice --example local_decode`:

  host3/run.sh uv run python rust/crates/features/voice/tests/local_decode_differential.py
  host3/run.sh uv run python rust/crates/features/voice/tests/local_decode_differential.py --plant-difference

The inputs are every WAV in the model archive's test_wavs/ directory and the one-second silence of
rust/parity/data/generate_silence.py. Python decodes each input with transcriber.transcribe_pcm_offline; the Rust
example decodes the same files through charliebot_voice::differential. The run compares each input's decode windows
and transcript, and exits 0 only when the differences equal the expected count.

A clean run expects zero differences. --plant-difference is the control: it decodes only the silence, which both sides
agree on, alters its Python transcript, and expects exactly one difference.

The model location defaults to ~/.charliebot/models, the default of --voice-models in rust/parity/README.md.
"""

import argparse
import dataclasses
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import types
import wave

import numpy as np

REPO_ROOT = pathlib.Path(__file__).resolve().parents[5]
for import_root in (REPO_ROOT, REPO_ROOT / "rust" / "parity" / "data"):
  if str(import_root) not in sys.path:
    sys.path.insert(0, str(import_root))

import generate_silence  # noqa: E402

from src.features.voice import transcriber  # noqa: E402
from src.features.voice.transcription import base  # noqa: E402

DEFAULT_VOICE_MODELS = pathlib.Path.home() / ".charliebot" / "models"
DEFAULT_BINARY = REPO_ROOT / "rust" / "target" / "debug" / "examples" / "local_decode"
PREVIEW_CHARS = 60


@dataclasses.dataclass(frozen=True)
class Decode:
  """One input's decode on one side: each window as (start, end, left, right) and the transcript."""

  windows: list[list[int]]
  text: str


def read_pcm(path: pathlib.Path) -> bytes:
  """The 16 kHz mono PCM16 frames of a WAV, converted here when the file is another rate or channel layout.

  The conversion is linear: channels are averaged, then samples are interpolated to the target rate. Both decoders
  receive these same bytes. The server refuses a non-16 kHz upload, so only this differential decodes such audio.
  """
  with wave.open(str(path), "rb") as wav:
    channels, width, rate = wav.getnchannels(), wav.getsampwidth(), wav.getframerate()
    if width != 2:
      raise ValueError(f"{path}: expected PCM16 samples, got {8 * width}-bit")
    samples = np.frombuffer(wav.readframes(wav.getnframes()), dtype="<i2")
  if channels > 1:
    samples = samples.reshape(-1, channels).mean(axis=1)
  if rate != base.SAMPLE_RATE:
    count = round(samples.size * base.SAMPLE_RATE / rate)
    positions = np.linspace(0, samples.size - 1, count)
    samples = np.interp(positions, np.arange(samples.size), samples)
  return np.round(samples).astype("<i2").tobytes()


def inputs(cache_dir: pathlib.Path) -> list[tuple[str, bytes]]:
  """Each input as (label, PCM bytes): the model's test WAVs, then the one-second silence."""
  wav_dir = cache_dir / transcriber.QWEN3_ASR_DIR_NAME / "test_wavs"
  wavs = sorted(wav_dir.glob("*.wav"))
  if not wavs:
    raise FileNotFoundError(f"no test WAVs under {wav_dir}")
  found = [(f"test_wavs/{path.name}", read_pcm(path)) for path in wavs]
  silence_bytes = generate_silence.SAMPLE_RATE * generate_silence.SAMPLE_WIDTH_BYTES * generate_silence.DURATION_SECONDS
  found.append(("silence", bytes(silence_bytes)))
  return found


def python_bundle(cache_dir: pathlib.Path) -> transcriber._SpeechModelBundle:
  """The Python sherpa bundle over the model files under cache_dir, as the server builds it."""
  cfg = types.SimpleNamespace(charliebot_home=cache_dir.parent)
  paths = transcriber.voice_model_paths(cfg)
  if paths.cache_dir != cache_dir:
    raise ValueError(f"--voice-models must name a directory called models, got {cache_dir}")
  return transcriber.create_sherpa_bundle(paths, "")


def python_decode(bundle: transcriber._SpeechModelBundle, pcm: bytes) -> Decode:
  """Python's windows and transcript for one input, from the same VAD and decode the server runs."""
  samples = np.frombuffer(pcm, dtype="<i2")
  vad = transcriber._open_vad(bundle.vad_config, samples.size / base.SAMPLE_RATE + 10)
  windows = transcriber.offline_decode_windows(vad, samples)
  return Decode(windows=[list(window) for window in windows], text=transcriber.transcribe_pcm_offline(bundle, pcm))


def rust_decode(binary: pathlib.Path, cache_dir: pathlib.Path,
                pcm_paths: list[pathlib.Path]) -> tuple[int, list[Decode]]:
  """The example's pid and one decode per PCM file, in argument order."""
  command = [str(binary), str(cache_dir), *(str(path) for path in pcm_paths)]
  process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
  stdout, stderr = process.communicate()
  if process.returncode != 0:
    raise RuntimeError(f"local_decode exited {process.returncode}: {stderr.strip()}")
  lines = stdout.splitlines()
  if len(lines) != len(pcm_paths):
    raise RuntimeError(f"local_decode printed {len(lines)} lines for {len(pcm_paths)} inputs")
  decodes = []
  for line, pcm_path in zip(lines, pcm_paths, strict=True):
    record = json.loads(line)
    if record["pcm"] != str(pcm_path):
      raise RuntimeError(f"local_decode answered {record['pcm']} where {pcm_path} was asked")
    decodes.append(Decode(windows=record["windows"], text=record["text"]))
  return process.pid, decodes


def differences(label: str, python: Decode, rust: Decode) -> list[str]:
  """Each field where the two sides disagree on one input."""
  found = []
  if python.windows != rust.windows:
    found.append(f"{label}: decode windows differ: python {python.windows} rust {rust.windows}")
  if python.text != rust.text:
    found.append(f"{label}: transcript differs: python {python.text!r} rust {rust.text!r}")
  return found


def main() -> None:
  parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
  parser.add_argument("--voice-models", type=pathlib.Path, default=DEFAULT_VOICE_MODELS)
  parser.add_argument("--binary", type=pathlib.Path, default=DEFAULT_BINARY)
  parser.add_argument(
      "--plant-difference",
      action="store_true",
      help="control run: decode only the silence and alter its Python transcript; expects exactly one difference")
  args = parser.parse_args()
  cache_dir = args.voice_models.expanduser().resolve()
  binary = args.binary.resolve()
  if not binary.is_file():
    raise FileNotFoundError(
        f"{binary} is missing; build it with cargo-slot.sh build -p charliebot-voice --example local_decode")

  cases = inputs(cache_dir)
  if args.plant_difference:
    cases = [case for case in cases if case[0] == "silence"]
  bundle = python_bundle(cache_dir)
  python_decodes = [python_decode(bundle, pcm) for _, pcm in cases]
  if args.plant_difference:
    python_decodes[0] = dataclasses.replace(python_decodes[0], text=python_decodes[0].text + " [planted]")

  with tempfile.TemporaryDirectory() as scratch:
    pcm_paths = []
    for index, (_, pcm) in enumerate(cases):
      pcm_path = pathlib.Path(scratch) / f"{index:02d}.pcm"
      pcm_path.write_bytes(pcm)
      pcm_paths.append(pcm_path)
    rust_pid, rust_decodes = rust_decode(binary, cache_dir, pcm_paths)

  found = []
  for (label, _), python, rust in zip(cases, python_decodes, rust_decodes, strict=True):
    case_differences = differences(label, python, rust)
    found.extend(case_differences)
    status = "DIFFERS" if case_differences else "match"
    preview = python.text[:PREVIEW_CHARS]
    print(f"{status} {label}: {len(python.windows)} windows, {len(python.text)} chars, python {preview!r}")
  for message in found:
    print(message)

  expected = 1 if args.plant_difference else 0
  print(
      f"differential: owner pids python {os.getpid()} rust {rust_pid}; inputs {len(cases)}, "
      f"differences {len(found)}, expected {expected}")
  sys.exit(0 if len(found) == expected else 1)


if __name__ == "__main__":
  main()
