"""Tests for the ``charliebot artifact wrap`` assembly verb and its pre-render driver.

The driver runs the checkout's src/core/prerender_math.js against the vendored
KaTeX build (one CDN fetch per pytest session); the byte-integrity gate
and the render-path assertion come from src/core/artifact_check.py.
"""

import re
import subprocess
from pathlib import Path

import pytest
from conftest import ROOT

from src.cli.artifact import main as artifact_main
from src.core.artifact_wrap import ensure_vendored_katex, wrap_fragment

_DRIVER = ROOT / "src" / "core" / "prerender_math.js"


@pytest.fixture(scope="session")
def vendored_katex(tmp_path_factory: pytest.TempPathFactory) -> Path:
  """One CDN fetch per session, at the vendor path the CLI's home resolution derives."""
  home = tmp_path_factory.mktemp("katex-home")
  return ensure_vendored_katex(home / "vendor" / "katex" / "katex.min.js")


@pytest.fixture
def cli_katex(monkeypatch: pytest.MonkeyPatch, vendored_katex: Path) -> Path:
  """Point the CLI verb's home resolution at the fetched vendor copy's home.

  The wrap verb resolves the home off the env (src.core.home), not the config —
  the M98 seam shape; the module-level name is the patch target.
  """
  monkeypatch.setattr("src.cli.artifact.charliebot_home_dir", lambda: vendored_katex.parents[2])
  return vendored_katex


def _write_fragment(tmp_path: Path, fragment: str | bytes, name: str = "fragment.html") -> Path:
  fragment_path = tmp_path / name
  if isinstance(fragment, bytes):
    fragment_path.write_bytes(fragment)
  else:
    fragment_path.write_text(fragment, encoding="utf-8")
  return fragment_path


def _wrap(tmp_path: Path, fragment: str | bytes, vendored_katex: Path) -> Path:
  output = tmp_path / "page.html"
  wrap_fragment(
      genre="explain",
      fragment=_write_fragment(tmp_path, fragment),
      output=output,
      math=True,
      vendor_path=vendored_katex,
  )
  return output


def _wrap_cli(fragment_path: Path, output: Path, genre: str, *flags: str) -> SystemExit:
  with pytest.raises(SystemExit) as exc_info:
    artifact_main(["wrap", str(fragment_path), "--genre", genre, "--output", str(output), *flags])
  return exc_info.value


# ---------------------------------------------------------------------------
# assembly + pre-render
# ---------------------------------------------------------------------------


@pytest.mark.integration
def test_wrap_takes_head_and_style_from_the_template_and_the_body_from_the_fragment(
    tmp_path: Path, vendored_katex: Path) -> None:
  output = _wrap(tmp_path, '<div class="wrap"><main><p>body text</p></main></div>', vendored_katex=vendored_katex)
  page = output.read_text(encoding="utf-8")
  assert "<title>CharlieBot Explain Template</title>" in page  # head verbatim from the template
  assert "body text" in page
  assert "Why the tint ramp still took 47 minutes" not in page  # template example body gone
  assert page.startswith("<!doctype html>")
  assert page.rstrip().endswith("</html>")
  assert page.count("<body>") == 1


def test_driver_runs_as_a_subprocess(tmp_path: Path, vendored_katex: Path) -> None:
  fragment = _write_fragment(tmp_path, "<p>$x^2$</p>", name="driver.html")
  proc = subprocess.run(["node", str(_DRIVER), str(fragment), str(vendored_katex)], capture_output=True, check=False)
  assert proc.returncode == 0, proc.stderr.decode("utf-8")
  assert 'class="katex"' in proc.stdout.decode("utf-8")


def test_cli_defaults_math_on_for_explain_and_off_for_other_genres(
    tmp_path: Path, capsys: pytest.CaptureFixture[str], cli_katex: Path) -> None:
  fragment = _write_fragment(tmp_path, r"<p>$$y = x$$</p>")
  explain_output = tmp_path / "explain.html"
  assert _wrap_cli(fragment, explain_output, "explain").code == 0
  assert 'class="katex-display"' in explain_output.read_text(encoding="utf-8")

  sitrep_output = tmp_path / "sitrep.html"
  assert _wrap_cli(fragment, sitrep_output, "sitrep").code == 0
  sitrep_page = sitrep_output.read_text(encoding="utf-8")
  assert 'class="katex"' not in sitrep_page
  assert "$$y = x$$" in sitrep_page


# ---------------------------------------------------------------------------
# byte self-check gate
# ---------------------------------------------------------------------------


def _damaged_fragment() -> bytes:
  r"""TAB standing in for the \t of \text, formfeed for the \f of \frac — the damaged-page shape."""
  prose = "<p>\u524d\u5411\u4e2d TABext{out_routed} \u5c5e\u6027\u3002</p>\n"
  display = "<p>$$\\x0crac{\\partial L}{\\partial TABext{weights}} = 0$$</p>\n"
  return (prose + display).replace("TAB", "\t").replace("\\x0c", "\x0c").encode("utf-8")


def test_wrap_byte_gate_aborts_before_write_and_names_offsets(
    tmp_path: Path, capsys: pytest.CaptureFixture[str], cli_katex: Path) -> None:
  fragment = _write_fragment(tmp_path, _damaged_fragment(), name="damaged.html")
  output = tmp_path / "damaged_page.html"
  assert _wrap_cli(fragment, output, "explain").code == 1
  err = capsys.readouterr().err
  assert "0x09 at offset" in err
  assert "0x0c at offset" in err
  assert "write aborted" in err
  assert not output.exists()  # no partial output behind the aborted write


def test_wrap_byte_gate_runs_on_the_assembled_bytes(
    tmp_path: Path, capsys: pytest.CaptureFixture[str], cli_katex: Path) -> None:
  r"""A fragment TAB reports at its assembled-page offset, past the template head."""
  fragment = _write_fragment(tmp_path, "<p>x\ty</p>")
  output = tmp_path / "page.html"
  assert _wrap_cli(fragment, output, "explain").code == 1
  offset = int(re.search(r"0x09 at offset (\d+)", capsys.readouterr().err).group(1))
  assert offset > 5000  # the template head alone is longer than any fragment prefix
