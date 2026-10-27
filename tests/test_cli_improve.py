"""Tests for the improve loop's reservation and stop in src/features/improve/improve_command.py."""
import pathlib
from unittest import mock

import pytest

from src.features.improve import improve_command


@pytest.mark.asyncio
async def test_session_takes_a_new_loop_after_stop(tmp_path: pathlib.Path) -> None:
  """After a stop, the session's active lock releases (the sequence's own exit
  step, src/features/improve/improve_sequence.py) and a new loop reserves cleanly."""
  cfg = mock.MagicMock()
  cfg.sessions_dir = tmp_path / "sessions"
  cfg.sessions_dir.mkdir(parents=True, exist_ok=True)

  state = await improve_command.reserve_loop_state("s1", "optimize", "improve/test", str(tmp_path), cfg)
  await improve_command.stop_improve_loop("s1", cfg)
  # The running controller's exit path releases the lock when the sequence ends.
  await improve_command.clear_active_loop_lock("s1", cfg)

  second = await improve_command.reserve_loop_state("s1", "optimize again", "improve/test-2", str(tmp_path), cfg)
  assert second.loop_id != state.loop_id
