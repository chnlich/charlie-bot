"""Tests for the ``charliebot artifact wrap`` assembly verb and its pre-render driver.

The driver runs the checkout's scripts/prerender_math.js against the vendored
KaTeX 0.16.21 build (one CDN fetch per pytest session); the byte-integrity gate
and the render-path assertion come from src/core/artifact_check.py.
"""

import re
import subprocess
from pathlib import Path
from types import SimpleNamespace

import pytest
import requests

from src.cli.artifact import main as artifact_main
from src.core import artifact_check
from src.core.artifact_wrap import ensure_vendored_katex, wrap_fragment

_DRIVER = Path(__file__).resolve().parents[2] / "scripts" / "prerender_math.js"

# The session failure formulas plus the bracket classes: the same sources the
# chat extension test pins, run here through the wrap driver so the two scanner
# copies stay behavior-identical. display marks the two display classes.
_MATH_SOURCES = [
    ("inline escape-eat", r"$\text{Output} = \text{out\_routed} + \text{out\_shared} + x$",
     r"\text{Output} = \text{out\_routed} + \text{out\_shared} + x", False),
    ("display em-inject", r"$$\text{logits}_{\text{token}} = x \cdot W_{\text{token}}^T \in [N, 32]$$",
     r"\text{logits}_{\text{token}} = x \cdot W_{\text{token}}^T \in [N, 32]", True),
    ("inline bracket", r"\(x^2\)", "x^2", False),
    ("display bracket", r"\[\text{logits} \in [N, 32]\]", r"\text{logits} \in [N, 32]", True),
]
_LITERAL_SOURCES = ["costs $5 and $10 today", "price $5 later", "between $5 and$10 total"]


@pytest.fixture(scope="session")
def vendored_katex(tmp_path_factory: pytest.TempPathFactory) -> Path:
  """One CDN fetch per session; the wrap runs point at the fetched copy."""
  return ensure_vendored_katex(tmp_path_factory.mktemp("katex-vendor") / "katex.min.js")


@pytest.fixture
def cli_katex(monkeypatch: pytest.MonkeyPatch, vendored_katex: Path) -> Path:
  """Point the CLI verb's config home at a dir whose vendor copy is the session-fetched one."""
  monkeypatch.setattr(
      "src.cli.artifact.get_config",
      lambda: SimpleNamespace(charliebot_home=vendored_katex.parent.parent))
  return vendored_katex


def _write_fragment(tmp_path: Path, fragment: str | bytes, name: str = "fragment.html") -> Path:
  fragment_path = tmp_path / name
  if isinstance(fragment, bytes):
    fragment_path.write_bytes(fragment)
  else:
    fragment_path.write_text(fragment, encoding="utf-8")
  return fragment_path


def _wrap(tmp_path: Path, fragment: str | bytes, genre: str = "explain", math: bool = True,
          vendored_katex: Path | None = None, name: str = "fragment.html") -> Path:
  output = tmp_path / "page.html"
  wrap_fragment(
      genre=genre,
      fragment=_write_fragment(tmp_path, fragment, name),
      output=output,
      math=math,
      vendor_path=vendored_katex,
  )
  return output


def _wrap_cli(tmp_path: Path, fragment_path: Path, output: Path, genre: str, *flags: str):
  with pytest.raises(SystemExit) as exc_info:
    artifact_main(["wrap", str(fragment_path), "--genre", genre, "--output", str(output), *flags])
  return exc_info.value


# ---------------------------------------------------------------------------
# vendored KaTeX
# ---------------------------------------------------------------------------


def test_ensure_vendored_katex_fetches_the_missing_file(tmp_path: Path) -> None:
  vendor = tmp_path / "vendor" / "katex.min.js"
  assert not vendor.exists()
  ensure_vendored_katex(vendor)
  assert vendor.is_file() and vendor.stat().st_size > 100_000


