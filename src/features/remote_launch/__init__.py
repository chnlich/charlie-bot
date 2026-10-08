from src.runtime.hooks import wiring


def register() -> None:
  wiring.register_command("remote-launch", "src.features.remote_launch.cli")
