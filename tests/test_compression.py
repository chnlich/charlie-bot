"""The one-shot level-1 deflator's boundary: valid gzip, level 1, byte-deterministic."""

import gzip

from src.core.compression import gzip_level1


def test_roundtrip_is_valid_gzip_level_1() -> None:
  data = ("{\"text\": \"内容\"}\n" * 2000).encode("utf-8")
  wire = gzip_level1(data)
  assert wire[:3] == b"\x1f\x8b\x08"
  assert gzip.decompress(wire) == data


def test_equal_input_produces_identical_bytes() -> None:
  data = b"payload " * 10000
  first = gzip_level1(data)
  assert gzip_level1(data) == first
  # The mtime header field stays zero — the property the gzip memos' repeat
  # serves-the-stored-bytes contracts build on across processes.
  assert first[4:8] == b"\x00\x00\x00\x00"


def test_empty_and_incompressible_input_roundtrip() -> None:
  assert gzip.decompress(gzip_level1(b"")) == b""
  blob = bytes(range(256)) * 4096
  assert gzip.decompress(gzip_level1(blob)) == blob
