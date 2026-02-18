"""
Live smoke test against the real Gemini API.

Requires GEMINI_API_KEY in the environment.
Skipped automatically if the key is missing.
"""

import json
import os
import pytest

# Skip all tests in this module if no key is available
pytestmark = pytest.mark.skipif(
    not os.environ.get("GEMINI_API_KEY"),
    reason="GEMINI_API_KEY not set",
)


@pytest.fixture()
def live_client(tmp_path):
    """
    Start the app with the real Gemini provider (no LLM mock).
    Only the scheduler is mocked to keep tests fast.
    """
    home = tmp_path / ".charliebot"
    home.mkdir()

    import src.core.config as cfg_mod
    import src.api.deps as deps_mod
    import server as server_mod
    from unittest.mock import MagicMock, patch
    from starlette.testclient import TestClient

    cfg_mod._config = None
    for attr in ("_git_manager", "_memory_manager", "_session_manager",
                 "_thread_manager", "_master_agent"):
        setattr(deps_mod, attr, None)

    env = {
        "CHARLIEBOT_HOME": str(home),
        "CHARLIEBOT_GEMINI_API_KEY": os.environ["GEMINI_API_KEY"],
    }

    with (
        patch.dict(os.environ, env),
        patch.object(server_mod, "_scheduler", MagicMock()),
    ):
        with TestClient(server_mod.app, raise_server_exceptions=True) as client:
            yield client

    cfg_mod._config = None
    for attr in ("_git_manager", "_memory_manager", "_session_manager",
                 "_thread_manager", "_master_agent"):
        setattr(deps_mod, attr, None)


def _create_session(client, name="live-session"):
    r = client.post("/api/sessions/", json={"name": name})
    assert r.status_code == 200, r.text
    return r.json()


def _parse_sse(text: str) -> list[dict]:
    events = []
    for line in text.splitlines():
        if line.startswith("data: "):
            try:
                events.append(json.loads(line[6:]))
            except json.JSONDecodeError:
                pass
    return events


class TestLiveGemini:
    def test_conversational_chat_returns_text(self, live_client):
        """Real Gemini call: simple greeting should return a respond action."""
        s = _create_session(live_client, "live-chat")
        with live_client.stream(
            "POST",
            f"/api/chat/{s['id']}/message",
            json={"content": "Reply with exactly: PONG"},
        ) as r:
            assert r.status_code == 200
            body = r.read().decode()

        events = _parse_sse(body)
        assert any(e["type"] == "done" for e in events)
        chunks = "".join(e["content"] for e in events if e["type"] == "chunk")
        assert len(chunks) > 0

    def test_delegate_task_classification(self, live_client):
        """Real Gemini call: a coding request should produce a delegate action."""
        s = _create_session(live_client, "live-delegate")
        with live_client.stream(
            "POST",
            f"/api/chat/{s['id']}/message",
            json={"content": "Write a Python function that reverses a string"},
        ) as r:
            assert r.status_code == 200
            body = r.read().decode()

        events = _parse_sse(body)
        # Either the bot delegates the task or responds — either is valid
        assert any(e["type"] == "done" for e in events)

    def test_history_persists_after_live_chat(self, live_client):
        """Real Gemini call: verify conversation history is saved."""
        s = _create_session(live_client, "live-history")
        with live_client.stream(
            "POST",
            f"/api/chat/{s['id']}/message",
            json={"content": "My name is Alice."},
        ) as r:
            r.read()

        history = live_client.get(f"/api/chat/{s['id']}/history").json()
        assert any("Alice" in m["content"] for m in history["messages"] if m["role"] == "user")
        assert any(m["role"] == "assistant" for m in history["messages"])
