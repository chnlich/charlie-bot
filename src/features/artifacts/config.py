"""The ``publish:`` config section model, registered in this package's ``register()``."""

import pathlib

import pydantic


class PublishConfig(pydantic.BaseModel):
  """``publish:`` section: the outbound publish lane, one of two kinds."""

  model_config = pydantic.ConfigDict(extra='forbid')

  # Publish lane, read by the one publish action (src/features/artifacts/publish.py). A host
  # configures one lane. The command lane: command is an external program's argv; it gets the
  # artifact's absolute path and the lane options, and prints the reader URL. The directory lane:
  # dir is the directory the host serves (a `tailscale serve` path, or a host-local static server
  # a serve rule proxies to), and public_base_url is the base of the links readers outside the
  # operator's devices open. With no lane configured, publish is unavailable; the reply path then
  # refuses instead of falling back to a server-port link.
  command: list[str] | None = None
  dir: pathlib.Path | None = None
  public_base_url: str | None = None

  @pydantic.model_validator(mode="after")
  def _expand_tilde(self) -> PublishConfig:
    """Expand ``~`` in the publish directory."""
    if self.dir is not None:
      self.dir = self.dir.expanduser()
    return self

  @pydantic.model_validator(mode="after")
  def _one_lane(self) -> PublishConfig:
    """Refuse an empty command, and a command set together with the directory lane's keys."""
    if self.command is None:
      return self
    if not self.command:
      raise ValueError("publish.command is an empty list; name the program and its arguments, or leave the key unset")
    if self.dir is not None or self.public_base_url is not None:
      raise ValueError(
          "publish.command cannot be set together with publish.dir or publish.public_base_url; "
          "a host configures one publish lane")
    return self
