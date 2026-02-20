"""Master Agent — orchestrates user interaction and task delegation."""

import json
import platform
from datetime import datetime, timezone
from typing import AsyncGenerator, Optional

import structlog

from src.agents.gemini_provider import GeminiProvider
from src.agents.kimi_provider import KimiProvider
from src.agents.llm_provider import LLMProvider
from src.core.config import CharliBotConfig
from src.core.memory import MemoryManager
from src.core.models import (
  ChatMessage,
  ConversationHistory,
  MessageRole,
  TaskDelegationResult,
)

log = structlog.get_logger()

SUMMARY_THRESHOLD = 10  # Compress history after this many turns
RECENT_TURNS_TO_KEEP = 5

VOICE_DISCLAIMER = (
  "This is a voice-transcribed message and may not be exactly accurate. "
  "Please ask clarifying questions if anything is unclear."
)

MASTER_SYSTEM_PROMPT = """You are CharlieBot's Master Agent — a hands-on assistant backed by Claude Code worker agents.

Your responsibilities:
1. Understand user requests and maintain conversation context
2. Handle small, quick tasks directly — you are NOT just a coordinator
3. Delegate substantial work to worker agents when needed
4. Classify delegated tasks by priority: P0 (immediate/interactive), P1 (standard feature/bug), P2 (background/refactoring)
5. Decide if a complex task needs plan mode (multi-step work requiring user review)
6. Review worker results and summarize for the user
7. Update memory when users reveal preferences or facts about themselves

## When to handle tasks yourself (action: "execute")
For small, quick tasks that a single command or a few commands can accomplish, do them yourself. Examples:
- Showing file contents ("show me server.py", "what's in config.yaml")
- Listing files/directories ("what files are in src/", "show project structure")
- Checking git status, log, diff, or branch info
- Running a quick shell command ("what's my Python version", "disk usage")
- Reading logs or checking process status
- Any read-only or quick informational task

For these, respond with:
{"action": "execute", "command": "the shell command to run"}

You can chain short commands with && if needed:
{"action": "execute", "command": "head -50 src/server.py"}

Keep execute commands simple and read-only when possible. Never use execute for destructive operations \
(rm -rf, git push --force, DROP TABLE, etc.) — delegate those instead so the worker can handle them safely.

## When to delegate to a worker agent
Delegate when the task requires substantial work: writing/editing code, multi-file changes, complex debugging, \
running test suites, building, deploying, or anything requiring multiple steps with judgment calls.

For standard delegated tasks:
{"action": "delegate", "priority": "P1", "description": "detailed task description", "plan_mode": false}

For P0 tasks (immediate/urgent or simple one-liners that still need a worker):
{"action": "delegate", "priority": "P0", "description": "...", "plan_mode": false}

For complex multi-step tasks requiring a planning phase:
{"action": "delegate", "priority": "P1", "description": "...", "plan_mode": true}

## When to respond directly (no action needed)
For purely conversational responses (greetings, questions about yourself, etc.):
{"action": "respond", "message": "your response here"}

## Direct Responses for System Metadata
You have access to live system metadata (current time, date, timezone, OS, platform, etc.) injected into \
your context under "## System Metadata" below. When the user asks simple informational questions that can \
be answered entirely from this metadata — such as "what time is it?", "what's today's date?", "what OS are \
we on?", or similar — respond DIRECTLY with {"action": "respond", "message": "..."} using the metadata. \
Do NOT delegate these to a worker agent.

When in doubt between execute and delegate, prefer execute for anything a single command can answer, \
and delegate for anything that requires writing or modifying code.

## Memory Updates
You have access to a persistent MEMORY.md shown above. Whenever the user reveals a preference, fact, or \
anything worth remembering long-term, add a memory_update field to ANY action you return:
{"action": "respond", "message": "...", "memory_update": "User prefers dark mode"}
{"action": "delegate", ..., "memory_update": "User works at Citadel"}
Only include memory_update when there is genuinely new, durable information not already in MEMORY.md. \
Write it as a concise, self-contained fact or bullet point.

When a worker finishes, summarize the results clearly and concisely.
"""


