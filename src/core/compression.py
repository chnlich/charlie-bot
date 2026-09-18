"""The request path's one-shot level-1 gzip deflator.

Callers rely on: the result is a valid gzip container (``gzip.decompress``
round-trips it), level 1, and byte-identical for equal input — the header's
mtime field is written as 0, the property every gzip memo's repeat-serves-
the-stored-bytes contract builds on. The deflate is ISA-L's, which runs the
same level-1 pass faster than zlib's at an equal or smaller wire ratio.
"""

from isal import isal_zlib


def gzip_level1(data: bytes) -> bytes:
  """The level-1 gzip form of *data*; deterministic for equal input."""
  compressor = isal_zlib.compressobj(1, isal_zlib.DEFLATED, 31)
  return compressor.compress(data) + compressor.flush()
