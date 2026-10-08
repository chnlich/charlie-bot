from src.runtime.hooks import sidebar_contributions, wiring


def register() -> None:
  wiring.register_router("src.features.recap.api", prefix="/api/sessions", tags=("sessions",))
  sidebar_contributions.register_sidebar_contribution("recap", "src.features.recap.sidebar:contribution")
