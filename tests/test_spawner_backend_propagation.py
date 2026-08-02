from pathlib import Path
from typing import Any, Optional

import pytest

from src.core import review, spawner
from src.core.config import CharlieBotConfig
from src.core.git import BaseResolution
from src.core.models import BackendOption, SessionMetadata, SpawnRequest, TaskType, ThreadMetadata, ThreadStatus

from conftest import JudgmentShim


def _build_cfg() -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=Path("/tmp/charliebot-test"),
      worktree_dir="/tmp/worktrees",
      backend_options=[
          BackendOption(
              id="claude-opus-4.6",
              label="Opus",
              type="cc-claude",
              model="claude-opus-4-6",
              effort="max",
              cli_binary="claude-sub",
          ),
          BackendOption(id="codex-o3", label="Codex", type="codex", model="o3"),
      ],
  )


def test_resolve_backend_option_requires_valid_backend_and_model() -> None:
  cfg = _build_cfg()
  opt = spawner.resolve_backend_option(cfg, "claude-opus-4.6", "claude-opus-4-6")
  assert opt.id == "claude-opus-4.6"
  assert opt.model == "claude-opus-4-6"
  assert opt.effort == "max"
  assert opt.cli_binary == "claude-sub"

  with pytest.raises(ValueError, match="not configured"):
    spawner.resolve_backend_option(cfg, "missing", "o3")

  with pytest.raises(ValueError, match="model is required"):
    spawner.resolve_backend_option(cfg, "codex-o3", "")


def test_resolve_backend_option_allows_antigravity_missing_model() -> None:
  cfg = CharlieBotConfig(
      charliebot_home=Path("/tmp/charliebot-test"),
      worktree_dir="/tmp/worktrees",
      backend_options=[
          BackendOption(id="agy", label="Antigravity", type="antigravity"),
      ],
  )

  opt = spawner.resolve_backend_option(cfg, "agy", None)

  assert opt.id == "agy"
  assert opt.model is None
  assert opt.cli_binary is None


@pytest.mark.parametrize(
    "backend_type",
    ["cc-claude", "cc-kimi", "cc-openai-compatible", "codex", "charlie-code", "gemini", "opencode"],
)
def test_resolve_backend_option_rejects_missing_model_for_model_required_backends(backend_type: str) -> None:
  cfg = CharlieBotConfig(
      charliebot_home=Path("/tmp/charliebot-test"),
      worktree_dir="/tmp/worktrees",
      backend_options=[
          BackendOption(id=backend_type, label=backend_type, type=backend_type),
      ],
  )

  with pytest.raises(ValueError, match="model is required"):
    spawner.resolve_backend_option(cfg, backend_type, None)


def test_build_worker_prompt_makes_iteration_reports_advisory() -> None:
  prompt = spawner._build_worker_prompt(
      description="Improve the CLI",
      repo_path=Path("/tmp/repo"),
      base_branch="main",
      branch_name="improve/test/iter2",
      wt_path="/tmp/worktrees/improve-test-iter2",
      session_meta=SessionMetadata(id="session-id", name="Improve Session"),
      cfg=_build_cfg(),
      task_type=TaskType.IMPLEMENT,
      loop_dir="/tmp/loops/2",
      iteration_number=2,
  )

  assert "Treat them as advisory evidence and hints only." in prompt
  assert "must not dictate your plan for this iteration" not in prompt
  assert "### What Changed" in prompt
  assert "### Evidence" in prompt
  assert "### Advisory Notes" in prompt
  assert "### Next" not in prompt


def test_build_worker_prompt_task_type_implement_matches_legacy_format() -> None:
  prompt = spawner._build_worker_prompt(
      description="Implement X",
      repo_path=Path("/tmp/repo"),
      base_branch="main",
      branch_name="charliebot/task-xyz",
      wt_path="/tmp/worktrees/charliebot-task-xyz",
      session_meta=SessionMetadata(id="session-id", name="impl"),
      cfg=_build_cfg(),
      task_type=TaskType.IMPLEMENT,
  )
  assert "Commit your changes with descriptive messages." in prompt
  assert "A reviewer will handle that." in prompt
  assert "Do NOT modify tracked files." not in prompt
  assert "Do NOT commit." not in prompt


