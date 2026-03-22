"""Tests for src/cli/improve.py."""

import json
from pathlib import Path
from unittest.mock import MagicMock, patch

from src.cli.improve import main


def _mock_config(tmp_path: Path):
  """Create a mock config with sessions_dir."""
  cfg = MagicMock()
  cfg.sessions_dir = tmp_path / "sessions"
  cfg.sessions_dir.mkdir(parents=True, exist_ok=True)
  cfg.server_port = 9443
  return cfg


def test_main_runs_iterations(tmp_path: Path):
  """main() delegates each iteration and collects summaries."""
  cfg = _mock_config(tmp_path)
  session_id = "test-session"
  (cfg.sessions_dir / session_id).mkdir(parents=True, exist_ok=True)

  # Mock responses for 2 iterations
  delegate_resp = MagicMock()
  delegate_resp.json.return_value = {"thread_id": "t1"}
  delegate_resp.raise_for_status = MagicMock()

  thread_resp = MagicMock()
  thread_resp.json.return_value = {"status": "completed"}
  thread_resp.raise_for_status = MagicMock()

  events_resp = MagicMock()
  events_resp.json.return_value = [
      {"type": "assistant", "content": "Made improvements to auth module."},
  ]
  events_resp.raise_for_status = MagicMock()

  def mock_post(url, **kwargs):
    return delegate_resp

  def mock_get(url, **kwargs):
    if "/events" in url:
      return events_resp
    return thread_resp

  with patch("sys.argv", ["improve", "--session", session_id, "--repo", "/tmp/repo", "--iterations", "2", "--goal", "optimize"]), \
       patch("src.cli.improve.get_config", return_value=cfg), \
       patch("src.cli.improve.requests.post", side_effect=mock_post) as post_mock, \
       patch("src.cli.improve.requests.get", side_effect=mock_get) as get_mock, \
       patch("src.cli.improve.time.sleep"):
    main()

  # Should have posted twice (once per iteration)
  assert post_mock.call_count == 2


def test_main_stops_when_state_stopped(tmp_path: Path):
  """main() stops early when improve_state.json has status=stopped."""
  cfg = _mock_config(tmp_path)
  session_id = "stop-session"
  state_dir = cfg.sessions_dir / session_id
  state_dir.mkdir(parents=True, exist_ok=True)

  # Write stopped state before starting
  (state_dir / "improve_state.json").write_text(json.dumps({
      "goal": "optimize",
      "max_iterations": 5,
      "status": "stopped",
  }))

  with patch("sys.argv", ["improve", "--session", session_id, "--repo", "/tmp/repo", "--iterations", "3", "--goal", "optimize"]), \
       patch("src.cli.improve.get_config", return_value=cfg), \
       patch("src.cli.improve.requests.post") as post_mock, \
       patch("src.cli.improve.time.sleep"):
    main()

  # Should not have delegated any iterations
  post_mock.assert_not_called()


def test_main_handles_failed_thread(tmp_path: Path):
  """main() continues when a thread completes with failed status."""
  cfg = _mock_config(tmp_path)
  session_id = "fail-session"
  (cfg.sessions_dir / session_id).mkdir(parents=True, exist_ok=True)

  delegate_resp = MagicMock()
  delegate_resp.json.return_value = {"thread_id": "t1"}
  delegate_resp.raise_for_status = MagicMock()

  thread_resp = MagicMock()
  thread_resp.json.return_value = {"status": "failed"}
  thread_resp.raise_for_status = MagicMock()

  events_resp = MagicMock()
  events_resp.json.return_value = []
  events_resp.raise_for_status = MagicMock()

  with patch("sys.argv", ["improve", "--session", session_id, "--repo", "/tmp/repo", "--iterations", "1", "--goal", "fix"]), \
       patch("src.cli.improve.get_config", return_value=cfg), \
       patch("src.cli.improve.requests.post", return_value=delegate_resp), \
       patch("src.cli.improve.requests.get", side_effect=lambda url, **kw: events_resp if "/events" in url else thread_resp), \
       patch("src.cli.improve.time.sleep"):
    main()
