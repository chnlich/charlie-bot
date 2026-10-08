from src.runtime.hooks import wiring


def register() -> None:
  wiring.register_router("src.features.files.api", tags=("files",), attr="mounted_router")
