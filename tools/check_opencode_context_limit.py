"""Check the free model's context override without credentials or model calls.

Run with ``uv run python -m tools.check_opencode_context_limit``. The server and its
XDG stores live in a temporary directory under this checkout and are removed on exit.
"""

import json
import os
import pathlib
import shutil
import socket
import subprocess
import tempfile

from src.backends.opencode import options


def main() -> None:
  """Start an isolated opencode v2 server and check its model catalog with curl."""
  binary = pathlib.Path.home() / ".opencode" / "bin" / "opencode"
  assert binary.is_file(), f"opencode binary missing: {binary}"
  curl = shutil.which("curl")
  assert curl is not None, "curl is required"
  version = subprocess.run([str(binary), "--version"], check=True, capture_output=True, text=True, timeout=10)
  assert version.stdout.startswith("opencode v2."), version.stdout
  print(version.stdout.strip())

  option = options.OpencodeBackend(
      id="free-context-check",
      label="Free context check",
      model="opencode/deepseek-v4.1-flash-free",
      context_limit=120000)
  content = options.model_overrides_config([option])
  print(f"OPENCODE_CONFIG_CONTENT={json.dumps(content)}")
  repo = pathlib.Path(__file__).resolve().parents[1]
  with tempfile.TemporaryDirectory(prefix=".opencode-context-check-", dir=repo) as directory:
    root = pathlib.Path(directory)
    env = {
        "PATH": os.environ["PATH"],
        "XDG_CONFIG_HOME": str(root / "config"),
        "XDG_DATA_HOME": str(root / "data"),
        "XDG_STATE_HOME": str(root / "state"),
        "XDG_CACHE_HOME": str(root / "cache"),
        "OPENCODE_SERVER_PASSWORD": "context-limit-check",
        "OPENCODE_CONFIG_CONTENT": json.dumps(content),
        "OPENCODE_DISABLE_FFF": "1"
    }
    with socket.socket() as listener:
      listener.bind(("127.0.0.1", 0))
      port = listener.getsockname()[1]
    with (root / "server.log").open("w") as log:
      proc = subprocess.Popen(
          [str(binary), "serve", "--port", str(port)], cwd=root, env=env, stdout=log, stderr=subprocess.STDOUT)
      try:
        response = subprocess.run(
            [
                curl, "--silent", "--show-error", "--fail", "--retry", "30", "--retry-connrefused", "--retry-delay",
                "1", "--retry-max-time", "60", "--max-time", "3", "-u", "opencode:context-limit-check",
                f"http://127.0.0.1:{port}/api/model"
            ],
            check=True,
            capture_output=True,
            text=True,
            timeout=65)
        model = next(
            model for model in json.loads(response.stdout)["data"]
            if model["providerID"] == "opencode" and model["id"] == "deepseek-v4.1-flash-free")
        print(json.dumps({"providerID": model["providerID"], "id": model["id"], "limit": model["limit"]}))
        assert model["limit"]["context"] == 120000, model["limit"]
        assert model["limit"]["output"] == 32000, model["limit"]
      except Exception:
        print((root / "server.log").read_text())
        raise
      finally:
        if proc.poll() is None:
          proc.terminate()
          try:
            proc.wait(timeout=5)
          finally:
            if proc.poll() is None:
              proc.kill()
              proc.wait(timeout=5)
      print("server stopped; no model call made")


if __name__ == "__main__":
  main()
