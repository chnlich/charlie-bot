"""The outside-write guard's trace parsing: a call strace prints as ??? is counted, any other unparsed line raises."""

import pathlib
import re

import pytest

from rust.parity import guard


@pytest.mark.parametrize(
    "undecodable",
    [
        "2434448 ???()  = ?\n",
        "2434448 ???( <unfinished ...>\n2434448 <... ??? resumed>) = ?\n",
    ],
)
def test_undecodable_call_is_counted_and_a_named_change_after_it_still_counts(
    tmp_path: pathlib.Path, undecodable: str) -> None:
  trace = tmp_path / "cand.server-1.strace"
  trace.write_text(undecodable + '2434448 unlink("/srv/outside/state.json") = 0\n', encoding="utf-8")
  report = guard.Report()
  guard.scan(trace, (tmp_path / "tree",), tmp_path, report)
  assert report.undecodable == [str(trace)]
  assert [change.path for change in report.violations] == ["/srv/outside/state.json"]


@pytest.mark.parametrize("line", ["not a strace line", "2434448 not a syscall line"])
def test_unparsed_line_raises_and_names_the_trace(tmp_path: pathlib.Path, line: str) -> None:
  trace = tmp_path / "cand.server-1.strace"
  trace.write_text(line + "\n", encoding="utf-8")
  with pytest.raises(ValueError, match=re.escape(str(trace))):
    guard.scan(trace, (tmp_path / "tree",), tmp_path, guard.Report())
