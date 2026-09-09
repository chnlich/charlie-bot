"""builtins.open stub counting the bytes binary reads actually pull.

The tail-window readers promise to touch only the newest window(s) of a file;
a test proves that promise by installing this stub and asserting on the
recorded byte total.
"""

from typing import IO, Any

import pytest


def install_byte_counting_open(monkeypatch: pytest.MonkeyPatch) -> list[int]:
  """Patch ``builtins.open`` to record the length of every ``rb`` read.

  Returns the per-read byte counts in read order; text-mode opens pass
  through untouched.
  """
  reads: list[int] = []
  real_open = open

  def counting_open(file: Any, mode: str = "r", *args: Any, **kwargs: Any) -> IO[Any]:
    real = real_open(file, mode, *args, **kwargs)
    if mode != "rb":
      return real

    class CountingReader:

      def __getattr__(self, name: str) -> Any:
        return getattr(real, name)

      def __enter__(self) -> Any:
        return self

      def __exit__(self, *exc: Any) -> Any:
        return real.__exit__(*exc)

      def read(self, size: int = -1) -> bytes:
        data = real.read(size)
        reads.append(len(data))
        return data

    return CountingReader()

  monkeypatch.setattr("builtins.open", counting_open)
  return reads
