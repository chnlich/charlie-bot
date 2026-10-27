from src.runtime.hooks import sequence_controllers


def register() -> None:
  sequence_controllers.register_sequence_controller(
      "improve", "src.features.improve.sequence_controller:ImproveSequenceController")
