"""The per-test wall-time budget mechanism itself: budgets fail slow tests, the
integration marker buys the larger budget, and the collection cap trips.

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
