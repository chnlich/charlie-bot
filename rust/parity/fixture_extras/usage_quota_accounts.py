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
  return {"claude_credentials": ".claude/.credentials.json"}