def test_build_worker_prompt_instructs_task_spec_source_file_handling() -> None:
  prompt = spawner._build_worker_prompt(
      description="## Goal\nImplement X\n\n## Source Files\n- /tmp/source.md",
      repo_path=Path("/tmp/repo"),
      base_branch="main",
      branch_name="charliebot/task-xyz",
      wt_path="/tmp/worktrees/charliebot-task-xyz",
      session_meta=SessionMetadata(id="session-id", name="impl"),
      cfg=_build_cfg(),
      task_type=TaskType.IMPLEMENT,
  )

  assert "contains a `## Source Files` section" in prompt
  assert "read every listed source file before editing" in prompt
  assert "stop and report the conflict" in prompt
  assert "instead of inventing a merged requirement" in prompt


def test_build_worker_prompt_task_type_quick_edit_skips_reviewer_mention() -> None:
  prompt = spawner._build_worker_prompt(
      description="Cherry-pick fix",
      repo_path=Path("/tmp/repo"),
      base_branch="main",
      branch_name="charliebot/task-xyz",
      wt_path="/tmp/worktrees/charliebot-task-xyz",
      session_meta=SessionMetadata(id="session-id", name="quick"),
      cfg=_build_cfg(),
      task_type=TaskType.QUICK_EDIT,
  )
  assert "Commit your changes with descriptive messages." in prompt
  assert "No reviewer will run" in prompt
  assert "A reviewer will handle that." not in prompt


def test_build_worker_prompt_task_type_script_run_forbids_edits_and_commits() -> None:
  prompt = spawner._build_worker_prompt(
      description="Run SLURM benchmark",
      repo_path=Path("/tmp/repo"),
      base_branch="main",
      branch_name="charliebot/task-xyz",
      wt_path="/tmp/worktrees/charliebot-task-xyz",
      session_meta=SessionMetadata(id="session-id", name="script"),
      cfg=_build_cfg(),
      task_type=TaskType.SCRIPT_RUN,
  )
  assert "Do NOT modify tracked files" in prompt
  assert "Do NOT commit" in prompt
  assert "Commit your changes with descriptive messages." not in prompt
  assert "A reviewer will handle that." not in prompt


def test_build_worker_prompt_rejects_verify_task_type() -> None:
  with pytest.raises(ValueError, match="unsupported task_type"):
    spawner._build_worker_prompt(
        description="Verify plan",
        repo_path=Path("/tmp/repo"),
        base_branch="main",
        branch_name="charliebot/task-xyz",
        wt_path="/tmp/worktrees/charliebot-task-xyz",
        session_meta=SessionMetadata(id="session-id", name="verify"),
        cfg=_build_cfg(),
        task_type=TaskType.VERIFY,
    )


@pytest.mark.asyncio
async def test_worker_start_summary_is_locator_without_task_description(monkeypatch: pytest.MonkeyPatch) -> None:
  thread = ThreadMetadata(
      id="12345678-1234-1234-1234-123456789abc",
      session_id="session-id",
      description="Sensitive task description",
      backend="codex-o3",
      model="o3",
  )
  captured: dict[str, Any] = {}

  class FakeWorker:

    async def run(self) -> int:
      return 0

    async def terminate(self) -> None:
      return None

  class FakeThreadManager(JudgmentShim):

    async def save_metadata(self, meta: ThreadMetadata) -> None:
      captured["saved_thread"] = meta

  class FakeSessionManager(JudgmentShim):

    async def persist_and_broadcast(self, session_id: str, event: dict[str, Any]) -> None:
      captured["session_id"] = session_id
      captured["event"] = event

  monkeypatch.setattr(spawner, "_worker_summary_timestamp", lambda: "2026-07-01 12:34 PDT")

  exit_code, quota_exhausted, error = await spawner._stream_worker_events(
      FakeWorker(),
      "session-id",
      "Sensitive task description",
      thread,
      FakeThreadManager(),
      FakeSessionManager(),
  )

  assert exit_code == 0
  assert quota_exhausted is False
  assert error == ""
  event = captured["event"]
  assert event["status"] == "running"
  assert "12345678" in event["content"]
  assert thread.id in event["content"]
  assert "status: running" in event["content"]
  assert "Workers panel" in event["content"]
  assert "Sensitive task description" not in event["content"]