class MasterAgent:
  """Wraps LLM providers with fallback, conversation management, and task classification."""

  def __init__(self, cfg: CharliBotConfig, memory_manager: MemoryManager):
    self._primary: LLMProvider = GeminiProvider(cfg.gemini_api_key, cfg.gemini_model)
    self._fallback: LLMProvider = KimiProvider(cfg.kimi_api_key, cfg.kimi_base_url, cfg.kimi_model)
    self._memory = memory_manager

  async def chat(
    self,
    history: ConversationHistory,
    user_content: str,
    is_voice: bool = False,
  ) -> tuple[ConversationHistory, dict]:
    """
    Process a user message and return updated history + action dict.
    Action dict is one of:
    - {"action": "respond", "message": "..."}
    - {"action": "delegate", "priority": ..., "description": ..., "plan_mode": ...}
    """
    # Add voice disclaimer if applicable
    if is_voice:
      user_content = f"[Voice message — {VOICE_DISCLAIMER}]\n\n{user_content}"

    user_msg = ChatMessage(role=MessageRole.USER, content=user_content, is_voice=is_voice)
    history.messages.append(user_msg)

    # Summarize if history is too long
    if len(history.messages) > SUMMARY_THRESHOLD * 2:
      history = await self._summarize_history(history)

    # Build context: MEMORY.md + summary + recent turns
    memory_content = await self._memory.read_memory()
    system = self._build_system_prompt(memory_content, history.summary)

    # Call LLM with fallback
    raw_response = await self._call_with_fallback(history.messages, system)

    # Parse response
    action = self._parse_response(raw_response)

    # Append assistant response to history
    display_content = action.get("message", raw_response) if action.get("action") == "respond" else raw_response
    assistant_msg = ChatMessage(role=MessageRole.ASSISTANT, content=display_content)
    history.messages.append(assistant_msg)

    return history, action

  async def chat_streaming(
    self,
    history: ConversationHistory,
    user_content: str,
    is_voice: bool = False,
  ) -> AsyncGenerator[str, None]:
    """Stream assistant response chunks. Handles non-streaming delegation inline."""
    if is_voice:
      user_content = f"[Voice message — {VOICE_DISCLAIMER}]\n\n{user_content}"

    user_msg = ChatMessage(role=MessageRole.USER, content=user_content, is_voice=is_voice)
    history.messages.append(user_msg)

    if len(history.messages) > SUMMARY_THRESHOLD * 2:
      history = await self._summarize_history(history)

    memory_content = await self._memory.read_memory()
    system = self._build_system_prompt(memory_content, history.summary)

    full_response = ""
    async for chunk in self._stream_with_fallback(history.messages, system):
      full_response += chunk
      yield chunk

    assistant_msg = ChatMessage(role=MessageRole.ASSISTANT, content=full_response)
    history.messages.append(assistant_msg)

  async def review_worker_result(
    self,
    thread_description: str,
    events_summary: str,
  ) -> str:
    """Generate a user-friendly summary of a completed Worker's output."""
    messages = [
      ChatMessage(
        role=MessageRole.USER,
        content=(
          f"A worker just completed the following task:\n\n**{thread_description}**\n\n"
          f"Worker output summary:\n{events_summary}\n\n"
          "Please write a clear, concise summary for the user of what was accomplished."
        ),
      )
    ]
    return await self._call_with_fallback(messages, MASTER_SYSTEM_PROMPT)

  async def transcribe_audio(self, audio_bytes: bytes, mime_type: str) -> str:
    """Transcribe audio — always uses Gemini (Kimi doesn't support this)."""
    return await self._primary.transcribe_audio(audio_bytes, mime_type)

  # ---------------------------------------------------------------------------
  # Private helpers
  # ---------------------------------------------------------------------------

  @staticmethod
  def _get_system_metadata() -> str:
    """Return a snapshot of current system time, date, and platform info."""
    now_utc = datetime.now(timezone.utc)
    now_local = datetime.now().astimezone()
    tz_name = now_local.tzname() or "Unknown"
    return (
      f"- Current UTC time: {now_utc.strftime('%Y-%m-%d %H:%M:%S %Z')}\n"
      f"- Current local time: {now_local.strftime('%Y-%m-%d %H:%M:%S %Z')} ({tz_name})\n"
      f"- Platform: {platform.system()} {platform.release()}\n"
      f"- Architecture: {platform.machine()}\n"
      f"- Python version: {platform.python_version()}"
    )

  def _build_system_prompt(self, memory_content: str, summary: Optional[str]) -> str:
    parts = [MASTER_SYSTEM_PROMPT]

    # Inject live system metadata so the agent can answer time/date/env questions directly
    parts.append(f"\n## System Metadata\n{self._get_system_metadata()}")

    if memory_content.strip() and memory_content.strip() != "# MEMORY":
      parts.append(f"\n## User Memory\n{memory_content}")
    if summary:
      parts.append(f"\n## Conversation Summary (earlier turns)\n{summary}")
    return "\n".join(parts)

  def _parse_response(self, raw: str) -> dict:
    """Parse LLM response as JSON action dict. Falls back to respond action."""
    raw = raw.strip()
    # Strip markdown code fences if present
    if raw.startswith("```"):
      lines = raw.split("\n")
      raw = "\n".join(lines[1:-1]) if len(lines) > 2 else raw
    try:
      data = json.loads(raw)
      if "action" in data:
        return data
    except (json.JSONDecodeError, ValueError) as e:
      log.debug("llm_response_not_json", error=str(e), raw=raw[:200])
    return {"action": "respond", "message": raw}

  async def _call_with_fallback(self, messages: list[ChatMessage], system_prompt: str) -> str:
    """Try primary provider; fall back to secondary on API error."""
    try:
      return await self._primary.complete(messages, system_prompt)
    except Exception as e:
      log.warning("primary_llm_failed", error=str(e), fallback=self._fallback.model_name)
      return await self._fallback.complete(messages, system_prompt)

  async def _stream_with_fallback(
    self,
    messages: list[ChatMessage],
    system_prompt: str,
  ) -> AsyncGenerator[str, None]:
    """Stream from primary; fall back to non-streaming secondary on error."""
    try:
      async for chunk in self._primary.complete_streaming(messages, system_prompt):
        yield chunk
    except Exception as e:
      log.warning("primary_stream_failed", error=str(e), fallback=self._fallback.model_name)
      result = await self._fallback.complete(messages, system_prompt)
      yield result

  async def _summarize_history(self, history: ConversationHistory) -> ConversationHistory:
    """Compress all but the last RECENT_TURNS_TO_KEEP turns into a summary."""
    to_summarize = history.messages[:-RECENT_TURNS_TO_KEEP]
    recent = history.messages[-RECENT_TURNS_TO_KEEP:]

    conversation_text = "\n".join(
      f"{msg.role.value.upper()}: {msg.content[:500]}" for msg in to_summarize
    )
    prompt_messages = [
      ChatMessage(
        role=MessageRole.USER,
        content=(
          f"Summarize the following conversation in 2-3 paragraphs, preserving key facts, "
          f"decisions, and context:\n\n{conversation_text}"
        ),
      )
    ]
    summary = await self._call_with_fallback(prompt_messages, "You are a helpful summarizer.")

    if history.summary:
      summary = f"{history.summary}\n\n[Later:]\n{summary}"

    history.summary = summary
    history.messages = recent
    return history

