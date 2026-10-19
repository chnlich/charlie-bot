"""Generate the synthetic non-silent voice fixture: a 1.25 s, 16 kHz mono PCM16 sine tone (not speech)."""

import math
import pathlib
import struct
import wave

SAMPLE_RATE = 16000
SAMPLE_WIDTH_BYTES = 2
DURATION_SECONDS = 1.25
FREQUENCY_HZ = 440.0
AMPLITUDE = 8000


def main() -> None:
  directory = pathlib.Path(__file__).resolve().parent / "audio"
  directory.mkdir(parents=True, exist_ok=True)
  count = round(SAMPLE_RATE * DURATION_SECONDS)
  pcm = b"".join(
      struct.pack("<h", round(AMPLITUDE * math.sin(2 * math.pi * FREQUENCY_HZ * index / SAMPLE_RATE)))
      for index in range(count))
  with wave.open(str(directory / "voice-tone.wav"), "wb") as output:
    output.setnchannels(1)
    output.setsampwidth(SAMPLE_WIDTH_BYTES)
    output.setframerate(SAMPLE_RATE)
    output.writeframes(pcm)


if __name__ == "__main__":
  main()
