"""The ``host_auth:`` config section: the browser logins the login page offers.

A login is a command that prints a device-approval link, plus a readback command that exits 0 while
the login is valid. The operator's host config names them; the repo ships none.
"""

import re

import pydantic


class BrowserLoginConfig(pydantic.BaseModel):
  """One ``host_auth.browser_logins`` entry: a login that the page starts with one click."""

  model_config = pydantic.ConfigDict(extra='forbid')

  # The URL segment of the start route, /api/host-auth/logins/<name>/start.
  name: str = pydantic.Field(pattern=r"^[a-z0-9-]+$")
  # The text the page shows for this login.
  label: str
  # The login command as argv. It runs without a shell and reads stdin from /dev/null.
  login_command: list[str] = pydantic.Field(min_length=1)
  # A regex. Its first match in the login output is the approval link, kept exactly as matched.
  approval_url_pattern: str
  # The argv of a check that exits 0 while the login is valid. Its output shows on the page.
  readback_command: list[str] = pydantic.Field(min_length=1)

  @pydantic.field_validator("approval_url_pattern")
  @classmethod
  def _compiles(cls, pattern: str) -> str:
    try:
      re.compile(pattern)
    except re.error as exc:
      raise ValueError(f"is not a valid regular expression: {exc}") from exc
    return pattern


class HostAuthConfig(pydantic.BaseModel):
  """``host_auth:`` section: the host-auth panel's browser logins."""

  model_config = pydantic.ConfigDict(extra='forbid')

  browser_logins: list[BrowserLoginConfig] = pydantic.Field(default_factory=list)

  @pydantic.field_validator("browser_logins")
  @classmethod
  def _unique_names(cls, logins: list[BrowserLoginConfig]) -> list[BrowserLoginConfig]:
    seen: set[str] = set()
    for login in logins:
      if login.name in seen:
        raise ValueError(f"name {login.name!r} is used by more than one login")
      seen.add(login.name)
    return logins
