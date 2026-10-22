"""Synthetic quota-account files for the failing quota-panel scenario (features-ext-usage-failing).

The panel lists two accounts in registration order: ``claude:main``, then ``codex:main``.

Claude (src/backends/claude_code/usage_quota.py): ``quota_accounts()`` yields the ``main`` account on the default
login directory ``$HOME/.claude`` whether or not it exists. Each fetch reads ``.credentials.json`` there. Without the
file the fetch returns "credentials not found" and raises nothing, so the poller logs no ``ext_usage_poll_error``.
This extra writes the file with a synthetic token, so the fetch reaches the usage endpoint and raises.

Codex (src/backends/codex/usage_quota.py): ``quota_accounts()`` yields ``main`` on ``$HOME/.codex`` and needs no file.
Its fetch reads ``.codex/sessions``, which this extra leaves absent, so the fetch returns "no sessions found".

The harness proxy has no fake for the quota hosts: ``fakes/proxy.py`` HOST_FAKE lists neither ``api.anthropic.com``
(the usage endpoint) nor ``platform.claude.com`` (the token refresh). The scenario declares ``openai_compatible`` so
the CONNECT proxy is in the request path. The proxy answers ``CONNECT api.anthropic.com:443`` with
``403 Forbidden`` and records the refusal in ``logs/fakes.jsonl``, so the Claude fetch fails on both sides and
``ext_usage_poll_error`` logs with ``provider=claude`` and ``account=main``.
"""

import json

from rust.parity.fixture import Builder

CLAUDE_CREDENTIALS = {
    "claudeAiOauth": {
        "accessToken": "synthetic-claude-access-token",
        "refreshToken": "synthetic-claude-refresh-token",
    },
}


async def build(b: Builder) -> dict:
  claude_dir = b.cfg.charliebot_home.parent / ".claude"
  claude_dir.mkdir(parents=True)
  (claude_dir / ".credentials.json").write_text(json.dumps(CLAUDE_CREDENTIALS, indent=2), encoding="utf-8")

  # The legacy metadata used by the session API parity scenario predates the
  # registered slot defaults. Leave one Slack origin intact so chat-threads
  # still has a row to serialize, while the other absent slots exercise the
  # shared response serializer.
  session_fields = (
      "route",
      "backend_from_pool",
      "web_status",
      "discord_origin",
      "discord_watermark_id",
      "claude_account",
      "slack_watermark_ts",
      "slack_origin",
  )
  delta_id = b.sid("delta")
  for metadata_path in b.cfg.sessions_dir.glob("*/metadata.json"):
    metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
    for field in session_fields:
      if metadata_path.parent.name == delta_id and field == "slack_origin":
        continue
      metadata.pop(field, None)
    metadata_path.write_text(json.dumps(metadata, indent=2) + "\n", encoding="utf-8")

  # One manager proves that a retired Run backend does not replace its
  # configured session backend. One worker proves the same Run backend is
  # displayed after startup recovery warms the in-memory map.
  for run_key in ("atlas_t3", "notes_r2"):
    run = b.manifest["runs"][run_key]
    run_path = b.cfg.sessions_dir / run["session"] / "data" / "runs" / run["run"] / "metadata.json"
    run_metadata = json.loads(run_path.read_text(encoding="utf-8"))
    run_metadata["backend"] = "synthetic-retired-run-backend"
    run_path.write_text(json.dumps(run_metadata, indent=2) + "\n", encoding="utf-8")

  return {"claude_credentials": ".claude/.credentials.json"}
