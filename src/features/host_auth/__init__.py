from src.runtime.hooks import page_render, wiring


def register() -> None:
  wiring.register_router("src.features.host_auth.api", tags=("host-auth",))
  wiring.register_service("host_auth", "src.features.host_auth.api")
  page_render.register_templates("src.features.host_auth")
  page_render.register_home_card(
      "Host login authorization", "/host-auth", "Per-host ssh login state and the estimated Okta renewal deadline.")
