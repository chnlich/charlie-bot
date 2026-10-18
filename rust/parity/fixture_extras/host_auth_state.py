"""Write the operator's ssh config and a host-auth state file for the host-auth parity scenario.

Every last_probe_at lies in 2099, so no host is due at startup and no probe runs. Every enrolled_observed_at is null,
so no estimate depends on the clock. The state holds one entry per status and one alias the config no longer lists.
"""

from rust.parity.fixture import Builder
from src.features.host_auth import host_auth

PROBED = "2099-01-01T00:00:00+00:00"

# The config carries each parser case: plain aliases, a quoted alias, an `=` separator, a comment, a wildcard block,
# a compound Host line, a Match block, a duplicate alias and a block without HostName.
CONFIG = """\
# Synthetic operator ssh config for the host-auth parity scenario.
Host login-ok
  HostName login-ok.example.internal

Host "login-held"
  HostName login-held.example.internal

Host=login-eq
  HostName=login-eq.example.internal  # separators and an inline comment

Host login-down
  HostName login-down.example.internal

Host *.example.internal
  User synthetic-operator

Host login-pair-a login-pair-b
  HostName login-pair.example.internal

Match host login-matched
  HostName login-matched.example.internal

Host login-ok
  HostName login-ok-second.example.internal

Host login-nohost
  User synthetic-operator
"""

HELD_DETAIL = "Okta verification required\nEnroll at https://login.example.internal/activate?user_code=7f3c9d21"
CHECK_DETAIL = "Tailscale SSH requires an additional check.\nTo authenticate, visit: https://login.example.net/a/7f3c9d21"
DOWN_DETAIL = "ssh: connect to host login-down.example.internal port 22: Connection timed out"


def _entry(alias: str, status: str, detail: str) -> dict:
  return {
      "alias": alias,
      "hostname": f"{alias}.example.internal",
      "status": status,
      "detail": detail,
      "last_probe_at": PROBED,
      "last_change_at": PROBED,
      "last_ok_at": PROBED if status == host_auth.STATUS_OK else None,
      "enrolled_observed_at": None,
  }


async def build(b: Builder) -> dict:
  ssh_dir = b.cfg.charliebot_home.parent / ".ssh"
  ssh_dir.mkdir(parents=True)
  (ssh_dir / "config").write_text(CONFIG, encoding="utf-8")

  state = host_auth.empty_state()
  state["probed_at"] = PROBED
  state["hosts"] = [
      _entry("login-ok", host_auth.STATUS_OK, "exit 0"),
      _entry("login-held", host_auth.STATUS_NEEDS_OKTA, HELD_DETAIL),
      _entry("login-eq", host_auth.STATUS_NEEDS_INTERACTIVE_AUTH, CHECK_DETAIL),
      _entry("login-down", host_auth.STATUS_UNREACHABLE, DOWN_DETAIL),
      _entry("login-retired", host_auth.STATUS_OK, "exit 0"),
  ]
  host_auth.save_state(state, b.cfg.charliebot_home / host_auth.STATE_FILENAME)
  return {"config": ".ssh/config", "state": "home/host_auth.json"}
