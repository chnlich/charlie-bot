"""Drift guard for the chat-client test doubles.

A double that keeps a method its real client lost passes every test that uses it, while production raises
AttributeError at the call. Each public method of a double must exist on the real class it stands in for, with
the same parameter names in the same order.
"""

from __future__ import annotations

import inspect

import conftest
import pytest
from core import test_discord_gateway, test_discord_listener

from src.features.discord import discord_client
from src.features.slack import slack_listener

# (label, double, real class). The label names the double's module: two doubles are both called FakeDiscordClient.
DOUBLE_PAIRS = [
    ("conftest.FakeSlackClient", conftest.FakeSlackClient, slack_listener.SlackClient),
    (
        "core.test_discord_listener.FakeDiscordClient", test_discord_listener.FakeDiscordClient,
        discord_client.DiscordClient),
    ("core.test_discord_gateway.FakeDiscordREST", test_discord_gateway.FakeDiscordREST, discord_client.DiscordClient),
]

# Known parameter drift, each entry a strict xfail. The mark must come off once the double's parameter names
# match the real method's, so a fixed double never stays marked.
KNOWN_PARAM_DRIFT = {}


def _public_methods(double: type) -> list[str]:
  """The public functions a double defines itself."""
  return [name for name, value in vars(double).items() if not name.startswith("_") and inspect.isfunction(value)]


def _param_names(method) -> list[str]:
  """Parameter names after self, in declaration order; keyword-only names included."""
  return list(inspect.signature(method).parameters)[1:]


METHOD_CASES = [
    (label, double, real, method) for label, double, real in DOUBLE_PAIRS for method in _public_methods(double)
]


def _known_drift_mark(label: str, method: str):
  reason = KNOWN_PARAM_DRIFT.get((label, method))
  return () if reason is None else pytest.mark.xfail(strict=True, raises=AssertionError, reason=reason)


EXISTENCE_CASES = [
    pytest.param(label, double, real, method, id=f"{label}->{real.__name__}.{method}")
    for label, double, real, method in METHOD_CASES
]
PARAM_CASES = [
    pytest.param(
        label, double, real, method, id=f"{label}->{real.__name__}.{method}", marks=_known_drift_mark(label, method))
    for label, double, real, method in METHOD_CASES
]


@pytest.mark.parametrize(("label", "double", "real", "method"), EXISTENCE_CASES)
def test_double_method_exists_on_real_client(label: str, double: type, real: type, method: str) -> None:
  assert hasattr(real, method), f"{real.__name__} has no {method}, but {label} still defines it"


@pytest.mark.parametrize(("label", "double", "real", "method"), PARAM_CASES)
def test_double_method_params_match_real_client(label: str, double: type, real: type, method: str) -> None:
  assert _param_names(getattr(double, method)) == _param_names(getattr(real, method))
