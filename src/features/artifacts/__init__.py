from src.runtime.hooks import wiring


def register() -> None:
  wiring.register_command("artifact", "src.features.artifacts.cli")
  wiring.register_command("plan", "src.features.artifacts.plan_cli")
  wiring.register_command("publish", "src.features.artifacts.publish_cli")
