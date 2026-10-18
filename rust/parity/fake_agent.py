"""Scripted agent used by the isolated parity harness."""

import base64
import ctypes
import fcntl
import hashlib
import hmac
import json
import os
import pathlib
import re
import sys
import time

_ARGV_VALUE = re.compile(r"\$\{argv:(?P<flag>[^}]+)\}")
_PDEATHSIG_OPTION = 2


class MissingArgvFlag(ValueError):
  """An emit value names a command-line flag the backend did not pass."""


def _proc_identity(pid: int) -> tuple[str, str]:
  stat = pathlib.Path(f"/proc/{pid}/stat").read_text(encoding="utf-8")
  fields = stat.rsplit(")", 1)[1].split()
  return fields[0], fields[19]


def _access_key(credentials: pathlib.Path) -> str:
  in_charliebot = False
  for line in credentials.read_text(encoding="utf-8").splitlines():
    if line and not line[0].isspace():
      in_charliebot = line.strip() == "charliebot:"
    elif in_charliebot and line.strip().startswith("access_key:"):
      value = line.strip().split(":", 1)[1].strip()
      if value[:1] in ("'", '"') and value[-1:] == value[:1]:
        value = value[1:-1]
      if not value:
        raise ValueError(f"empty charliebot.access_key in {credentials}")
      return value
  raise ValueError(f"charliebot.access_key is absent from {credentials}")


def _run_token_record() -> tuple[dict | None, bool]:
  token = os.environ.get("CHARLIEBOT_RUN_TOKEN")
  if token is None:
    return None, False
  payload_b64, signature_b64 = token.split(".", 1)
  padding = "=" * (-len(payload_b64) % 4)
  payload = json.loads(base64.urlsafe_b64decode(payload_b64 + padding))
  if not isinstance(payload, dict):
    raise ValueError("CHARLIEBOT_RUN_TOKEN payload is not an object")
  signature_padding = "=" * (-len(signature_b64) % 4)
  signature = base64.urlsafe_b64decode(signature_b64 + signature_padding)
  credentials = pathlib.Path(os.environ["CHARLIEBOT_HOME"]) / "credentials.yaml"
  key = _access_key(credentials)
  expected = hmac.new(key.encode("utf-8"), payload_b64.encode("ascii"), hashlib.sha256).digest()
  claims_valid = all(
      isinstance(payload.get(field), str) and payload[field] for field in ("session_id", "run_id", "agent"))
  return payload, claims_valid and hmac.compare_digest(signature, expected)


def _environment() -> dict[str, str]:
  names = {
      "CHARLIEBOT_SESSION_ID", "CHARLIEBOT_HOME", "CHARLIEBOT_VENV_BIN", "PATH", "GIT_CEILING_DIRECTORIES",
      "PYDANTIC_DISABLE_PLUGINS", "CLAUDECODE"
  }
  names.update(name for name in os.environ if name.startswith("CLAUDE_CODE_"))
  return {name: os.environ[name] for name in sorted(names) if name in os.environ}


def _spawn_record(index: int, name: str, scripted: bool, stdin_bytes: bytes) -> dict:
  pid = os.getpid()
  _, pid_start = _proc_identity(pid)
  pdeathsig = ctypes.c_int()
  libc = ctypes.CDLL(None, use_errno=True)
  result = libc.prctl(_PDEATHSIG_OPTION, ctypes.byref(pdeathsig), 0, 0, 0)
  if result != 0:
    error_number = ctypes.get_errno()
    raise OSError(error_number, os.strerror(error_number))
  claims, token_valid = _run_token_record()
  return {
      "index": index,
      "name": name,
      "scripted": scripted,
      "argv": [name, *sys.argv[2:]],
      "cwd": os.getcwd(),
      "pid": pid,
      "pid_start": pid_start,
      "stdout_path": os.readlink("/proc/self/fd/1"),
      "stdin_bytes": len(stdin_bytes),
      "stdin_sha256": hashlib.sha256(stdin_bytes).hexdigest(),
      "nice": os.getpriority(os.PRIO_PROCESS, 0),
      "own_session": os.getsid(0) == pid,
      "pdeathsig": pdeathsig.value,
      "env": _environment(),
      "run_token_claims": claims,
      "token_valid": token_valid,
  }


