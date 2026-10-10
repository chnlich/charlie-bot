"""The ``publish:`` config section model, registered in this package's ``register()``."""

import pathlib

import pydantic


class PublishConfig(pydantic.BaseModel):
  """``publish:`` section: the outbound static publish lane."""

  model_config = pydantic.ConfigDict(extra='forbid')

  # Publish lane — the pair the outbound-link rewrite consumes (src/features/artifacts/publish.py):
  # dir is the directory the host serves (a `tailscale serve` path, or a host-local
  # static server a serve rule proxies to), and public_base_url is the base of the
  # links readers outside the operator's devices open. Unconfigured (either one) makes
  # publish unavailable; the reply path then refuses instead of falling back to a server-port link.
  dir: pathlib.Path | None = None
  public_base_url: str | None = None

  @pydantic.model_validator(mode="after")
  def _expand_tilde(self) -> PublishConfig:
    """Expand ``~`` in the publish directory."""
    if self.dir is not None:
      self.dir = self.dir.expanduser()
    return self
