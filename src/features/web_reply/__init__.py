"""The web reply channel: a web session's agent posts the replies its user reads, as Slack and Discord threads do."""

from src.infra import metadata_slot_registration
from src.runtime.hooks import turn_contributions


def register() -> None:
  turn_contributions.register_turn_contribution("web_reply", "src.features.web_reply.turn_contribution:CONTRIBUTION")
  metadata_slot_registration.register_metadata_fields("web_reply", "src.features.web_reply.metadata:WebReplyMetadata")
