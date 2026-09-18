"""The batch-mode ssh invocation shared by the remote-probe and remote-launch paths."""

from src.core.timeouts import SSH_CONNECT_TIMEOUT


def ssh_cmd(host: str, *remote_argv: str) -> list[str]:
  """Build the ssh argv that runs *remote_argv* on *host*.

  BatchMode fails instead of hanging on a password prompt, and the connect is
  bounded by SSH_CONNECT_TIMEOUT; both options are policy here, so every
  remote ssh subprocess takes its argv from this one place.
  """
  return ["ssh", "-o", "BatchMode=yes", "-o", f"ConnectTimeout={SSH_CONNECT_TIMEOUT}", host, *remote_argv]
