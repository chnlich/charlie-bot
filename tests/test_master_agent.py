"""Tests for src/agents/master_agent.py (pure logic, no LLM calls)."""

import pytest
from unittest.mock import AsyncMock, MagicMock, patch

from src.agents.master_agent import MasterAgent, MASTER_SYSTEM_PROMPT, RECENT_TURNS_TO_KEEP
from src.core.models import (
    ChatMessage,
    ConversationHistory,
    MessageRole,
    Priority,
    Task,
)


def _make_agent(tmp_home):
    """Create a MasterAgent with mocked LLM providers and memory."""
    from src.core.config import load_config
    from src.core.memory import MemoryManager

    cfg = load_config()
    cfg.memory_file.write_text("# MEMORY\n\nUser prefers dark mode.\n", encoding="utf-8")
    memory = MemoryManager(cfg)

    with patch("src.agents.master_agent.GeminiProvider") as MockGemini, \
         patch("src.agents.master_agent.KimiProvider") as MockKimi:
        agent = MasterAgent(cfg, memory)

    return agent, memory


# ---------------------------------------------------------------------------
# _parse_response
# ---------------------------------------------------------------------------

class TestParseResponse:
    def setup_method(self):
        self.agent = MagicMock(spec=MasterAgent)
        self.parse = MasterAgent._parse_response.__get__(self.agent, MasterAgent)

    def test_valid_delegate_json(self):
        raw = '{"action": "delegate", "priority": "P1", "description": "fix bug", "plan_mode": false}'
        result = self.parse(raw)
        assert result["action"] == "delegate"
        assert result["priority"] == "P1"

    def test_valid_respond_json(self):
        raw = '{"action": "respond", "message": "Hello there!"}'
        result = self.parse(raw)
        assert result["action"] == "respond"
        assert result["message"] == "Hello there!"

    def test_markdown_fenced_json(self):
        raw = '```json\n{"action": "delegate", "priority": "P0", "description": "urgent"}\n```'
        result = self.parse(raw)
        assert result["action"] == "delegate"

    def test_plain_text_falls_back_to_respond(self):
        raw = "Sure, I can help with that!"
        result = self.parse(raw)
        assert result["action"] == "respond"
        assert result["message"] == raw

    def test_invalid_json_falls_back_to_respond(self):
        raw = "{not valid json at all"
        result = self.parse(raw)
        assert result["action"] == "respond"

    def test_json_without_action_key_falls_back(self):
        raw = '{"something": "else"}'
        result = self.parse(raw)
        assert result["action"] == "respond"

    def test_empty_string_falls_back_to_respond(self):
        result = self.parse("")
        assert result["action"] == "respond"

    def test_whitespace_stripped_before_parse(self):
        raw = '   {"action": "respond", "message": "hi"}   '
        result = self.parse(raw)
        assert result["action"] == "respond"


# ---------------------------------------------------------------------------
# _build_task_from_action
# ---------------------------------------------------------------------------

class TestBuildTaskFromAction:
    def setup_method(self):
        self.agent = MagicMock(spec=MasterAgent)
        self.build = MasterAgent._build_task_from_action.__get__(self.agent, MasterAgent)

    def test_p0_priority(self):
        action = {"priority": "P0", "description": "urgent fix", "plan_mode": False}
        task = self.build(action, "sess-1")
        assert task.priority == Priority.P0
        assert task.description == "urgent fix"
        assert task.is_plan_mode is False

    def test_p1_priority(self):
        action = {"priority": "P1", "description": "standard", "plan_mode": False}
        task = self.build(action, "sess-1")
        assert task.priority == Priority.P1

    def test_p2_priority(self):
        action = {"priority": "P2", "description": "background", "plan_mode": False}
        task = self.build(action, "sess-1")
        assert task.priority == Priority.P2

    def test_plan_mode_true(self):
        action = {"priority": "P1", "description": "complex task", "plan_mode": True}
        task = self.build(action, "sess-1")
        assert task.is_plan_mode is True

    def test_unknown_priority_defaults_to_p1(self):
        action = {"priority": "UNKNOWN", "description": "??", "plan_mode": False}
        task = self.build(action, "sess-1")
        assert task.priority == Priority.P1

    def test_missing_priority_defaults_to_p1(self):
        action = {"description": "no priority key", "plan_mode": False}
        task = self.build(action, "sess-1")
        assert task.priority == Priority.P1

    def test_returns_task_instance(self):
        action = {"priority": "P1", "description": "test", "plan_mode": False}
        task = self.build(action, "sess-1")
        assert isinstance(task, Task)


