"""Weighted text length: one counting rule shared across layers.

``weighted_length`` is the shared notation: a string's length in weighted
characters is ``len(text)`` plus one per double-counted character, so a
Chinese message and an English one carry the same information per weighted
character. Consumers: the inter-agent message limit
(src/infra/constants.py) and the plan-page goal budget (artifact_check).
"""

# CJK ideographs, CJK punctuation, and fullwidth forms count double, so the same
# information density spends the same budget in Chinese and English.
_CJK_RANGES = ((0x3000, 0x303F), (0x4E00, 0x9FFF), (0xFF00, 0xFFEF))


def weighted_length(text: str) -> int:
  """Weighted characters in *text*: ``len(text)`` plus one per character in
  U+3000-303F (CJK punctuation), U+4E00-9FFF (CJK ideographs), or
  U+FF00-FFEF (fullwidth forms)."""
  return len(text) + sum(1 for c in text if any(lo <= ord(c) <= hi for lo, hi in _CJK_RANGES))
