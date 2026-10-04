"""The sweep runner's extraction and wiring contracts.

The parser is the standing sweep's front door: a doc-format drift that silently
yields zero units darkens every metric of every round after it, so the real
doc's shape is asserted here against the actual file, and the builder→consumer
env carry and the exit-path scratch sweep are asserted against a synthetic doc.
"""

from pathlib import Path

import pytest
from conftest import ROOT

import tools.perf_sweep

_PAIR_UNITS = ("M35", "M55", "M70", "M71", "M112", "M120")


def _fake_doc(consumer_body: str) -> str:
  lines = [
      "# Perf baseline",
      "",
      "## Collector commands",
      "",
      "Preflight:",
      "",
      "```bash",
      "echo preflight-ok",
      "```",
      "",
      "M99 — fake unit. Builder builds a scratch home; consumer consumes it.",
      "",
      "```bash",
      "d=$(mktemp -d /tmp/perf-sweep-test-XXXXXX)",
      'touch "$d/marker"',
      'echo "export T_HOME=$d"',
      "```",
      "",
      "```bash",
      consumer_body,
      "```",
      "",
      "## Sampling history",
      "",
  ]
  return "\n".join(lines)


def test_parse_collectors_on_the_real_doc() -> None:
  preamble, units = tools.perf_sweep.parse_collectors(ROOT / "docs" / "perf_baseline.md")
  assert len(preamble) == 1
  assert "fetch origin main" in preamble[0]
  labels = [label for label, _ in units]
  assert len(units) >= 130
  assert sum(len(blocks) for _, blocks in units) >= 135
  for label in _PAIR_UNITS:
    assert labels.count(label) == 1, f"{label} must be one unit"
    assert units[labels.index(label)][1], f"{label} must carry its builder and consumer blocks"
  assert units[labels.index("M7 warm-gate changed round")][1] == []


def test_runner_carries_builder_env_and_sweeps_scratch(tmp_path: Path, capsys: pytest.Capsys) -> None:
  doc = tmp_path / "perf_baseline.md"
  doc.write_text(_fake_doc('test -f "$T_HOME/marker" && echo consumer-read-the-env'))
  assert tools.perf_sweep.SweepRunner(doc, None).run() == 0
  out = capsys.readouterr().out
  assert "consumer-read-the-env" in out
  assert not list(Path("/tmp").glob("perf-sweep-test-*")), "the exit-path sweep removes the scratch"


def test_runner_marks_failed_unit_and_still_sweeps(tmp_path: Path, capsys: pytest.Capsys) -> None:
  doc = tmp_path / "perf_baseline.md"
  doc.write_text(_fake_doc('[ -n "$T_HOME" ] || exit 9; [ -f "$T_HOME/marker" ] || exit 8; exit 7'))
  assert tools.perf_sweep.SweepRunner(doc, None).run() == 1
  out = capsys.readouterr().out
  assert "FAILED units: M99" in out
  assert "rc=7" in out, "the consumer's own exit surfaces; an env-carry failure would read rc=9 or rc=8"
  assert not list(Path("/tmp").glob("perf-sweep-test-*")), "a failed unit's scratch is swept all the same"


def test_runner_pins_one_scratch_login_dir_for_every_block(
    tmp_path: Path,
    capsys: pytest.Capsys,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  login = tmp_path / "login"
  login.mkdir()
  monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(login))
  doc = tmp_path / "perf_baseline.md"
  doc.write_text(
      _fake_doc('echo "consumer-config=$CLAUDE_CONFIG_DIR"; exit 7').replace(
          "echo preflight-ok", 'echo "preflight-config=$CLAUDE_CONFIG_DIR"'))
  assert tools.perf_sweep.SweepRunner(doc, None).run() == 1
  out = capsys.readouterr().out
  seen = [
      line.split("=", 1)[1] for line in out.splitlines() if line.startswith(("preflight-config=", "consumer-config="))
  ]
  assert len(seen) == 2 and seen[0] == seen[1], "the preflight and every unit block share one login directory"
  config_dir = Path(seen[0])
  assert config_dir != login
  assert config_dir.name.startswith("perf-sweep-claude-config-")
  assert not config_dir.exists(), "a run with a failed unit still removes the scratch login directory"


def test_export_of_the_pinned_login_dir_fails_the_unit(tmp_path: Path, capsys: pytest.Capsys) -> None:
  login = tmp_path / "login"
  login.mkdir()
  doc = tmp_path / "perf_baseline.md"
  lines = _fake_doc('echo "consumer-config=$CLAUDE_CONFIG_DIR"').splitlines()
  lines.insert(lines.index('echo "export T_HOME=$d"') + 1, f'echo "export CLAUDE_CONFIG_DIR={login}"')
  doc.write_text("\n".join(lines) + "\n")
  assert tools.perf_sweep.SweepRunner(doc, None).run() == 1
  out = capsys.readouterr().out
  assert "FAILED units: M99" in out
  consumer = [line.split("=", 1)[1] for line in out.splitlines() if line.startswith("consumer-config=")]
  assert len(consumer) == 1 and Path(consumer[0]).name.startswith("perf-sweep-claude-config-"), "the pin survives"
  assert login.is_dir(), "a rejected export is never swept as scratch"


def test_unparseable_export_line_fails_the_unit_not_the_sweep(tmp_path: Path, capsys: pytest.Capsys) -> None:
  """The export lines are corpus-derived, so one the parser cannot split is that unit's failure."""
  doc = tmp_path / "perf_baseline.md"
  lines = _fake_doc('echo consumer-ran').splitlines()
  # The corpus shapes: a space-bearing value splits into a token with no '='.
  lines.insert(lines.index("## Sampling history") - 2, 'echo "export BAD_VALUE=two words"')
  doc.write_text("\n".join(lines) + "\n")
  assert tools.perf_sweep.SweepRunner(doc, None).run() == 1
  out = capsys.readouterr().out
  assert "the unit fails, the sweep continues" in out
  assert "FAILED units: M99" in out
  assert "consumer-ran" in out, "the consumer still ran on the unit's own failure"
  assert not list(Path("/tmp").glob("perf-sweep-test-*")), "the scratch sweep survives the parse failure"
