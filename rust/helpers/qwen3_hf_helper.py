"""Recognize audio with the qwen3_hf engine's model, for the Rust server's voice route.

The Rust server runs this with <repo>/.venv/bin/python, the repo root as its working directory and on sys.path, and
no arguments. The process keeps the model loaded for the server's life and speaks JSON lines:

  The first line, printed once the bundle is built, is one of
    {"status": "ready"}
    {"status": "init_error", "error": <the exception text>}

  Each request is one line on stdin, {"samples": <base64 of float32 little-endian samples at 16 kHz>}. Its reply is
  one line, {"status": "ok", "text": <the recognizer's text>} or {"status": "error", "error": <the exception text>}.

The recognizer call is all this process does. The Rust side windows the audio with the VAD, joins the segments and
normalizes the whitespace. Logs and tracebacks go to stderr.
"""

import base64
import json
import os
import sys
import traceback
from typing import TextIO


def main() -> None:
  # Model loading writes to stdout, which carries the replies. The replies go to a copy of the original stdout, and
  # stdout and file descriptor 1 both point at stderr, so native code that writes to fd 1 reaches the log.
  reply_stream = os.fdopen(os.dup(1), "w", encoding="utf-8")
  os.dup2(sys.stderr.fileno(), 1)
  sys.stdout = sys.stderr

  # deferred: these imports load torch, which must come after the redirect above
  import numpy as np

  from src.features.voice import transcriber
  from src.features.voice.transcription import base
  from src.infra import config

  try:
    cfg = config.get_config()
    bundle = transcriber.create_qwen3_hf_bundle(cfg, transcriber.voice_model_paths(cfg))
  except Exception as exc:
    traceback.print_exc()
    _reply(reply_stream, {"status": "init_error", "error": str(exc)})
    return
  _reply(reply_stream, {"status": "ready"})

  recognizer = bundle.recognizers[0]
  for line in sys.stdin:
    request = json.loads(line)
    try:
      samples = np.frombuffer(base64.b64decode(request["samples"]), dtype="<f4").astype(np.float32)
      stream = recognizer.create_stream()
      stream.accept_waveform(base.SAMPLE_RATE, samples)
      recognizer.decode_stream(stream)
      reply = {"status": "ok", "text": stream.result.text}
    except Exception as exc:
      traceback.print_exc()
      reply = {"status": "error", "error": str(exc)}
    _reply(reply_stream, reply)


def _reply(stream: TextIO, value: dict[str, str]) -> None:
  stream.write(json.dumps(value) + "\n")
  stream.flush()


if __name__ == "__main__":
  main()
