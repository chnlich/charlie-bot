from src.runtime.hooks import turn_contributions


def register() -> None:
  turn_contributions.register_turn_contribution(
      "chat_threads", "src.features.chat_threads.turn_contribution:CONTRIBUTION")
