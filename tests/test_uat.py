"""
User Acceptance Tests for CharlieBot API.

Tests every major user flow through the live ASGI app using Starlette's
TestClient (no mock HTTP layer — the full FastAPI stack runs).

LLM providers are mocked so no API keys are required.
The APScheduler is replaced with a MagicMock so the server can be
restarted cleanly between tests.
"""

import io
import json
import os
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
from starlette.testclient import TestClient

# Import the server module once at collection time
import server as server_mod


# ---------------------------------------------------------------------------
# SSE helpers
# ---------------------------------------------------------------------------

def _respond_json(text: str) -> str:
    return json.dumps({"action": "respond", "message": text})


def _delegate_json(description: str, priority: str = "P1", plan_mode: bool = False) -> str:
    return json.dumps({
        "action": "delegate",
        "priority": priority,
        "description": description,
        "plan_mode": plan_mode,
    })


def _parse_sse(text: str) -> list[dict]:
    events = []
    for line in text.splitlines():
        if line.startswith("data: "):
            try:
                events.append(json.loads(line[6:]))
            except json.JSONDecodeError:
                pass
    return events


# ---------------------------------------------------------------------------
# App fixture
# ---------------------------------------------------------------------------

@pytest.fixture()
def app_client(tmp_path):
    """
    Boot the full FastAPI app per-test with:
    - Isolated tmp charliebot home
    - Mocked APScheduler (avoids event-loop reuse issues between tests)
    - Mocked Gemini + Kimi LLM providers
    - All dep-injection singletons reset
    """
    home = tmp_path / ".charliebot"
    home.mkdir()

    import src.core.config as cfg_mod
    import src.api.deps as deps_mod

    # Reset singletons before
    cfg_mod._config = None
    for attr in ("_git_manager", "_memory_manager", "_session_manager",
                 "_thread_manager", "_master_agent"):
        setattr(deps_mod, attr, None)

    # LLM mocks — default: conversational reply
    async def _stream(messages, system):
        yield _respond_json("Hello! How can I help you today?")

    mock_gemini = MagicMock()
    mock_gemini.complete_streaming = _stream
    mock_gemini.complete = AsyncMock(return_value=_respond_json("Hello!"))
    mock_gemini.transcribe_audio = AsyncMock(return_value="transcribed text")

    mock_kimi = MagicMock()
    mock_kimi.complete = AsyncMock(return_value=_respond_json("Fallback"))
    mock_kimi.model_name = "kimi"

    with (
        patch.dict(os.environ, {"CHARLIEBOT_HOME": str(home)}),
        patch("src.agents.master_agent.GeminiProvider", return_value=mock_gemini),
        patch("src.agents.master_agent.KimiProvider", return_value=mock_kimi),
        # Replace the module-level scheduler so it doesn't try to reuse an
        # event loop across TestClient instances
        patch.object(server_mod, "_scheduler", MagicMock()),
    ):
        with TestClient(server_mod.app, raise_server_exceptions=True) as client:
            client._mock_gemini = mock_gemini
            client._mock_kimi = mock_kimi
            yield client

    # Reset singletons after
    cfg_mod._config = None
    for attr in ("_git_manager", "_memory_manager", "_session_manager",
                 "_thread_manager", "_master_agent"):
        setattr(deps_mod, attr, None)


# ---------------------------------------------------------------------------
# Convenience helpers
# ---------------------------------------------------------------------------

def _create_session(client, name="uat-session", **kwargs):
    r = client.post("/api/sessions/", json={"name": name, **kwargs})
    assert r.status_code == 200, r.text
    return r.json()


def _send_message(client, session_id: str, content: str) -> list[dict]:
    """Post a chat message and return the parsed SSE events."""
    with client.stream(
        "POST",
        f"/api/chat/{session_id}/message",
        json={"content": content},
    ) as r:
        assert r.status_code == 200
        body = r.read().decode()
    return _parse_sse(body)


# ===========================================================================
# UAT-1: Session lifecycle
# ===========================================================================

