"""The web reply channel: a web session's agent posts the replies its user reads, as Slack and Discord threads do."""

from src.runtime.hooks import turn_contributions, wiring


def register() -> None:
  wiring.register_command("web", "src.features.web_reply.cli")
  wiring.register_router("src.features.web_reply.api", prefix="/api/internal", tags=("internal",))
  turn_contributions.register_turn_contribution("web_reply", "src.features.web_reply.turn_contribution:CONTRIBUTION")
