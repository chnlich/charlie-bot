"""Generate the synthetic one-second, 16 kHz mono PCM16 silence fixtures."""

import pathlib
import wave

SAMPLE_RATE = 16000
SAMPLE_WIDTH_BYTES = 2
DURATION_SECONDS = 1


def main() -> None:
  directory = pathlib.Path(__file__).resolve().parent / "audio"
  directory.mkdir(parents=True, exist_ok=True)
  pcm = bytes(SAMPLE_RATE * SAMPLE_WIDTH_BYTES * DURATION_SECONDS)
  (directory / "silence.pcm").write_bytes(pcm)
  with wave.open(str(directory / "silence.wav"), "wb") as output:
    output.setnchannels(1)
    output.setsampwidth(SAMPLE_WIDTH_BYTES)
    output.setframerate(SAMPLE_RATE)
    output.writeframes(pcm)
  (directory / "not-wave.bin").write_bytes(b"synthetic invalid WAV body\n")


if __name__ == "__main__":
  main()