# ---------------------------------------------------------------------------
# _build_system_prompt
# ---------------------------------------------------------------------------

class TestBuildSystemPrompt:
    def setup_method(self):
        self.agent = MagicMock(spec=MasterAgent)
        self.build = MasterAgent._build_system_prompt.__get__(self.agent, MasterAgent)

    def test_base_prompt_always_included(self):
        result = self.build("", None)
        assert MASTER_SYSTEM_PROMPT in result

    def test_memory_included_when_non_empty(self):
        result = self.build("User prefers tabs", None)
        assert "User prefers tabs" in result
        assert "User Memory" in result

    def test_memory_not_included_when_empty(self):
        result = self.build("", None)
        assert "User Memory" not in result

    def test_memory_not_included_when_only_header(self):
        result = self.build("# MEMORY", None)
        assert "User Memory" not in result

    def test_summary_included_when_provided(self):
        result = self.build("", "Earlier we discussed authentication.")
        assert "Earlier we discussed authentication." in result
        assert "Conversation Summary" in result

    def test_summary_not_included_when_none(self):
        result = self.build("", None)
        assert "Conversation Summary" not in result

    def test_both_memory_and_summary(self):
        result = self.build("User likes Python", "We talked about refactoring.")
        assert "User likes Python" in result
        assert "We talked about refactoring." in result


# ---------------------------------------------------------------------------
# chat() — end-to-end with mocked LLM
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
class TestMasterAgentChat:
    async def test_chat_respond_action(self, tmp_home):
        agent, memory = _make_agent(tmp_home)
        agent._primary = MagicMock()
        agent._primary.complete = AsyncMock(
            return_value='{"action": "respond", "message": "Hello!"}'
        )
        agent._fallback = MagicMock()

        history = ConversationHistory(session_id="s")
        history, action = await agent.chat(history, "Hi there")

        assert action["action"] == "respond"
        assert action["message"] == "Hello!"
        assert len(history.messages) == 2  # user + assistant

    async def test_chat_delegate_action(self, tmp_home):
        agent, memory = _make_agent(tmp_home)
        agent._primary = MagicMock()
        agent._primary.complete = AsyncMock(
            return_value='{"action": "delegate", "priority": "P1", "description": "fix the bug", "plan_mode": false}'
        )
        agent._fallback = MagicMock()

        history = ConversationHistory(session_id="s")
        history, action = await agent.chat(history, "Fix the login bug")

        assert action["action"] == "delegate"
        assert action["priority"] == "P1"

    async def test_chat_voice_adds_disclaimer(self, tmp_home):
        agent, memory = _make_agent(tmp_home)
        captured = []

        async def capture_complete(messages, system):
            captured.append(messages)
            return '{"action": "respond", "message": "ok"}'

        agent._primary = MagicMock()
        agent._primary.complete = capture_complete
        agent._fallback = MagicMock()

        history = ConversationHistory(session_id="s")
        await agent.chat(history, "do something", is_voice=True)

        user_content = captured[0][0].content
        assert "voice-transcribed" in user_content.lower() or "Voice message" in user_content

    async def test_chat_uses_fallback_on_primary_failure(self, tmp_home):
        agent, memory = _make_agent(tmp_home)
        agent._primary = MagicMock()
        agent._primary.complete = AsyncMock(side_effect=Exception("API error"))
        agent._fallback = MagicMock()
        agent._fallback.complete = AsyncMock(
            return_value='{"action": "respond", "message": "fallback response"}'
        )
        agent._fallback.model_name = "kimi"

        history = ConversationHistory(session_id="s")
        history, action = await agent.chat(history, "hello")

        assert action["action"] == "respond"
        assert action["message"] == "fallback response"
        agent._fallback.complete.assert_awaited_once()