class TestSessionLifecycle:
    def test_list_sessions_starts_empty(self, app_client):
        r = app_client.get("/api/sessions/")
        assert r.status_code == 200
        assert r.json() == []

    def test_create_session(self, app_client):
        s = _create_session(app_client, name="my-project")
        assert s["name"] == "my-project"
        assert s["status"] == "active"
        assert s["base_branch"] == "main"
        assert "id" in s

    def test_create_session_custom_branch(self, app_client):
        s = _create_session(app_client, name="feat", base_branch="develop")
        assert s["base_branch"] == "develop"

    def test_get_session_by_id(self, app_client):
        s = _create_session(app_client, name="get-me")
        r = app_client.get(f"/api/sessions/{s['id']}")
        assert r.status_code == 200
        assert r.json()["id"] == s["id"]

    def test_get_unknown_session_returns_404(self, app_client):
        r = app_client.get("/api/sessions/does-not-exist")
        assert r.status_code == 404

    def test_list_sessions_shows_created(self, app_client):
        s1 = _create_session(app_client, name="alpha")
        s2 = _create_session(app_client, name="beta")
        ids = [s["id"] for s in app_client.get("/api/sessions/").json()]
        assert s1["id"] in ids and s2["id"] in ids

    def test_list_sessions_newest_first(self, app_client):
        import time
        s1 = _create_session(app_client, name="first")
        time.sleep(0.01)
        s2 = _create_session(app_client, name="second")
        sessions = app_client.get("/api/sessions/").json()
        assert sessions[0]["id"] == s2["id"]

    def test_archive_session(self, app_client):
        s = _create_session(app_client, name="to-archive")
        r = app_client.delete(f"/api/sessions/{s['id']}")
        assert r.status_code == 200
        assert r.json()["ok"] is True
        updated = app_client.get(f"/api/sessions/{s['id']}").json()
        assert updated["status"] == "archived"

    def test_archive_unknown_session_returns_404(self, app_client):
        r = app_client.delete("/api/sessions/ghost-session")
        assert r.status_code == 404


# ===========================================================================
# UAT-2: Task queue management
# ===========================================================================

class TestQueueManagement:
    def test_empty_queue_on_new_session(self, app_client):
        s = _create_session(app_client, name="queue-test")
        r = app_client.get(f"/api/sessions/{s['id']}/queue")
        assert r.status_code == 200
        data = r.json()
        assert data["tasks"] == []
        assert data["session_id"] == s["id"]

    def test_cancel_nonexistent_task_is_noop(self, app_client):
        s = _create_session(app_client, name="cancel-noop")
        r = app_client.delete(f"/api/sessions/{s['id']}/queue/nonexistent-id")
        assert r.status_code == 200
        assert r.json()["ok"] is True

    def test_reorder_nonexistent_task_is_noop(self, app_client):
        s = _create_session(app_client, name="reorder-noop")
        r = app_client.post(
            f"/api/sessions/{s['id']}/queue/reorder",
            json={"task_id": "fake-id", "priority": "P0"},
        )
        assert r.status_code == 200
        assert r.json()["ok"] is True


# ===========================================================================
# UAT-3: Chat — conversational response
# ===========================================================================

class TestChatConversational:
    def test_chat_returns_sse_stream(self, app_client):
        s = _create_session(app_client, name="chat-sse")
        events = _send_message(app_client, s["id"], "Hello!")
        types = [e["type"] for e in events]
        assert "chunk" in types
        assert "done" in types

    def test_chat_chunk_contains_response_text(self, app_client):
        s = _create_session(app_client, name="chat-text")
        events = _send_message(app_client, s["id"], "Say hi")
        chunks = "".join(e["content"] for e in events if e["type"] == "chunk")
        assert len(chunks) > 0

    def test_chat_persists_to_history(self, app_client):
        s = _create_session(app_client, name="chat-history")
        _send_message(app_client, s["id"], "Remember this please")
        history = app_client.get(f"/api/chat/{s['id']}/history").json()
        assert any("Remember this" in m["content"] for m in history["messages"] if m["role"] == "user")
        assert any(m["role"] == "assistant" for m in history["messages"])

    def test_chat_with_unknown_session_returns_404(self, app_client):
        r = app_client.post("/api/chat/no-such-session/message", json={"content": "hi"})
        assert r.status_code == 404

    def test_get_history_unknown_session_returns_404(self, app_client):
        r = app_client.get("/api/chat/no-such-session/history")
        assert r.status_code == 404

    def test_get_history_empty_on_new_session(self, app_client):
        s = _create_session(app_client, name="empty-history")
        r = app_client.get(f"/api/chat/{s['id']}/history")
        assert r.status_code == 200
        assert r.json()["messages"] == []

    def test_multiple_chat_turns_accumulate_in_history(self, app_client):
        s = _create_session(app_client, name="multi-turn")
        for msg in ["Hello", "How are you?", "What can you do?"]:
            _send_message(app_client, s["id"], msg)
        history = app_client.get(f"/api/chat/{s['id']}/history").json()
        user_msgs = [m for m in history["messages"] if m["role"] == "user"]
        assert len(user_msgs) == 3