def _read_stdin() -> bytes:
  return b"" if sys.stdin.isatty() else sys.stdin.buffer.read()


def _start_index(agent_dir: pathlib.Path, name: str, stdin_bytes: bytes) -> tuple[int, bool, dict | None]:
  lock_path = agent_dir / "lock"
  spawns_path = agent_dir / "spawns.jsonl"
  with lock_path.open("a+") as lock_file:
    fcntl.flock(lock_file.fileno(), fcntl.LOCK_EX)
    starts_count = sum(1 for line in spawns_path.read_text(encoding="utf-8").splitlines()
                       if line.strip()) if spawns_path.exists() else 0
    script_path = agent_dir / "script.json"
    script = json.loads(script_path.read_text(encoding="utf-8")) if script_path.exists() else None
    starts = script["starts"] if script is not None else []
    entry = starts[starts_count] if starts_count < len(starts) else None
    scripted = entry is not None and entry["name"] == name
    record = _spawn_record(starts_count, name, scripted, stdin_bytes)
    with spawns_path.open("a", encoding="utf-8") as spawns_file:
      spawns_file.write(json.dumps(record, ensure_ascii=False, separators=(",", ":")) + "\n")
      spawns_file.flush()
    return starts_count, scripted, entry


def _argv_value(flag: str) -> str:
  args = sys.argv[2:]
  for index, arg in enumerate(args):
    if arg == flag:
      if index + 1 == len(args):
        raise MissingArgvFlag(f"missing argv value after flag {flag!r}")
      return args[index + 1]
  raise MissingArgvFlag(f"missing argv flag {flag!r}")


def _substitute(value: object) -> object:
  if isinstance(value, str):
    match = _ARGV_VALUE.fullmatch(value)
    return _argv_value(match["flag"]) if match else value
  if isinstance(value, list):
    return [_substitute(item) for item in value]
  if isinstance(value, dict):
    return {key: _substitute(item) for key, item in value.items()}
  return value


def _run_steps(entry: dict, agent_dir: pathlib.Path, index: int) -> int:
  for step in entry["steps"]:
    if "emit" in step:
      event = _substitute(step["emit"])
      if not isinstance(event, dict):
        raise ValueError("emit step must resolve to a JSON object")
      sys.stdout.write(json.dumps(event, ensure_ascii=False, separators=(",", ":")) + "\n")
      sys.stdout.flush()
    elif "gate" in step:
      gate = step["gate"]
      (agent_dir / f"at-{gate}").write_text(str(index), encoding="utf-8")
      while not (agent_dir / f"open-{gate}").exists():
        time.sleep(0.02)
    else:
      raise ValueError(f"unknown fake-agent step: {step!r}")
  return entry["exit_code"]


def _write_exit(agent_dir: pathlib.Path, index: int, exit_code: int) -> None:
  (agent_dir / f"exit-{index}").write_text(str(exit_code), encoding="utf-8")


def main() -> int:
  name = sys.argv[1]
  agent_dir = pathlib.Path.home() / "fake-agent"
  agent_dir.mkdir(parents=True, exist_ok=True)
  stdin_bytes = _read_stdin()
  index, scripted, entry = _start_index(agent_dir, name, stdin_bytes)
  if not scripted:
    print(f"{name}: parity stand-in, not an agent CLI", file=sys.stderr)
    exit_code = 1
  else:
    try:
      exit_code = _run_steps(entry, agent_dir, index)
    except MissingArgvFlag as error:
      print(f"{name}: {error}", file=sys.stderr)
      exit_code = 2
    except Exception as error:
      print(f"{name}: {type(error).__name__}: {error}", file=sys.stderr)
      exit_code = 2
  _write_exit(agent_dir, index, exit_code)
  return exit_code


if __name__ == "__main__":
  sys.exit(main())