@pytest.mark.asyncio
async def test_worker_finish_summary_is_locator_without_task_description(monkeypatch: pytest.MonkeyPatch) -> None:
  thread = ThreadMetadata(
      id="87654321-4321-4321-4321-cba987654321",
      session_id="session-id",
      description="Sensitive task description",
      backend="codex-o3",
      model="o3",
  )
  captured: dict[str, Any] = {}

  class FakeThreadManager(JudgmentShim):

    async def get_thread(self, session_id: str, thread_id: str) -> ThreadMetadata:
      del session_id, thread_id
      return thread

  class FakeSessionManager(JudgmentShim):

    async def get_session(self, session_id: str) -> SessionMetadata:
      return SessionMetadata(id=session_id, name="Test")

    async def mark_unread(self, session_id: str) -> None:
      captured["mark_unread"] = session_id

    async def persist_and_broadcast(self, session_id: str, event: dict[str, Any]) -> None:
      captured["session_id"] = session_id
      captured["event"] = event

  async def fake_read_events_summary(session_id: str, thread_id: str, thread_mgr: Any) -> str:
    del session_id, thread_id, thread_mgr
    return "Worker output body"

  monkeypatch.setattr(spawner, "_read_events_summary", fake_read_events_summary)
  monkeypatch.setattr(spawner, "_worker_summary_timestamp", lambda: "2026-07-01 12:35 PDT")

  events_summary, full_summary = await spawner._broadcast_completion(
      "session-id",
      "Sensitive task description",
      thread,
      0,
      FakeThreadManager(),
      FakeSessionManager(),
  )

  assert events_summary == "Worker output body"
  assert "Sensitive task description" in full_summary
  event = captured["event"]
  assert event["status"] == "completed"
  assert "87654321" in event["content"]
  assert thread.id in event["content"]
  assert "status: completed" in event["content"]
  assert "Workers panel" in event["content"]
  assert "Sensitive task description" not in event["content"]
  assert "Worker output body" not in event["content"]
  assert "Worker output body" in event["full_content"]


@pytest.mark.asyncio
async def test_resolve_requested_subagent_backend_model_uses_requested_backend() -> None:
  cfg = _build_cfg()

  class FakeSessionManager(JudgmentShim):

    async def get_session(self, session_id: str) -> SessionMetadata:
      assert session_id == "session-id"
      return SessionMetadata(id=session_id, name="Test", backend="claude-opus-4.6")

  backend, model = await spawner.resolve_requested_subagent_backend_model(
      "session-id", cfg, FakeSessionManager(), requested_backend="codex-o3")

  assert backend == "codex-o3"
  assert model == "o3"


@pytest.mark.asyncio
async def test_resolve_requested_subagent_backend_model_defaults_to_session_backend() -> None:
  cfg = _build_cfg()

  class FakeSessionManager(JudgmentShim):

    async def get_session(self, session_id: str) -> SessionMetadata:
      assert session_id == "session-id"
      return SessionMetadata(id=session_id, name="Test", backend="claude-opus-4.6")

  backend, model = await spawner.resolve_requested_subagent_backend_model("session-id", cfg, FakeSessionManager())

  assert backend == "claude-opus-4.6"
  assert model == "claude-opus-4-6"


@pytest.mark.asyncio
async def test_resolve_requested_subagent_backend_model_allows_antigravity_missing_model() -> None:
  cfg = CharlieBotConfig(
      charliebot_home=Path("/tmp/charliebot-test"),
      worktree_dir="/tmp/worktrees",
      backend_options=[
          BackendOption(id="agy", label="Antigravity", type="antigravity"),
      ],
  )

  class FakeSessionManager(JudgmentShim):

    async def get_session(self, session_id: str) -> SessionMetadata:
      assert session_id == "session-id"
      return SessionMetadata(id=session_id, name="Test", backend="agy")

  backend, model = await spawner.resolve_requested_subagent_backend_model("session-id", cfg, FakeSessionManager())

  assert backend == "agy"
  assert model is None


