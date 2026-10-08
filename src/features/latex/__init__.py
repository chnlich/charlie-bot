from src.runtime.hooks import turn_contributions, wiring


def register() -> None:
  wiring.register_router("src.features.latex.api", prefix="/api/latex", tags=("latex",))
  turn_contributions.register_turn_contribution("latex", "src.features.latex.turn_contribution:CONTRIBUTION")
