"""The backend pool: rotation, exclusion, cooldowns, the quota table and the reviewer's member choice."""

import datetime

import conftest
import pytest

from src.infra import config, models
from src.runtime import backend_pool, review


def _cfg(tmp_path, pool: list[str]) -> config.CharlieBotConfig:
  options = [conftest.backend_option(id=i, label=i, type="cc-claude", model="m") for i in ("a", "b", "c")]
  return config.CharlieBotConfig(charliebot_home=tmp_path, backends={"options": options, "pool": pool})


def _select_id(cfg: config.CharlieBotConfig, exclude=()) -> str | None:
  option = backend_pool.select(cfg, caller="test", exclude=exclude)
  return option.id if option is not None else None


def test_rotation_follows_the_pool_order(tmp_path) -> None:
  cfg = _cfg(tmp_path, ["a", "b", "c"])
  assert [_select_id(cfg) for _ in range(4)] == ["a", "b", "c", "a"]


def test_selection_skips_cooling_members_and_the_callers_exclusion_set(tmp_path) -> None:
  cfg = _cfg(tmp_path, ["a", "b", "c"])
  backend_pool.cool("a", None)
  # The position starts at a: a is cooling, so b is taken; the position moves past b.
  assert _select_id(cfg, exclude={"c"}) == "b"
  assert _select_id(cfg, exclude={"b", "c"}) is None


def test_a_cooldown_lasts_until_the_reported_reset(tmp_path) -> None:
  cfg = _cfg(tmp_path, ["a"])
  reset = models.utc_now() + datetime.timedelta(hours=2)
  backend_pool.cool("a", reset)
  assert _select_id(cfg) is None
  backend_pool.cool("a", models.utc_now() - datetime.timedelta(seconds=1))
  assert _select_id(cfg) == "a"


def test_a_cooldown_without_a_reset_lasts_thirty_minutes(tmp_path) -> None:
  cfg = _cfg(tmp_path, ["a"])
  until = backend_pool.cool("a", None)
  assert abs((until - models.utc_now()) - backend_pool.DEFAULT_COOLDOWN) < datetime.timedelta(seconds=5)
  assert _select_id(cfg) is None


def test_recovery_report_names_each_member_and_its_recovery_time(tmp_path) -> None:
  cfg = _cfg(tmp_path, ["a", "b"])
  reset = datetime.datetime(2030, 1, 2, 3, 4, tzinfo=datetime.UTC)
  backend_pool.cool("a", reset)
  backend_pool.cool("b", reset + datetime.timedelta(hours=1))
  assert backend_pool.recovery_report(cfg) == (
      "a cooling until 2030-01-02T03:04+00:00; b cooling until 2030-01-02T04:04+00:00")


@pytest.mark.parametrize(
    "text",
    [
        "You've hit your usage limit. Upgrade to Pro.",  # Codex
        "Budget has been exceeded! Current cost: 12.3, Max budget: 10",  # LiteLLM
        "Disk quota exceeded",  # kept on purpose: the substring match stays
        "rate limit reached for this key",
        "HTTP 429 Too Many Requests",
    ],
)
def test_quota_error_texts_are_recognized(text: str) -> None:
  assert backend_pool.is_quota_error(text)


@pytest.mark.parametrize("text", ["syntax error in the diff", "connection reset by peer"])
def test_other_error_texts_are_not_quota_errors(text: str) -> None:
  assert not backend_pool.is_quota_error(text)


def test_reviewer_avoids_every_member_the_worker_node_used(tmp_path) -> None:
  cfg = _cfg(tmp_path, ["a", "b", "c"])
  selection = review.select_reviewer_backend(cfg, "reviewer", "a", "m", [], used_backends={"a", "b"})
  assert selection is not None and selection[0] == "c"


def test_reviewer_never_falls_back_to_a_member_the_worker_node_used(tmp_path) -> None:
  """Every candidate used or cooling: the reviewer gets None, never the worker's own member."""
  cfg = _cfg(tmp_path, ["a", "b"])
  backend_pool.cool("b", None)
  assert review.select_reviewer_backend(cfg, "reviewer", "a", "m", [], used_backends={"a"}) is None
  assert review.select_reviewer_backend(cfg, "reviewer", "a", "m", [], used_backends={"a", "b"}) is None


def test_a_check_without_used_members_keeps_the_session_backend_fallback(tmp_path) -> None:
  """Verify and the cold read name no used members: a spent pool falls back to the session backend."""
  cfg = _cfg(tmp_path, ["a", "b"])
  backend_pool.cool("b", None)
  selection = review.select_reviewer_backend(cfg, "verify", "a", "m", [])
  assert selection is not None and selection[0] == "a"


def test_an_empty_pool_runs_the_check_on_the_workers_backend(tmp_path) -> None:
  cfg = _cfg(tmp_path, [])
  selection = review.select_reviewer_backend(cfg, "reviewer", "a", "m", [], used_backends={"a"})
  assert selection is not None and selection[0] == "a"