def test_ensure_vendored_katex_fails_loud_naming_the_manual_command(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  vendor = tmp_path / "vendor" / "katex.min.js"

  def refuse(url: str, timeout: int) -> None:
    raise requests.ConnectionError(f"no route to {url}")

  monkeypatch.setattr("src.core.artifact_wrap.requests.get", refuse)
  with pytest.raises(RuntimeError, match=r"curl -fsSL"):
    ensure_vendored_katex(vendor)
  assert not vendor.exists()


# ---------------------------------------------------------------------------
# assembly + pre-render
# ---------------------------------------------------------------------------


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


@pytest.mark.parametrize("label,source,content,display", _MATH_SOURCES)
def test_wrap_prerenders_each_delimiter_class_to_katex_markup(
    tmp_path: Path, vendored_katex: Path, label: str, source: str, content: str, display: bool) -> None:
  output = _wrap(tmp_path, f"<p>before {source} after</p>", vendored_katex=vendored_katex)
  page = output.read_text(encoding="utf-8")
  assert 'class="katex"' in page, label
  assert ('class="katex-display"' in page) == display, label
  # The delimiters themselves never reach the shipped page: KaTeX received the
  # content between them, and the annotation carries the formula source.
  assert source not in page, label
  assert content in page, label


@pytest.mark.parametrize("literal", _LITERAL_SOURCES)
def test_wrap_leaves_dollar_amounts_literal(tmp_path: Path, vendored_katex: Path, literal: str) -> None:
  output = _wrap(tmp_path, f"<p>{literal}</p>", vendored_katex=vendored_katex)
  page = output.read_text(encoding="utf-8")
  assert 'class="katex"' not in page
  assert literal in page


def test_driver_escaped_dollar_never_opens_a_span(tmp_path: Path, vendored_katex: Path) -> None:
  r"""An escaped \$ never opens (the chat path's marked escape rule); a genuine $...$ span renders."""
  lines = [
      r"<p>price \$5 and \$10 today</p>",
      r"<p>I owe \$50, and $\alpha$ is fine</p>",
      r"<p>costs \$x$ each</p>",
      r"<p>close $5 + \$3$ total</p>",
  ]
  fragment_path = _write_fragment(tmp_path, chr(10).join(lines) + chr(10), name="escaped.html")
  proc = subprocess.run(["node", str(_DRIVER), str(fragment_path), str(vendored_katex)], capture_output=True,
                        check=False)
  assert proc.returncode == 0, proc.stderr.decode("utf-8")
  out = proc.stdout.decode("utf-8")
  assert out.count('class="katex"') == 2  # only $\alpha$ and the $5 + \$3$ span render
  for literal in (r"price \$5 and \$10 today", r"I owe \$50, and ", r"costs \$x$ each"):
    assert literal in out, literal


def test_driver_copies_protected_blocks_comments_and_tags_verbatim(
    tmp_path: Path, vendored_katex: Path) -> None:
  fragment = (
      "<p>$x$</p>\n"
      "<pre>fence $x$ stays</pre>\n"
      "<code>span $x$ stays</code>\n"
      "<style>p { color: red } /* $x$ */</style>\n"
      "<script>var s = \"$x$\";</script>\n"
      "<textarea>$x$</textarea>\n"
      "<noscript>$x$</noscript>\n"
      "<!-- comment $x$ dropped from the scan -->\n"
      '<a href="attr$x$link">attr</a>\n')
  fragment_path = _write_fragment(tmp_path, fragment, name="protected.html")
  proc = subprocess.run(["node", str(_DRIVER), str(fragment_path), str(vendored_katex)], capture_output=True,
                        check=False)
  assert proc.returncode == 0, proc.stderr.decode("utf-8")
  out = proc.stdout.decode("utf-8")
  assert out.count('class="katex"') == 1  # only the free <p> span rendered
  for literal in ("fence $x$ stays", "span $x$ stays", "var s = \"$x$\"", "<textarea>$x$</textarea>",
                  "<noscript>$x$</noscript>", "comment $x$ dropped"):
    assert literal in out, literal
  assert 'href="attr$x$link"' in out  # tag interiors never scan


def test_driver_runs_as_a_subprocess(tmp_path: Path, vendored_katex: Path) -> None:
  fragment = _write_fragment(tmp_path, "<p>$x^2$</p>", name="driver.html")
  proc = subprocess.run(["node", str(_DRIVER), str(fragment), str(vendored_katex)], capture_output=True, check=False)
  assert proc.returncode == 0, proc.stderr.decode("utf-8")
  assert 'class="katex"' in proc.stdout.decode("utf-8")


def test_cli_defaults_math_on_for_explain_and_off_for_other_genres(
    tmp_path: Path, capsys: pytest.CaptureFixture[str], cli_katex: Path) -> None:
  fragment = _write_fragment(tmp_path, r"<p>$$y = x$$</p>")
  explain_output = tmp_path / "explain.html"
  assert _wrap_cli(tmp_path, fragment, explain_output, "explain").code == 0
  assert 'class="katex-display"' in explain_output.read_text(encoding="utf-8")

  sitrep_output = tmp_path / "sitrep.html"
  assert _wrap_cli(tmp_path, fragment, sitrep_output, "sitrep").code == 0
  sitrep_page = sitrep_output.read_text(encoding="utf-8")
  assert 'class="katex"' not in sitrep_page
  assert "$$y = x$$" in sitrep_page


def test_cli_no_math_disables_prerender_for_explain(tmp_path: Path, capsys: pytest.CaptureFixture[str],
                                                    cli_katex: Path) -> None:
  fragment = _write_fragment(tmp_path, r"<p>$$y = x$$</p>")
  output = tmp_path / "plain.html"
  assert _wrap_cli(tmp_path, fragment, output, "explain", "--no-math").code == 0
  page = output.read_text(encoding="utf-8")
  assert 'class="katex"' not in page
  assert "$$y = x$$" in page


# ---------------------------------------------------------------------------
# byte self-check gate
# ---------------------------------------------------------------------------


def _damaged_fragment() -> bytes:
  r"""TAB standing in for the \t of \text, formfeed for the \f of \frac — the 4914c102 damage."""
  prose = "<p>\u524d\u5411\u4e2d TABext{out_routed} \u5c5e\u6027\u3002</p>\n"
  display = "<p>$$\\x0crac{\\partial L}{\\partial TABext{weights}} = 0$$</p>\n"
  return (prose + display).replace("TAB", "\t").replace("\\x0c", "\x0c").encode("utf-8")


def test_wrap_byte_gate_aborts_before_write_and_names_offsets(tmp_path: Path, capsys: pytest.CaptureFixture[str],
                                                              cli_katex: Path) -> None:
  fragment = _write_fragment(tmp_path, _damaged_fragment(), name="damaged.html")
  output = tmp_path / "damaged_page.html"
  assert _wrap_cli(tmp_path, fragment, output, "explain").code == 1
  err = capsys.readouterr().err
  assert "0x09 at offset" in err
  assert "0x0c at offset" in err
  assert "write aborted" in err
  assert not output.exists()  # no partial output behind the aborted write


def test_wrap_byte_gate_runs_on_the_assembled_bytes(tmp_path: Path, capsys: pytest.CaptureFixture[str],
                                                    cli_katex: Path) -> None:
  r"""A fragment TAB reports at its assembled-page offset, past the template head."""
  fragment = _write_fragment(tmp_path, "<p>x\ty</p>")
  output = tmp_path / "page.html"
  assert _wrap_cli(tmp_path, fragment, output, "explain").code == 1
  offset = int(re.search(r"0x09 at offset (\d+)", capsys.readouterr().err).group(1))
  assert offset > 5000  # the template head alone is longer than any fragment prefix


def test_wrap_byte_gate_passes_a_clean_fragment_through(tmp_path: Path, capsys: pytest.CaptureFixture[str],
                                                        cli_katex: Path) -> None:
  fragment = _write_fragment(tmp_path, r"<p>$$\text{logits} = Wx$$</p>")
  output = tmp_path / "clean.html"
  assert _wrap_cli(tmp_path, fragment, output, "explain").code == 0
  assembled = output.read_bytes()
  assert not [1 for b in assembled if b < 0x20 and b != 0x0A]


# ---------------------------------------------------------------------------
# render-path assertion over the wrap product
# ---------------------------------------------------------------------------


def test_render_path_assertion_passes_the_wrapped_page(tmp_path: Path, vendored_katex: Path) -> None:
  body = ('<div class="triad"><div class="row"><span class="k">You know</span>X</div></div>' +
          "".join(f'<section><h2><span class="n">{i}</span> S</h2><p>$x^{i}$</p></section>' for i in range(1, 6)))
  output = _wrap(tmp_path, body, vendored_katex=vendored_katex)
  by_name: dict[str, list[artifact_check.AssertionOutcome]] = {}
  for outcome in artifact_check.run_assertions("explain", output):
    by_name.setdefault(outcome.name, []).append(outcome)
  assert [o.passed for o in by_name["byte-integrity"]] == [True]
  assert [o.passed for o in by_name["render-path"]] == [True]
  assert 'pre-rendered class="katex" markup' in by_name["render-path"][0].detail
