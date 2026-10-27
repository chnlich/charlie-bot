"""The sidebar-contributions hook (src/runtime/hooks/sidebar_contributions.py) and the packages that fill it.

The registry tests run against an empty registry swapped in for the process-wide one. The deletion test
runs a fresh interpreter that drops one package line from PACKAGES: the session store must keep working
without the Threads view.
"""

import pathlib
import sys
import types

import pytest

from src.infra import models
from src.runtime import scheduled_sessions
from src.runtime.hooks import sidebar_contributions as hook


@pytest.fixture
def empty_registry(monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setattr(hook, "_registered", {})
  monkeypatch.setattr(hook, "_resolved", None)


class Roots(hook.SidebarContribution):
  """Answers the view named by a session's name prefix: "alpha-..." roots view alpha, "beta-..." roots view beta."""

  def view_member(self, meta: models.SessionMetadata) -> str | None:
    return meta.name.split("-")[0] if meta.name.startswith(("alpha-", "beta-")) else None


def stub_module(monkeypatch: pytest.MonkeyPatch, name: str, **attrs: object) -> None:
  monkeypatch.setitem(sys.modules, name, types.SimpleNamespace(**attrs))


def test_the_base_class_adds_nothing(tmp_path: pathlib.Path) -> None:
  contribution = hook.SidebarContribution()

  assert contribution.watched_files == ()
  assert contribution.row_flags(tmp_path, "sid") == {}
  assert contribution.view_member(models.SessionMetadata(profile="manager", name="any")) is None
  assert contribution.copy_on_fork(tmp_path, tmp_path) is None
  assert contribution.drop_runtime_state("sid") is None


@pytest.mark.usefixtures("empty_registry")
def test_a_contribution_imports_on_first_use_and_comes_back_in_registration_order(
    monkeypatch: pytest.MonkeyPatch) -> None:
  second, first = hook.SidebarContribution(), hook.SidebarContribution()
  hook.register_sidebar_contribution("second", "not_imported_yet_a:contribution")
  hook.register_sidebar_contribution("first", "not_imported_yet_b:contribution")
  assert "not_imported_yet_a" not in sys.modules

  stub_module(monkeypatch, "not_imported_yet_a", contribution=second)
  stub_module(monkeypatch, "not_imported_yet_b", contribution=first)

  assert hook.sidebar_contributions() == (second, first)


@pytest.mark.usefixtures("empty_registry")
def test_a_contribution_instance_can_be_registered_directly() -> None:
  contribution = hook.SidebarContribution()

  hook.register_sidebar_contribution("inline", contribution)

  assert hook.sidebar_contributions() == (contribution,)


@pytest.mark.usefixtures("empty_registry")
def test_a_second_registration_of_one_name_raises_and_leaves_the_registry_alone() -> None:
  hook.register_sidebar_contribution("alpha", "module_a:contribution")

  with pytest.raises(ValueError, match="alpha"):
    hook.register_sidebar_contribution("alpha", "module_b:contribution")

  assert hook._registered == {"alpha": "module_a:contribution"}


@pytest.mark.usefixtures("empty_registry")
def test_every_view_a_contribution_answers_maps_its_own_subtree(monkeypatch: pytest.MonkeyPatch) -> None:
  stub_module(monkeypatch, "roots_module", contribution=Roots())
  hook.register_sidebar_contribution("roots", "roots_module:contribution")
  alpha_root = models.SessionMetadata(profile="manager", name="alpha-root")
  alpha_child = models.SessionMetadata(profile="manager", name="child", task_parent_id=alpha_root.id)
  beta_root = models.SessionMetadata(profile="manager", name="beta-root")
  plain = models.SessionMetadata(profile="manager", name="plain")

  views = scheduled_sessions.view_subtree_roots([alpha_child, plain, alpha_root, beta_root])

  assert views == {
      "alpha": {
          alpha_root.id: alpha_root.id,
          alpha_child.id: alpha_root.id
      },
      "beta": {
          beta_root.id: beta_root.id
      },
  }


@pytest.mark.usefixtures("empty_registry")
def test_a_view_no_session_roots_has_no_entry() -> None:
  assert scheduled_sessions.view_subtree_roots([models.SessionMetadata(profile="manager", name="plain")]) == {}