# ===========================================================================
# UAT-4: Chat — task delegation
# ===========================================================================

class TestChatDelegation:
    def test_delegate_creates_task_in_queue(self, app_client):
        s = _create_session(app_client, name="delegate-test")

        async def _del(messages, system):
            yield _delegate_json("Fix the login bug", priority="P1")

        app_client._mock_gemini.complete_streaming = _del
        events = _send_message(app_client, s["id"], "Fix the login bug")

        del_events = [e for e in events if e["type"] == "task_delegated"]
        assert len(del_events) == 1
        assert del_events[0]["priority"] == "P1"
        assert "login bug" in del_events[0]["description"].lower()

        tasks = app_client.get(f"/api/sessions/{s['id']}/queue").json()["tasks"]
        assert len(tasks) == 1 and tasks[0]["status"] == "pending"

    def test_delegate_p0_urgent_task(self, app_client):
        s = _create_session(app_client, name="urgent")

        async def _urgent(messages, system):
            yield _delegate_json("Production outage fix", priority="P0")

        app_client._mock_gemini.complete_streaming = _urgent
        events = _send_message(app_client, s["id"], "Production is down!")
        del_ev = next(e for e in events if e["type"] == "task_delegated")
        assert del_ev["priority"] == "P0"

    def test_delegate_plan_mode_task(self, app_client):
        s = _create_session(app_client, name="plan-mode")

        async def _plan(messages, system):
            yield _delegate_json("Refactor auth module", priority="P1", plan_mode=True)

        app_client._mock_gemini.complete_streaming = _plan
        events = _send_message(app_client, s["id"], "Refactor auth please")
        del_ev = next(e for e in events if e["type"] == "task_delegated")
        assert del_ev["plan_mode"] is True

    def test_delegate_p2_background_task(self, app_client):
        s = _create_session(app_client, name="bg-task")

        async def _bg(messages, system):
            yield _delegate_json("Low-priority cleanup", priority="P2")

        app_client._mock_gemini.complete_streaming = _bg
        events = _send_message(app_client, s["id"], "Run cleanup in background")
        del_ev = next(e for e in events if e["type"] == "task_delegated")
        assert del_ev["priority"] == "P2"

    def test_cancel_queued_task(self, app_client):
        s = _create_session(app_client, name="cancel-queued")

        async def _del(messages, system):
            yield _delegate_json("Background refactoring", priority="P2")

        app_client._mock_gemini.complete_streaming = _del
        events = _send_message(app_client, s["id"], "Refactor in background")
        task_id = next(e for e in events if e["type"] == "task_delegated")["task_id"]

        r = app_client.delete(f"/api/sessions/{s['id']}/queue/{task_id}")
        assert r.status_code == 200

        tasks = app_client.get(f"/api/sessions/{s['id']}/queue").json()["tasks"]
        assert next(t for t in tasks if t["id"] == task_id)["status"] == "cancelled"

    def test_reorder_queued_task_to_p0(self, app_client):
        s = _create_session(app_client, name="reorder-queued")

        async def _del(messages, system):
            yield _delegate_json("Low priority cleanup", priority="P2")

        app_client._mock_gemini.complete_streaming = _del
        events = _send_message(app_client, s["id"], "Clean up old code")
        task_id = next(e for e in events if e["type"] == "task_delegated")["task_id"]

        app_client.post(
            f"/api/sessions/{s['id']}/queue/reorder",
            json={"task_id": task_id, "priority": "P0"},
        )

        tasks = app_client.get(f"/api/sessions/{s['id']}/queue").json()["tasks"]
        assert next(t for t in tasks if t["id"] == task_id)["priority"] == "P0"

    def test_multiple_delegations_respect_priority_order(self, app_client):
        s = _create_session(app_client, name="multi-delegate")

        async def _p2(messages, system):
            yield _delegate_json("background", priority="P2")

        async def _p0(messages, system):
            yield _delegate_json("urgent", priority="P0")

        async def _p1(messages, system):
            yield _delegate_json("normal", priority="P1")

        # Enqueue P2 first, then P0, then P1 — queue should reorder them
        for stream in [_p2, _p0, _p1]:
            app_client._mock_gemini.complete_streaming = stream
            _send_message(app_client, s["id"], "task")

        tasks = app_client.get(f"/api/sessions/{s['id']}/queue").json()["tasks"]
        pending = [t for t in tasks if t["status"] == "pending"]
        priorities = [t["priority"] for t in pending]
        assert priorities[0] == "P0"
        assert priorities[1] == "P1"
        assert priorities[2] == "P2"


