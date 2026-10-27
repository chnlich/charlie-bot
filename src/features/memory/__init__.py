from src.runtime.hooks import turn_contributions


def register() -> None:
  turn_contributions.register_turn_contribution("memory", "src.features.memory.turn_contribution:CONTRIBUTION")
