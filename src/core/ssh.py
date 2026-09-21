"""The batch-mode ssh invocation shared by the remote-probe and remote-launch paths."""

import os

from src.core.timeouts import SSH_CONNECT_TIMEOUT

# Connection reuse for the probe family. Every remote probe paid one full ssh
# handshake (TCP + KEX + auth, ~0.85 s to this deployment's SLURM login host)
# while ControlMaster=auto amortizes it: one master per (local user, host,
# port) serves every probe until _CONTROL_PERSIST_SECONDS idle, and the remote
# watch ladder's 600 s plateau plus its ≤10 s noise stays inside that window,
# so a watched host's probes never re-master while the watch lives. The
# handshake is paid once per expiry instead of once per probe; a master killed
# uncleanly leaves a stale socket and the next probe re-masters over it
# (measured: one recovery probe, rc 0, warmth after). ssh creates the socket
# 0600; the 0700 parent keeps other local users from reaching it.
_CONTROL_DIR = os.path.join(os.path.expanduser("~"), ".ssh", "controlmasters")
_CONTROL_PERSIST_SECONDS = 1200


def _control_dir() -> str:
  """Create the control-socket directory 0700 and return its path."""
  os.makedirs(_CONTROL_DIR, mode=0o700, exist_ok=True)
  return _CONTROL_DIR


def ssh_cmd(host: str, *remote_argv: str) -> list[str]:
  """Build the ssh argv that runs *remote_argv* on *host*.

  BatchMode fails instead of hanging on a password prompt, and the connect is
  bounded by SSH_CONNECT_TIMEOUT; both options are policy here, so every
  remote ssh subprocess takes its argv from this one place. The probe family's
  repeated handshakes ride one ControlMaster per (user, host, port): probes
  multiplex over the master while it lives, the master exits after
  _CONTROL_PERSIST_SECONDS idle, and ssh falls back to a fresh master when a
  socket went stale. The directory is created here because ssh fails a probe
  outright when the ControlPath's parent is missing.
  """
  return [
      "ssh",
      "-o",
      "BatchMode=yes",
      "-o",
      f"ConnectTimeout={SSH_CONNECT_TIMEOUT}",
      "-o",
      "ControlMaster=auto",
      "-o",
      f"ControlPath={_control_dir()}/%C",
      "-o",
      f"ControlPersist={_CONTROL_PERSIST_SECONDS}",
      host,
      *remote_argv,
  ]
