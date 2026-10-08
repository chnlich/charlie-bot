from src.runtime.hooks import sidebar_contributions, turn_contributions, wiring


def register() -> None:
  turn_contributions.register_turn_contribution(
      "chat_threads", "src.features.chat_threads.turn_contribution:CONTRIBUTION")
  wiring.register_router(
      "src.features.chat_threads.api", prefix="/api/sessions", tags=("sessions",), before_runtime=True)
  sidebar_contributions.register_sidebar_contribution("chat_threads", "src.features.chat_threads.sidebar:contribution")
