"""Guard: the voice input UI serves English text only.

Every source file the voice feature hands to the browser — its static assets
and its templates — must be free of CJK characters. A phrase copied from a
Chinese plan page into a UI string would otherwise ship unseen; this test fails
on the file and line that holds it.
"""

import pathlib

import conftest

# The CJK blocks the voice UI must not contain: CJK punctuation and kana,
# CJK extension A, the unified ideographs, Hangul syllables, CJK compatibility
# ideographs, and the fullwidth forms.
CJK_RANGES = (
    ("\u3000", "\u30ff"),
    ("\u3400", "\u4dbf"),
    ("\u4e00", "\u9fff"),
    ("\uac00", "\ud7af"),
    ("\uf900", "\ufaff"),
    ("\uff00", "\uffef"),
)

VOICE_UI_DIRS = (
    pathlib.Path("src/features/voice/static"),
    pathlib.Path("src/features/voice/templates"),
)


def is_cjk(character: str) -> bool:
  """Whether ``character`` lies in one of the CJK ranges the voice UI forbids."""
  return any(low <= character <= high for low, high in CJK_RANGES)


def cjk_hits(root: pathlib.Path) -> list[str]:
  """One ``path:line: text`` per line of a voice UI source file that holds a CJK character."""
  hits = []
  for directory in VOICE_UI_DIRS:
    for path in sorted((root / directory).rglob("*")):
      if not path.is_file():
        continue
      rel = path.relative_to(root).as_posix()
      for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), start=1):
        if any(is_cjk(character) for character in line):
          hits.append(f"{rel}:{number}: {line}")
  return hits


def test_voice_ui_sources_hold_no_cjk() -> None:
  hits = cjk_hits(conftest.ROOT)
  assert not hits, "\n".join(hits)
