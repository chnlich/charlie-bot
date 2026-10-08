from src.runtime.hooks import sequence_controllers, wiring


def register() -> None:
  sequence_controllers.register_sequence_controller(
      "improve", "src.features.improve.sequence_controller:ImproveSequenceController")
  wiring.register_command("improve", "src.features.improve.cli")
  wiring.register_command("improve-stop", "src.features.improve.stop_cli")
  wiring.register_router("src.features.improve.api", prefix="/api/internal", tags=("internal",))
