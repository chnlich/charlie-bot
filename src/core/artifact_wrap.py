"""Assemble a genre page from a content fragment — the ``charliebot artifact wrap`` verb.

The writer supplies the body content (everything that goes inside <body>); the
genre template supplies head and style verbatim, plus a working math render path
in its head. Math in the fragment is pre-rendered to KaTeX markup at assembly
time (explain genre by default), so the shipped page carries class="katex"
markup and never depends on view-time scripts. The assembled bytes pass the same
byte-integrity rule the artifact_check assertion runs before anything is written,
so a fragment with mangled control bytes aborts the write instead of shipping.
"""

import subprocess
from pathlib import Path

import requests

from src.core import artifact_check

_REPO_ROOT = Path(__file__).resolve().parents[2]
_PRERENDER_DRIVER = _REPO_ROOT / "scripts" / "prerender_math.js"

KATEX_VERSION = "0.16.21"
KATEX_CDN_URL = f"https://cdn.jsdelivr.net/npm/katex@{KATEX_VERSION}/dist/katex.min.js"


def vendor_katex_path(charliebot_home: Path) -> Path:
  """The host's vendored KaTeX copy the wrap pre-render loads (UMD build), under the profile home."""
  return charliebot_home / "vendor" / "katex" / "katex.min.js"


def ensure_vendored_katex(vendor_path: Path) -> Path:
  """Return *vendor_path*, fetching it once from the allowlisted CDN when absent."""
  if vendor_path.is_file():
    return vendor_path
  vendor_path.parent.mkdir(parents=True, exist_ok=True)
  try:
    response = requests.get(KATEX_CDN_URL, timeout=60)
    response.raise_for_status()
  except requests.RequestException as e:
    raise RuntimeError(
        f"vendored KaTeX missing at {vendor_path} and the CDN fetch failed ({e}); place it manually:\n"
        f"  mkdir -p {vendor_path.parent} && curl -fsSL {KATEX_CDN_URL} -o {vendor_path}") from e
  vendor_path.write_bytes(response.content)
  return vendor_path


def _prerender_math(fragment: Path, vendor_path: Path) -> str:
  """Run the node pre-render driver over the fragment file; return the transformed HTML.

  The driver reads the file itself, so the fragment's bytes cross the process
  boundary untouched (a fragment holding control bytes keeps them until the
  byte self-check sees them)."""
  vendor_path = ensure_vendored_katex(vendor_path)
  proc = subprocess.run(
      ["node", str(_PRERENDER_DRIVER), str(fragment), str(vendor_path)],
      capture_output=True,
      check=False,
  )
  if proc.returncode != 0:
    raise RuntimeError(
        f"math pre-render driver failed (exit {proc.returncode}): {proc.stderr.decode('utf-8', errors='replace').strip()}"
    )
  return proc.stdout.decode("utf-8")


def _splice(template: str, fragment: str) -> str:
  """The page = template through its <body> open tag + the fragment + the template's tail."""
  body_open = template.index("<body")
  body_open_end = body_open + template[body_open:].index(">") + 1
  tail_start = template.index("</body>")
  return f"{template[:body_open_end]}\n{fragment.rstrip(chr(10))}\n{template[tail_start:]}"


def wrap_fragment(genre: str, fragment: Path, output: Path, math: bool, vendor_path: Path) -> Path:
  """Assemble the *genre* page from the *fragment* body content and write it to *output*.

  Five steps: read the genre template (head/style shell) -> pre-render math in
  the fragment (when *math*) -> splice the fragment into the template -> run the
  byte-integrity rule on the assembled bytes -> write. The self-check aborts
  before any write and names the offending byte offsets."""
  template_rel = f"prompts/{artifact_check._GENRE_TEMPLATES[genre]}"
  template = (_REPO_ROOT / template_rel).read_text(encoding="utf-8")
  fragment_text = fragment.read_bytes().decode("utf-8")  # strict: a non-UTF-8 fragment fails loudly here
  body = _prerender_math(fragment, vendor_path) if math else fragment_text
  assembled = _splice(template, body).encode("utf-8")
  bad = artifact_check.non_lf_control_bytes(assembled)
  if bad:
    raise ValueError(
        f"assembled {genre} page has {len(bad)} non-LF control bytes; write aborted: "
        f"{artifact_check.named_control_bytes(bad)}")
  output.parent.mkdir(parents=True, exist_ok=True)
  output.write_bytes(assembled)
  return output
