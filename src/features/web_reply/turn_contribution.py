"""The web_reply turn contribution: the reply rules, the round-end audit, the unread claim and the chat lines."""

from collections.abc import Callable

from src.features.web_reply import audit, reply
from src.infra import config, models
from src.infra import event_types as ET
from src.runtime import (
    session_events,
    session_lifecycle,
    session_store,
    session_successor,
    task_execution,
    task_prompts,
)
from src.runtime.hooks import turn_contributions


def _web_reply_message(event: dict) -> dict:
  return {"role": ET.WEB_REPLY, "content": event.get("content", "")}


def _web_reply_notice_message(event: dict) -> dict:
  return {"role": ET.WEB_REPLY_NOTICE, "content": event.get("content", "")}


def _web_status_message(event: dict) -> dict:
  return {"role": ET.WEB_STATUS, "content": event.get("content", ""), "web_status": event.get(ET.WEB_STATUS)}


def _web_ask_check_message(event: dict) -> dict:
  return {"role": ET.WEB_ASK_CHECK, "content": event.get("content", "")}


class WebReplyTurnContribution(turn_contributions.TurnContribution):
  """Carries the reply rules in a web session's manager turns, audits the turns that answer a user, renders replies."""

  async def after_turn(self, meta: models.SessionMetadata, done_event: dict, *, cfg: config.CharlieBotConfig) -> None:
    # An errored turn is not audited: its error event already shows and marks the session unread.
    if not reply.is_web_session(meta) or done_event["exit_code"] != 0:
      return
    await audit.audit_turn(
        meta.id,
        done_event,
        session_store.store(),
        session_events.events(),
        session_lifecycle.lifecycle(),
        session_successor.successor(),
        task_execution.task_manager(),
        archived=meta.status == models.SessionStatus.ARCHIVED)

  def instruction_segments(self, meta: models.SessionMetadata, kind: str,
                           cfg: config.CharlieBotConfig) -> list[task_prompts.RuleSegment]:
    """The web reply rules for a manager run of a web session, read fresh from prompts/ on every call."""
    if kind not in task_prompts.MANAGER_KINDS or not reply.is_web_session(meta):
      return []
    text = (cfg.charlie_bot_repo / "prompts" / reply.RULES_FILE).read_text(encoding="utf-8").strip()
    return [
        task_prompts.RuleSegment(
            text=text,
            sources=(
                task_prompts.PromptSource(scope=task_prompts.SCOPE_BASE, source_ref=f"prompts/{reply.RULES_FILE}"),),
        )
    ]

  def claims_turn_end_unread(self, meta: models.SessionMetadata) -> bool:
    """A web session's clean turn leaves the unread mark to the audit and the reply, which mark it when needed."""
    return reply.is_web_session(meta)

  def event_renderers(self) -> dict[str, Callable[[dict], dict]]:
    return {
        ET.WEB_REPLY: _web_reply_message,
        ET.WEB_REPLY_NOTICE: _web_reply_notice_message,
        ET.WEB_STATUS: _web_status_message,
        ET.WEB_ASK_CHECK: _web_ask_check_message,
    }


CONTRIBUTION = WebReplyTurnContribution()