# ===========================================================================
# UAT-5: Thread management
# ===========================================================================

class TestThreadManagement:
    def test_list_threads_empty_on_new_session(self, app_client):
        s = _create_session(app_client, name="thread-list")
        r = app_client.get(f"/api/sessions/{s['id']}/threads")
        assert r.status_code == 200
        assert r.json() == []

    def test_get_unknown_thread_returns_404(self, app_client):
        s = _create_session(app_client, name="thread-404")
        r = app_client.get(f"/api/threads/{s['id']}/threads/no-such-thread")
        assert r.status_code == 404

    def test_get_thread_events_returns_empty_list(self, app_client):
        s = _create_session(app_client, name="events-empty")
        # events endpoint returns [] when no events.jsonl exists (no thread check)
        r = app_client.get(f"/api/threads/{s['id']}/threads/fake-thread/events")
        assert r.status_code == 200
        assert r.json() == []

    def test_cancel_nonexistent_thread_returns_404(self, app_client):
        s = _create_session(app_client, name="cancel-thread-404")
        r = app_client.post(f"/api/threads/{s['id']}/threads/fake-thread/cancel")
        assert r.status_code == 404

    def test_approve_plan_nonexistent_thread_returns_404(self, app_client):
        s = _create_session(app_client, name="approve-404")
        r = app_client.post(
            f"/api/threads/{s['id']}/threads/fake-thread/approve-plan",
            json={"approved_steps": ["step 1", "step 2"]},
        )
        assert r.status_code == 404


# ===========================================================================
# UAT-6: Memory endpoints
# ===========================================================================

class TestMemoryEndpoints:
    def test_get_memory_content(self, app_client):
        r = app_client.get("/api/memory/memory")
        assert r.status_code == 200
        assert "MEMORY" in r.json()["content"]

    def test_get_progress_content(self, app_client):
        r = app_client.get("/api/memory/progress")
        assert r.status_code == 200
        assert "PROGRESS" in r.json()["content"]

    def test_search_past_tasks_empty(self, app_client):
        r = app_client.get("/api/memory/past-tasks/search?q=authentication")
        assert r.status_code == 200
        data = r.json()
        assert data["query"] == "authentication"
        assert isinstance(data["results"], list)

    def test_search_past_tasks_missing_query_returns_422(self, app_client):
        r = app_client.get("/api/memory/past-tasks/search")
        assert r.status_code == 422

    def test_search_past_tasks_limit_respected(self, app_client):
        r = app_client.get("/api/memory/past-tasks/search?q=task&limit=2")
        assert r.status_code == 200
        assert len(r.json()["results"]) <= 2

    def test_search_past_tasks_invalid_limit_returns_422(self, app_client):
        r = app_client.get("/api/memory/past-tasks/search?q=x&limit=100")
        assert r.status_code == 422  # limit max is 20


