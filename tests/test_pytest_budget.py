"""The per-test wall-time budget mechanism itself: budgets fail slow tests, the
integration marker buys the larger budget, the collection cap trips, and a
budget-only trip reruns once with the rerun's timing deciding the outcome.

The inner runs load the REAL tests/conftest.py (registered as a plugin), so the
mechanism under test is the enforcement the suite actually runs under; each
inner run's ini shrinks the budgets instead of sleeping for real seconds.
"""

from __future__ import annotations

import conftest
import pytest

_INNER_INI = """
[pytest]
unit_test_budget_seconds = 0.03
integration_test_budget_seconds = 0.5
"""

_INNER_SLOW_TESTS = """
import time

import pytest


@pytest.mark.integration
def test_marked_gets_the_larger_budget():
    time.sleep(0.12)


def test_unmarked_over_budget_fails():
    time.sleep(0.12)
"""

_INNER_RERUN_PASS_TESTS = """
import time

calls = {"n": 0}


def test_over_budget_then_fast():
    calls["n"] += 1
    if calls["n"] == 1:
        time.sleep(0.12)
"""

_INNER_RERUN_FAIL_TESTS = """
import time


def test_always_over_budget():
    time.sleep(0.12)
"""

_INNER_NO_RERUN_TESTS = """
calls = {"n": 0}


def test_assertion_failure_never_reruns():
    calls["n"] += 1
    assert calls["n"] == 2
"""

_INNER_CAP_TESTS = """
import pytest


@pytest.mark.integration
def test_integration_one():
    pass


@pytest.mark.integration
def test_integration_two():
    pass
"""


def _run_inner(pytester: pytest.Pytester, ini: str, test_source: str) -> pytest.RunResult:
  pytester.makefile(".ini", pytest=ini)  # a real pytest.ini, not makeini's tox.ini
  pytester.makepyfile(test_source)
  return pytester.runpytest("-p", "no:cacheprovider", plugins=[conftest])


_INNER_FIXTURE_TIME_TESTS = """
import time

import pytest


@pytest.fixture
def time_spent_outside_the_call():
    time.sleep(0.02)
    yield
    time.sleep(0.02)


def test_fixture_time_counts_in_the_budget(time_spent_outside_the_call):
    pass
"""


def test_setup_and_teardown_time_counts_in_the_budget(pytester: pytest.Pytester) -> None:
  """Fixture setup and teardown accrue into the same budget: two stages each
  inside the unit budget trip it together, so a stage wrapper gone missing
  cannot silently shrink what the budget counts. The trip lands on the
  teardown report, which pytest surfaces as an error, not a failure."""
  result = _run_inner(pytester, _INNER_INI, _INNER_FIXTURE_TIME_TESTS)
  outcomes = result.parseoutcomes()
  assert outcomes.get("errors") == 1, (outcomes, result.stdout.str())
  assert "exceeds the 0.03s unit budget" in result.stdout.str(), result.stdout.str()


def test_budget_mechanism(pytester: pytest.Pytester) -> None:
  """An over-budget test fails with the compliance message; the same sleep under
  the integration marker passes on its larger budget."""
  result = _run_inner(pytester, _INNER_INI, _INNER_SLOW_TESTS)
  outcomes = result.parseoutcomes()
  assert outcomes.get("passed") == 1, (outcomes, result.stdout.str())
  assert outcomes.get("failed") == 1, (outcomes, result.stdout.str())
  out = result.stdout.str()
  assert "exceeds the 0.03s unit budget" in out, out
  assert "Make the test faster, or mark it @pytest.mark.integration" in out, out


def test_budget_only_failure_reruns_once_and_passes(pytester: pytest.Pytester) -> None:
  """A wall-time-only trip reruns once: the rerun is within budget, so the
  combined outcome is a pass, and the summary records both timings."""
  result = _run_inner(pytester, _INNER_INI, _INNER_RERUN_PASS_TESTS)
  result.assert_outcomes(passed=1)
  assert result.ret == 0
  result.stdout.fnmatch_lines(
      [
          "*BUDGET RERUN*: attempt 1 0.1*s over the 0.03s unit budget; rerun 0.0*s within budget*",
      ])


def test_budget_rerun_that_also_exceeds_fails_with_both_timings(pytester: pytest.Pytester) -> None:
  """When the rerun exceeds the budget too, the combined outcome is a failure
  carrying both attempts' timings."""
  result = _run_inner(pytester, _INNER_INI, _INNER_RERUN_FAIL_TESTS)
  result.assert_outcomes(failed=1)
  out = result.stdout.str()
  assert "exceeds the 0.03s unit budget" in out, out
  assert "attempt 1 0.1" in out, out
  assert "rerun 0.1" in out, out


def test_assertion_failure_never_reruns(pytester: pytest.Pytester) -> None:
  """An assertion failure is the outcome on its first attempt: no rerun runs
  that could turn it into a pass, and no budget text rides along."""
  result = _run_inner(pytester, _INNER_INI, _INNER_NO_RERUN_TESTS)
  result.assert_outcomes(failed=1)
  out = result.stdout.str()
  assert "AssertionError" in out, out
  assert "exceeds the 0.03s unit budget" not in out, out
  assert "BUDGET RERUN" not in out, out


def test_integration_cap_trips_at_collection(pytester: pytest.Pytester) -> None:
  """More collected integration tests than max_integration_tests fails the session
  at collection with the cap message."""
  cap_ini = _INNER_INI + "max_integration_tests = 1\n"
  result = _run_inner(pytester, cap_ini, _INNER_CAP_TESTS)
  assert result.ret != 0
  # A collection-time UsageError prints on stderr.
  result.stderr.fnmatch_lines([
      "*2 collected tests carry @pytest.mark.integration; the cap is 1*",
  ])
