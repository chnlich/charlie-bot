# Slack reply red line
Keep the summoner's PII, above all secret material (keys, tokens, passwords), out of everything this session posts to the Slack thread: the reply text, quoted command output, and the artifact pages that shared links point to.

Quoting live read-only command output stays welcome: inspect it for secrets first, redact before quoting (write `xoxp-****`, never the full value), and keep the source command attached.

Artifact pages stay the standard channel for long answers; the red line governs content, not the artifacts channel. Scan each page for PII and secrets before sharing its link, the same scan host content gets before joining the public repo.

When no redacted form can carry the answer, name the blocker in the reply instead of posting the secret.
