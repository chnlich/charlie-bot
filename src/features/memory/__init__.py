from src.runtime.hooks import turn_contributions, wiring


def register() -> None:
  wiring.register_command("memory", "src.features.memory.cli")
  turn_contributions.register_turn_contribution("memory", "src.features.memory.turn_contribution:CONTRIBUTION")
