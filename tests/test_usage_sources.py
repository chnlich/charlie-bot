"""Tests for the usage-sources hook (src/runtime/hooks/usage_sources.py) and the registrations that fill it.

The hook tests run against an empty registry swapped in for the process-wide one, so a source a
test registers never reaches another test. The registration tests read the real registry the
suite's conftest fills through ``registrations.register_all()``; that the registration imports no
implementation module is tests/test_backend_hooks.py's check.
"""

from __future__ import annotations

import datetime
import sys
import types

import pytest

from src.features.usage import token_tally
from src.runtime.hooks import backend_types, usage_sources


@pytest.fixture
def empty_registry(monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setattr(usage_sources, "_sources", {})
  monkeypatch.setattr(usage_sources, "_type_sources", {})


def _source(
    name: str, *prefixes: str, run_logs_only: bool = False, module: str | None = None) -> usage_sources.UsageSource:
  return usage_sources.UsageSource(name=name, id_prefixes=prefixes, run_logs_only=run_logs_only, module=module)


@pytest.mark.usefixtures("empty_registry")
def test_sources_come_back_in_registration_order() -> None:
  for name in ("zeta", "alpha", "mid"):
    usage_sources.register_source(_source(name))
  assert [s.name for s in usage_sources.sources()] == ["zeta", "alpha", "mid"]


@pytest.mark.usefixtures("empty_registry")
def test_a_repeated_name_or_id_prefix_is_rejected_and_leaves_the_registry_alone() -> None:
  usage_sources.register_source(_source("alpha", "alpha-"))
  with pytest.raises(ValueError, match="alpha"):
    usage_sources.register_source(_source("alpha", "other-"))
  with pytest.raises(ValueError, match="alpha-"):
    usage_sources.register_source(_source("beta", "alpha-"))
  assert [s.name for s in usage_sources.sources()] == ["alpha"]


@pytest.mark.usefixtures("empty_registry")
def test_a_backend_type_resolves_to_its_source_even_when_the_source_registers_later() -> None:
  usage_sources.attribute_backend_type("type-a", "alpha")
  usage_sources.register_source(_source("alpha"))
  assert usage_sources.source_for("type-a").name == "alpha"
  assert usage_sources.source_for("unattributed") is None


@pytest.mark.usefixtures("empty_registry")
def test_attribution_to_a_source_that_never_registered_raises_on_lookup() -> None:
  """A deleted backend package whose attributing sibling remains fails loudly instead of
  attributing the sibling's usage to nothing."""
  usage_sources.attribute_backend_type("type-a", "gone")
  with pytest.raises(ValueError, match="gone"):
    usage_sources.source_for("type-a")


@pytest.mark.usefixtures("empty_registry")
def test_a_backend_type_attributes_once() -> None:
  usage_sources.attribute_backend_type("type-a", "alpha")
  with pytest.raises(ValueError, match="type-a"):
    usage_sources.attribute_backend_type("type-a", "beta")


@pytest.mark.usefixtures("empty_registry")
def test_a_retired_backend_id_resolves_through_the_id_prefixes() -> None:
  """An id that left config.yaml has no option to read a type from; the prefix every id keeps
  names its source, the longest-lived record of which backend ran the call."""
  usage_sources.register_source(_source("alpha", "alpha-", "alpha2-"))
  usage_sources.register_source(_source("beta", "beta-"))
  assert token_tally.backend_source("alpha2-old", {}).name == "alpha"
  assert token_tally.backend_source("beta-old", {}).name == "beta"
  assert token_tally.backend_source("gamma-old", {}) is None


@pytest.mark.usefixtures("empty_registry")
def test_a_source_without_a_module_has_no_implementation() -> None:
  source = _source("alpha", run_logs_only=True)
  usage_sources.register_source(source)
  with pytest.raises(ValueError, match="alpha"):
    usage_sources.implementation(source)


class _Account(usage_sources.QuotaAccount):
  """A quota account that only names itself."""

  def __init__(self, provider: str, label: str) -> None:
    self.provider = provider
    self.label = label
    self.last_error = "no data"

  async def fetch(self) -> dict | None:
    return None


@pytest.mark.usefixtures("empty_registry")
def test_quota_accounts_come_from_the_registered_sources_in_registration_order(monkeypatch: pytest.MonkeyPatch) -> None:
  """Each source's implementation module lists its own accounts; a source whose module defines no
  ``quota_accounts`` or has no module adds none."""

  def implementation(name: str, *labels: str) -> str:
    module = types.ModuleType(name)
    if labels:
      module.quota_accounts = lambda: [_Account(name, label) for label in labels]
    monkeypatch.setitem(sys.modules, name, module)
    return name

  usage_sources.register_source(_source("zeta", module=implementation("fake_zeta", "z1", "z2")))
  usage_sources.register_source(_source("logs-only", module=implementation("fake_logs_only")))
  usage_sources.register_source(_source("clc", run_logs_only=True))
  usage_sources.register_source(_source("alpha", module=implementation("fake_alpha", "a1")))

  assert [(a.provider, a.label) for a in usage_sources.quota_accounts()] == [
      ("fake_zeta", "z1"), ("fake_zeta", "z2"), ("fake_alpha", "a1")
  ]


@pytest.mark.usefixtures("empty_registry")
def test_sweeps_come_from_the_registered_sources_in_registration_order(monkeypatch: pytest.MonkeyPatch) -> None:
  """Each source's implementation module sweeps with the one scope the caller built; a source whose
  module defines no ``sweep`` or has no module adds none."""
  scope = usage_sources.SweepScope(
      cfg=None,
      now=datetime.datetime(2026, 9, 4, tzinfo=datetime.UTC),
      dry_run=True,
      session_id=None,
      facts={},
      references={},
      orphan_idle_days=2,
      vacuum=False,
      force=False)
  received: list[tuple[str, usage_sources.SweepScope]] = []

  def implementation(name: str, *categories: str) -> str:
    module = types.ModuleType(name)
    if categories:

      def sweep(given: usage_sources.SweepScope) -> usage_sources.SourceSweep:
        received.append((name, given))
        return usage_sources.SourceSweep(
            categories=tuple(usage_sources.CategoryResult(category, "files", 0, 0) for category in categories),
            freelist=None)

      module.sweep = sweep
    monkeypatch.setitem(sys.modules, name, module)
    return name

  usage_sources.register_source(_source("zeta", module=implementation("fake_zeta", "z1", "z2")))
  usage_sources.register_source(_source("logs-only", module=implementation("fake_logs_only")))
  usage_sources.register_source(_source("clc", run_logs_only=True))
  usage_sources.register_source(_source("alpha", module=implementation("fake_alpha", "a1")))

  swept = usage_sources.sweep_all(scope)

  assert [[category.name for category in one.categories] for one in swept] == [["z1", "z2"], ["a1"]]
  assert [(name, given is scope) for name, given in received] == [("fake_zeta", True), ("fake_alpha", True)]


def test_the_panel_lists_claude_accounts_before_codex() -> None:
  names = [source.name for source in usage_sources.sources()]
  assert names.index("Claude Code") < names.index("Codex")


def test_the_backend_packages_register_their_sources() -> None:
  """A retired backend id attributes through these prefixes, and the capture reads each module's logs."""
  registered = {s.name: (s.id_prefixes, s.run_logs_only, s.module) for s in usage_sources.sources()}
  assert registered == {
      "Claude Code": (("claude-",), False, "src.backends.claude_code.usage_logs"),
      "Codex": (("codex-",), False, "src.backends.codex.usage_logs"),
      "opencode": (("opencode-",), False, "src.backends.opencode.usage_logs"),
      "CLC": (("charlie-code-",), True, None),
  }


def test_every_backend_type_attributes_to_the_cli_that_logs_it() -> None:
  expected = {
      "cc-claude": "Claude Code",
      "cc-kimi": "Claude Code",
      "cc-openai-compatible": "Claude Code",
      "codex": "Codex",
      "opencode": "opencode",
      "charlie-code": "CLC",
  }
  for backend_type in backend_types.registered_types():
    source = usage_sources.source_for(backend_type)
    assert (source.name if source else None) == expected.get(backend_type), backend_type
