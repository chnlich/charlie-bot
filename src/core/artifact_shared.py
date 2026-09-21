"""The check/wrap shared artifact-page slice: the genre template map and the byte-integrity rule.

stdlib-only by contract: the artifact CLI's wrap verb (docs/perf_baseline.md M102) imports
this module on its timed wall, so nothing here may pull the assertion stack
(src.core.artifact_check — dataclasses→inspect, plan_diff, html). The byte rule is one
function pair so the checker's gate and the wrap self-check judge the identical rule on
identical input.
"""

GENRE_TEMPLATES = {
    "plan": "plan_template.html",
    "understanding": "plan_template.html",
    "sitrep": "sitrep_template.html",
    "debug": "debug_template.html",
    "explain": "explain_template.html",
}


def non_lf_control_bytes(data: bytes) -> list[tuple[int, int]]:
  """Every control byte other than LF (0x0A) in *data*, as (offset, byte value) pairs.

  Single source for the byte-integrity gate: the assertion runner feeds it the
  artifact's raw file bytes (re-read from disk, not the parsed DOM — the DOM
  layer drops comment bytes and normalizes whitespace, which hides mangled
  bytes), and the ``artifact wrap`` self-check feeds it the assembled page
  bytes, so both judge the identical rule on identical input. The damaged
  pages hold 8 and 12 such bytes (TAB from a decoded \t, formfeed
  from a decoded \f); the clean pages and the five genre templates hold zero.
  """
  return [(offset, value) for offset, value in enumerate(data) if value < 0x20 and value != 0x0A]


def named_control_bytes(bad: list[tuple[int, int]]) -> str:
  """The byte-integrity failure location string, shared by the gate and the wrap self-check."""
  return ", ".join(f"0x{value:02x} at offset {offset}" for offset, value in bad)