# ===========================================================================
# UAT-7: Voice transcription
# ===========================================================================

class TestVoiceTranscription:
    def test_empty_audio_returns_400(self, app_client):
        r = app_client.post(
            "/api/voice/transcribe",
            data={"session_id": "any"},
            files={"audio": ("test.webm", b"", "audio/webm")},
        )
        assert r.status_code == 400
        assert "Empty audio" in r.json()["detail"]

    def test_transcription_success_with_mocked_gemini(self, app_client):
        app_client._mock_gemini.transcribe_audio = AsyncMock(return_value="Hello world")
        r = app_client.post(
            "/api/voice/transcribe",
            data={"session_id": "any"},
            files={"audio": ("test.webm", b"fake-audio-bytes", "audio/webm")},
        )
        assert r.status_code == 200
        data = r.json()
        assert data["transcription"] == "Hello world"
        assert "disclaimer" in data

    def test_transcription_gemini_api_error_returns_500(self, app_client):
        app_client._mock_gemini.transcribe_audio = AsyncMock(
            side_effect=Exception("Gemini API error")
        )
        r = app_client.post(
            "/api/voice/transcribe",
            data={"session_id": "any"},
            files={"audio": ("test.webm", b"fake-audio-bytes", "audio/webm")},
        )
        assert r.status_code == 500
        assert "Transcription failed" in r.json()["detail"]

    def test_transcription_no_api_key_returns_503(self, app_client):
        app_client._mock_gemini.transcribe_audio = AsyncMock(
            side_effect=NotImplementedError("no Gemini key")
        )
        r = app_client.post(
            "/api/voice/transcribe",
            data={"session_id": "any"},
            files={"audio": ("test.webm", b"fake-audio-bytes", "audio/webm")},
        )
        assert r.status_code == 503


# ===========================================================================
# UAT-8: WebSocket — live event streaming
# ===========================================================================

class TestWebSocket:
    def test_websocket_catchup_complete_for_unknown_thread(self, app_client):
        with app_client.websocket_connect("/ws/threads/nonexistent-thread") as ws:
            data = ws.receive_json()
            assert data["type"] == "catchup_complete"

    def test_websocket_replays_historical_events(self, app_client):
        import src.core.config as cfg_mod
        cfg = cfg_mod.get_config()

        thread_id = "test-thread-replay"
        events_dir = cfg.sessions_dir / "sess-ws" / "threads" / thread_id / "data"
        events_dir.mkdir(parents=True, exist_ok=True)
        (events_dir / "events.jsonl").write_text(
            '{"type": "output", "content": "line1"}\n'
            '{"type": "output", "content": "line2"}\n'
        )

        with app_client.websocket_connect(f"/ws/threads/{thread_id}") as ws:
            received = []
            for _ in range(10):
                msg = ws.receive_json()
                received.append(msg)
                if msg.get("type") == "catchup_complete":
                    break

        types = [m["type"] for m in received]
        assert "catchup_complete" in types
        # Two historical events must precede catchup_complete
        assert types.index("catchup_complete") >= 2

    def test_websocket_client_can_send_ping(self, app_client):
        with app_client.websocket_connect("/ws/threads/ping-test") as ws:
            catchup = ws.receive_json()
            assert catchup["type"] == "catchup_complete"
            ws.send_text("ping")  # Server silently ignores client pings


# ===========================================================================
# UAT-9: OpenAPI / docs
# ===========================================================================

class TestOpenAPI:
    def test_openapi_schema_accessible(self, app_client):
        r = app_client.get("/openapi.json")
        assert r.status_code == 200
        schema = r.json()
        assert schema["info"]["title"] == "CharlieBot"
        assert schema["info"]["version"] == "0.1.0"

    def test_swagger_ui_accessible(self, app_client):
        r = app_client.get("/docs")
        assert r.status_code == 200

    def test_all_api_prefixes_present_in_schema(self, app_client):
        paths = app_client.get("/openapi.json").json()["paths"]
        for prefix in ["/api/sessions", "/api/chat", "/api/threads", "/api/voice", "/api/memory"]:
            assert any(p.startswith(prefix) for p in paths), f"No routes for {prefix}"