@pytest.mark.asyncio
async def test_spawn_worker_creates_worktree_and_uses_worktree_cwd(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(
      charliebot_home=tmp_path / "charliebot-home",
      worktree_dir=str(tmp_path / "worktrees"),
      backend_options=[
          BackendOption(id="codex-o3", label="Codex", type="codex", model="o3"),
      ],
  )
  repo_path = (tmp_path / "repo").resolve()
  repo_path.mkdir(parents=True, exist_ok=True)
  events_log = tmp_path / "events.jsonl"
  thread = ThreadMetadata(
      id="thread-1",
      session_id="session-id",
      description="Do work",
  )
  captures: dict[str, Any] = {}

  class FakeSessionManager(JudgmentShim):

    async def get_session(self, session_id: str) -> Any:
      return SessionMetadata(id=session_id, name="Test Session")

    async def save_chat_event(self, session_id: str, event: dict[str, Any]) -> None:
      captures["chat_event"] = event

    async def persist_and_broadcast(self, session_id: str, event: dict[str, Any]) -> None:
      captures["broadcast_event"] = event

  class FakeThreadManager(JudgmentShim):

    async def get_thread(self, session_id: str, thread_id: str) -> Optional[ThreadMetadata]:
      return thread

    async def save_metadata(self, meta: ThreadMetadata) -> None:
      captures["saved_thread"] = meta

    async def get_events_log_path(self, session_id: str, thread_id: str) -> Path:
      return events_log

    async def update_status(
        self,
        session_id: str,
        thread_id: str,
        status: Any,
        pid: Optional[int] = None,
        exit_code: Optional[int] = None,
        completed_at: Any = None,
    ) -> None:
      captures["status"] = status
      captures["exit_code"] = exit_code

  async def fake_git_current_branch(repo: Path) -> str:
    assert repo == repo_path
    return "main"

  async def fake_git_create_worktree(repo: Path, base_branch: str, branch_name: str, wt_path: Path) -> BaseResolution:
    captures["git_create_worktree"] = {
        "repo": repo,
        "base_branch": base_branch,
        "branch_name": branch_name,
        "wt_path": wt_path,
    }
    return BaseResolution(canonical=base_branch, start_point=base_branch, detail="fake")

  class FakeWorker:

    def __init__(
        self,
        thread_metadata: ThreadMetadata,
        working_dir: Path,
        events_log_path: Path,
        task_description: str,
        worker_cfg: CharlieBotConfig,
        backend_option: Optional[BackendOption] = None,
        extra_env: Optional[dict[str, str]] = None,
        on_spawned: Optional[callable] = None,
        instructions_content: Optional[str] = None,
    ) -> None:
      captures["worker_dir"] = working_dir
      captures["worker_backend"] = backend_option

    async def run(self) -> int:
      return 0

    async def terminate(self) -> None:
      return None

  async def fake_notify_completion(
      session_id: str,
      description: str,
      thread_meta: ThreadMetadata,
      exit_code: int,
      thread_mgr: Any,
      session_mgr: Any,
      notify_cfg: CharlieBotConfig,
      quota_exhausted: bool = False,
      error: str = "",
  ) -> None:
    captures["notify_exit_code"] = exit_code

  monkeypatch = pytest.MonkeyPatch()
  monkeypatch.setattr(spawner, "git_current_branch", fake_git_current_branch)
  monkeypatch.setattr(spawner, "git_create_worktree", fake_git_create_worktree)
  monkeypatch.setattr(spawner, "Worker", FakeWorker)
  monkeypatch.setattr(spawner, "_notify_completion", fake_notify_completion)

  await spawner.spawn_worker(
      session_id="session-id",
      description="Do work",
      thread_id="thread-1",
      cfg=cfg,
      session_mgr=FakeSessionManager(),
      thread_mgr=FakeThreadManager(),
      request=SpawnRequest(
          repo_path=str(repo_path),
          base_branch="main",
          resolved_backend="codex-o3",
          resolved_model="o3-pro",
      ),
  )
  monkeypatch.undo()

  assert "git_create_worktree" in captures
  assert captures["worker_dir"] == captures["git_create_worktree"]["wt_path"].resolve()
  assert captures["worker_dir"] != repo_path
  assert thread.worktree_path == str(captures["git_create_worktree"]["wt_path"])
  assert thread.base_branch == "main"


@pytest.mark.asyncio
async def test_finalize_worker_preserves_thread_dir_for_repoless_worker(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = _build_cfg()
  thread_dir = tmp_path / "sessions" / "session-id" / "threads" / "thread-1"
  thread_dir.mkdir(parents=True)
  (thread_dir / "artifact.txt").write_text("tmp", encoding="utf-8")

  thread = ThreadMetadata(
      id="thread-1",
      session_id="session-id",
      description="Prompt-only task",
      worktree_path=str(thread_dir),
      require_review=True,
  )
  captures: dict[str, Any] = {}

  class FakeThreadManager(JudgmentShim):

    async def get_thread(self, session_id: str, thread_id: str) -> ThreadMetadata:
      del session_id, thread_id
      return thread

    async def update_status(
        self,
        session_id: str,
        thread_id: str,
        status: Any,
        pid: Optional[int] = None,
        exit_code: Optional[int] = None,
        completed_at: Any = None,
    ) -> None:
      captures["status"] = status
      captures["exit_code"] = exit_code

  async def fake_notify_completion(
      session_id: str,
      description: str,
      thread_meta: ThreadMetadata,
      exit_code: int,
      thread_mgr: Any,
      session_mgr: Any,
      notify_cfg: CharlieBotConfig,
      quota_exhausted: bool = False,
      error: str = "",
  ) -> None:
    captures["notified"] = True

  monkeypatch.setattr(spawner, "_notify_completion", fake_notify_completion)

  await spawner._finalize_worker(
      session_id="session-id",
      description="Prompt-only task",
      thread=thread,
      exit_code=0,
      thread_mgr=FakeThreadManager(),
      session_mgr=object(),
      cfg=cfg,
  )

  assert captures["status"] == ThreadStatus.COMPLETED
  assert captures["exit_code"] == 0
  assert captures["notified"] is True
  assert thread_dir.exists()


@pytest.mark.asyncio
async def test_spawn_review_worker_propagates_backend_model(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  cfg = _build_cfg()
  captured: dict[str, Any] = {}
  saved_review_thread: dict[str, ThreadMetadata] = {}
  repo_path = tmp_path / "repo"
  worktree_path = tmp_path / "worktrees" / "charliebot-task-1"
  repo_path.mkdir()
  worktree_path.mkdir(parents=True)

  class FakeSessionManager(JudgmentShim):

    async def get_session(self, session_id: str) -> Optional[SessionMetadata]:
      return SessionMetadata(id=session_id, name="Scheduled: nightly", backend="claude-opus-4.6")

  class FakeThreadManager(JudgmentShim):

    async def create_thread(
        self,
        session_meta: SessionMetadata,
        description: str,
        branch_name: Optional[str] = None,
        review_of: Optional[str] = None,
    ) -> ThreadMetadata:
      return ThreadMetadata(
          id="review-thread-id",
          session_id=session_meta.id,
          description=description,
          branch_name=branch_name,
          review_of=review_of,
      )

    async def save_metadata(self, meta: ThreadMetadata) -> None:
      saved_review_thread["meta"] = meta

  async def fake_git_current_branch(repo_path: Path) -> str:
    return "main"

  async def fake_spawn_worker(
      session_id: str,
      description: str,
      thread_id: str,
      cfg: CharlieBotConfig,
      session_mgr: Any,
      thread_mgr: Any,
      request: Optional[SpawnRequest] = None,
  ) -> None:
    return None

  def fake_create_logged_task(coro: Any, *, name: Optional[str] = None) -> Any:
    if coro.cr_frame is not None:
      captured.update(coro.cr_frame.f_locals)
    coro.close()

    class DummyTask:

      def add_done_callback(self, cb: Any) -> None:
        pass

    return DummyTask()

  monkeypatch.setattr(review, "git_current_branch", fake_git_current_branch)
  monkeypatch.setattr(spawner, "spawn_worker", fake_spawn_worker)
  monkeypatch.setattr(review, "create_logged_task", fake_create_logged_task)

  original = ThreadMetadata(
      id="origin-thread-id",
      session_id="session-id",
      description="Do work",
      branch_name="charliebot/task-1",
      base_branch="main",
      repo_path=str(repo_path),
      worktree_path=str(worktree_path),
      backend="codex-o3",
      model="o3-pro",
  )

  await review.spawn_review_worker(
      "session-id",
      original,
      cfg,
      FakeSessionManager(),
      FakeThreadManager(),
  )

  assert captured["request"].repo_path == str(repo_path)
  assert captured["request"].prompt_override is not None
  assert captured["request"].resolved_backend == "codex-o3"
  assert captured["request"].resolved_model == "o3-pro"
  assert saved_review_thread["meta"].worktree_path == str(worktree_path)


@pytest.mark.asyncio
async def test_spawn_review_worker_fails_if_backend_model_missing(tmp_path: Path) -> None:
  cfg = _build_cfg()
  repo_path = tmp_path / "repo"
  worktree_path = tmp_path / "worktrees" / "charliebot-task-1"
  repo_path.mkdir()
  worktree_path.mkdir(parents=True)

  class FakeSessionManager(JudgmentShim):

    async def get_session(self, session_id: str) -> Optional[SessionMetadata]:
      return SessionMetadata(id=session_id, name="Scheduled: nightly", backend="claude-opus-4.6")

  class FakeThreadManager(JudgmentShim):

    async def create_thread(
        self,
        session_meta: SessionMetadata,
        description: str,
        branch_name: Optional[str] = None,
        review_of: Optional[str] = None,
    ) -> ThreadMetadata:
      return ThreadMetadata(
          id="review-thread-id",
          session_id=session_meta.id,
          description=description,
          branch_name=branch_name,
          review_of=review_of,
      )

  async def fake_git_current_branch(repo_path: Path) -> str:
    return "main"

  monkeypatch = pytest.MonkeyPatch()
  monkeypatch.setattr(review, "git_current_branch", fake_git_current_branch)

  original = ThreadMetadata(
      id="origin-thread-id",
      session_id="session-id",
      description="Do work",
      branch_name="charliebot/task-1",
      base_branch="main",
      repo_path=str(repo_path),
      worktree_path=str(worktree_path),
      backend="codex-o3",
      model=None,
  )

  with pytest.raises(ValueError, match="missing model metadata"):
    await review.spawn_review_worker(
        "session-id",
        original,
        cfg,
        FakeSessionManager(),
        FakeThreadManager(),
    )
  monkeypatch.undo()


@pytest.mark.asyncio
@pytest.mark.parametrize("task_type", [TaskType.IMPLEMENT, TaskType.QUICK_EDIT, TaskType.SCRIPT_RUN])
async def test_create_repoless_non_verify_profiles_propagate_antigravity_and_keep_prompt(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    task_type: TaskType,
) -> None:
  cfg = CharlieBotConfig(
      charliebot_home=tmp_path / "charliebot-home",
      worktree_dir=str(tmp_path / "worktrees"),
      backend_options=[
          BackendOption(id="agy", label="Antigravity", type="antigravity"),
      ],
  )
  thread = ThreadMetadata(
      id="thread-1",
      session_id="session-id",
      description="Prompt task",
  )
  captures: dict[str, Any] = {}

  class FakeThreadManager(JudgmentShim):

    async def save_metadata(self, meta: ThreadMetadata) -> None:
      captures["saved_thread"] = meta

    async def get_events_log_path(self, session_id: str, thread_id: str) -> Path:
      return tmp_path / "events.jsonl"

  class FakeWorker:

    def __init__(
        self,
        thread_metadata: ThreadMetadata,
        working_dir: Path,
        events_log_path: Path,
        task_description: str,
        worker_cfg: CharlieBotConfig,
        backend_option: Optional[BackendOption] = None,
        extra_env: Optional[dict[str, str]] = None,
        on_spawned: Optional[callable] = None,
        instructions_content: Optional[str] = None,
    ) -> None:
      captures["backend_option"] = backend_option
      captures["task_description"] = task_description

  monkeypatch.setattr(spawner, "Worker", FakeWorker)

  await spawner._create_repoless_process(
      "session-id",
      thread,
      "Prompt task",
      cfg,
      FakeThreadManager(),
      SpawnRequest(resolved_backend="agy", task_type=task_type),
  )

  assert thread.backend == "agy"
  assert thread.model is None
  assert captures["backend_option"].id == "agy"
  assert captures["backend_option"].model is None
  assert captures["task_description"] == "Prompt task"


@pytest.mark.asyncio
async def test_create_repoless_worker_assigns_claude_session_id(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = CharlieBotConfig(
      charliebot_home=tmp_path / "charliebot-home",
      worktree_dir=str(tmp_path / "worktrees"),
      backend_options=[
          BackendOption(id="claude-opus", label="Claude", type="cc-claude", model="claude-opus-4-8"),
      ],
  )
  thread = ThreadMetadata(
      id="thread-1",
      session_id="session-id",
      description="Prompt task",
  )
  captures: dict[str, Any] = {}

  class FakeThreadManager(JudgmentShim):

    async def save_metadata(self, meta: ThreadMetadata) -> None:
      captures["saved_thread"] = meta

    async def get_events_log_path(self, session_id: str, thread_id: str) -> Path:
      return tmp_path / "events.jsonl"

  class FakeWorker:

    def __init__(
        self,
        thread_metadata: ThreadMetadata,
        working_dir: Path,
        events_log_path: Path,
        task_description: str,
        worker_cfg: CharlieBotConfig,
        backend_option: Optional[BackendOption] = None,
        extra_env: Optional[dict[str, str]] = None,
        on_spawned: Optional[callable] = None,
        instructions_content: Optional[str] = None,
    ) -> None:
      captures["backend_option"] = backend_option

  monkeypatch.setattr(spawner, "Worker", FakeWorker)

  await spawner._create_repoless_process(
      "session-id",
      thread,
      "Prompt task",
      cfg,
      FakeThreadManager(),
      SpawnRequest(resolved_backend="claude-opus", resolved_model="claude-opus-4-8"),
  )

  assert thread.claude_session_id is not None
  assert "--session-id" in (thread.cli_command or "")
  assert thread.claude_session_id in (thread.cli_command or "")
  assert captures["saved_thread"].claude_session_id == thread.claude_session_id
  assert captures["backend_option"].type == "cc-claude"


@pytest.mark.asyncio
async def test_create_repoless_worker_prepends_verify_preamble(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = CharlieBotConfig(
      charliebot_home=tmp_path / "charliebot-home",
      worktree_dir=str(tmp_path / "worktrees"),
      backend_options=[
          BackendOption(id="codex-o3", label="Codex", type="codex", model="o3"),
      ],
  )
  thread = ThreadMetadata(
      id="thread-1",
      session_id="session-id",
      description="Check the plan",
      require_review=True,
  )
  captures: dict[str, Any] = {}

  class FakeThreadManager(JudgmentShim):

    async def save_metadata(self, meta: ThreadMetadata) -> None:
      captures["saved_thread"] = meta

    async def get_events_log_path(self, session_id: str, thread_id: str) -> Path:
      return tmp_path / "events.jsonl"

  class FakeWorker:

    def __init__(
        self,
        thread_metadata: ThreadMetadata,
        working_dir: Path,
        events_log_path: Path,
        task_description: str,
        worker_cfg: CharlieBotConfig,
        backend_option: Optional[BackendOption] = None,
        extra_env: Optional[dict[str, str]] = None,
        on_spawned: Optional[callable] = None,
        instructions_content: Optional[str] = None,
    ) -> None:
      captures["worker_dir"] = working_dir
      captures["task_description"] = task_description

  monkeypatch.setattr(spawner, "Worker", FakeWorker)

  await spawner._create_repoless_process(
      "session-id",
      thread,
      "Check claim A at /tmp/repo/file.py:10",
      cfg,
      FakeThreadManager(),
      SpawnRequest(resolved_backend="codex-o3", resolved_model="o3", task_type=TaskType.VERIFY),
  )

  prompt = captures["task_description"]
  canonical_template_path = (cfg.charlie_bot_repo / "prompts" / "plan_template.html").resolve()
  assert prompt.startswith("You are a read-only plan verifier.")
  preamble, plan_instructions = prompt.split("Verification scope:\n", maxsplit=1)
  for contract_layer in (preamble, plan_instructions):
    assert "allowed local and network reads" in contract_layer
    assert "already-available tools, commands, connectivity, and credentials" in contract_layer
    assert "web search/fetch" in contract_layer
    assert "read-only API queries" in contract_layer
    assert "read-only SSH commands" in contract_layer
    assert "semantic read-only behavior, not a transport or HTTP-method allowlist" in contract_layer
    assert "mutate local or external state" in contract_layer
    assert "create" in contract_layer
    assert "update" in contract_layer
    assert "delete" in contract_layer
    assert "trigger" in contract_layer
    assert "submit" in contract_layer
    assert "upload" in contract_layer
    assert "message" in contract_layer
    assert "file edit" in contract_layer
    assert "Git write" in contract_layer
    assert "job submission" in contract_layer
    assert "reasonable allowed local or network reads cannot access the evidence" in contract_layer
    assert "verification would require state mutation" in contract_layer
    assert "Network access alone never makes a claim `unverifiable`" in contract_layer
  assert "never attempt it" not in prompt
  assert "forbidden network access" not in prompt
  assert "verdict exactly one of `confirmed` / `mismatch` / `mismatch-approval` / `unverifiable`" in prompt
  assert "`mismatch-approval` is a mismatch that invalidates a term of the approval object (the plan's 4.1 Schema, resolved Trade-offs, or promoted Other Details entries)." in prompt
  assert "`RESULT: clean`, `RESULT: 2 mismatches (1 approval)`, and `RESULT: 1 mismatch (0 approval)`" in prompt
  assert "This report format is fixed by the harness and overrides any output format the task spec requests; a task spec may add checks or scope, never change the report format." in prompt
  assert str(canonical_template_path) in prompt
  assert "Check exactly the scope the task spec declares. A spec that declares neither scope is verified as full." in prompt
  assert "Full verification (the spec declares full)" in prompt
  artifact_only_index = prompt.index("artifact-only standalone-comprehension pass")
  anchors_index = prompt.index("then read the canonical plan template")
  assert artifact_only_index < anchors_index
  assert "check the plan against every canonical rule in the template's BLOCK KIT" in prompt
  assert "Delta verification (the spec declares delta)" in prompt
  assert "check exactly the declared terms, their dependent claims, prior mismatches (including whether previously reported findings are closed), and document structure" in prompt
  assert "- Adequacy (full scope only)" in prompt
  assert "unchanged content keeps its verdict" in prompt
  assert "is verified as full" in prompt
  assert "`scenario:` — the counterexample you built" in prompt
  assert "Adequacy findings use the labelled block form defined in the plan scope block" in prompt
  assert "missing or unreadable plan artifact or canonical template" in prompt
  assert "missing required in-scope source anchor" in prompt
  assert "in-scope canonical-rule deviation as `mismatch`" in prompt
  assert "Preserve `unverifiable` only" in prompt
  assert prompt.endswith("Check claim A at /tmp/repo/file.py:10")
  assert thread.require_review is False
  assert thread.repo_path is None
  assert thread.branch_name is None
  assert thread.tried_backends == ["codex-o3"]
  assert captures["worker_dir"] == cfg.sessions_dir / "session-id" / "threads" / "thread-1"


@pytest.mark.asyncio
async def test_spawn_worker_repoless_disables_review_and_uses_thread_dir(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(
      charliebot_home=tmp_path / "charliebot-home",
      worktree_dir=str(tmp_path / "worktrees"),
      backend_options=[
          BackendOption(id="codex-o3", label="Codex", type="codex", model="o3"),
      ],
  )
  events_log = tmp_path / "events.jsonl"
  thread = ThreadMetadata(
      id="thread-1",
      session_id="session-id",
      description="Prompt task",
      require_review=True,
  )
  captures: dict[str, Any] = {}

  class FakeSessionManager(JudgmentShim):

    async def get_session(self, session_id: str) -> Any:
      return SessionMetadata(id=session_id, name="Test Session")

    async def save_chat_event(self, session_id: str, event: dict[str, Any]) -> None:
      captures["chat_event"] = event

    async def persist_and_broadcast(self, session_id: str, event: dict[str, Any]) -> None:
      captures["broadcast_event"] = event

  class FakeThreadManager(JudgmentShim):

    async def get_thread(self, session_id: str, thread_id: str) -> Optional[ThreadMetadata]:
      return thread

    async def save_metadata(self, meta: ThreadMetadata) -> None:
      captures["saved_thread"] = meta

    async def get_events_log_path(self, session_id: str, thread_id: str) -> Path:
      return events_log

    async def update_status(
        self,
        session_id: str,
        thread_id: str,
        status: Any,
        pid: Optional[int] = None,
        exit_code: Optional[int] = None,
        completed_at: Any = None,
    ) -> None:
      captures["status"] = status
      captures["exit_code"] = exit_code

  class FakeWorker:

    def __init__(
        self,
        thread_metadata: ThreadMetadata,
        working_dir: Path,
        events_log_path: Path,
        task_description: str,
        worker_cfg: CharlieBotConfig,
        backend_option: Optional[BackendOption] = None,
        extra_env: Optional[dict[str, str]] = None,
        on_spawned: Optional[callable] = None,
        instructions_content: Optional[str] = None,
    ) -> None:
      captures["worker_dir"] = working_dir
      captures["worker_backend"] = backend_option

    async def run(self) -> int:
      return 0

    async def terminate(self) -> None:
      return None

  async def fake_notify_completion(
      session_id: str,
      description: str,
      thread_meta: ThreadMetadata,
      exit_code: int,
      thread_mgr: Any,
      session_mgr: Any,
      notify_cfg: CharlieBotConfig,
      quota_exhausted: bool = False,
      error: str = "",
  ) -> None:
    captures["notify_exit_code"] = exit_code
    captures["notify_require_review"] = thread_meta.require_review
    captures["notify_repo_path"] = thread_meta.repo_path
    captures["notify_worktree_path"] = thread_meta.worktree_path
    captures["notify_branch_name"] = thread_meta.branch_name

  monkeypatch = pytest.MonkeyPatch()
  monkeypatch.setattr(spawner, "Worker", FakeWorker)
  monkeypatch.setattr(spawner, "_notify_completion", fake_notify_completion)

  await spawner.spawn_worker(
      session_id="session-id",
      description="Prompt task",
      thread_id="thread-1",
      cfg=cfg,
      session_mgr=FakeSessionManager(),
      thread_mgr=FakeThreadManager(),
      request=SpawnRequest(
          resolved_backend="codex-o3",
          resolved_model="o3-pro",
      ),
  )
  monkeypatch.undo()

  expected_thread_dir = cfg.sessions_dir / "session-id" / "threads" / "thread-1"
  worker_dir = captures["worker_dir"]
  assert worker_dir == expected_thread_dir
  assert captures["notify_exit_code"] == 0
  assert captures["notify_require_review"] is False
  assert captures["notify_repo_path"] is None
  assert captures["notify_worktree_path"] == str(worker_dir)
  assert captures["notify_branch_name"] is None
