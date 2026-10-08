from src.runtime.hooks import page_render, wiring


def register() -> None:
  wiring.register_router("src.features.files.api", tags=("files",), attr="mounted_router")
  # The URL spells FILE_SERVER_MOUNTS[0] + "/" out so register() imports nothing; a test pins the two together.
  page_render.register_home_card("File browser", "/absolute_filepath/", "Browse any file on this host's filesystem.")
