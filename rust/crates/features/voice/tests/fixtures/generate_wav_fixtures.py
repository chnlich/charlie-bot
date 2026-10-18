from pathlib import Path
import wave


FIXTURE_DIR = Path(__file__).parent
CASES = {
    "wav-empty.wav": bytes(),
    "wav-one-byte.wav": bytes((1,)),
    "wav-two-bytes.wav": bytes((1, 2)),
    "wav-three-bytes.wav": bytes((1, 2, 3)),
    "wav-five-bytes.wav": bytes((1, 2, 3, 4, 5)),
}


def main() -> None:
  for name, pcm in CASES.items():
    with wave.open(str(FIXTURE_DIR / name), "wb") as output:
      output.setnchannels(1)
      output.setsampwidth(2)
      output.setframerate(16_000)
      output.writeframes(pcm)


if __name__ == "__main__":
  main()
