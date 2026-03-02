# Scheduled Tasks (`config.d/cron.yaml`)

Cron tasks are defined in `~/.charliebot/config.d/cron.yaml` under `scheduled_tasks`.

Example:

```yaml
scheduled_tasks:
  - name: nightly-review
    cron: "0 2 * * *"
    prompt: "Review open TODOs and create an action summary."
    repo: "~/workspace/charlie-bot"
    timezone: "America/New_York"
    enabled: true
    subagent:
      backend: "codex-o3"
      model: "o3"
```

`subagent.backend` and `subagent.model` are optional per-task overrides for worker runs.
If `subagent` is omitted, the scheduler uses the session's configured backend default.

MVP note: fallback backend/model selection is not supported for scheduled workers.
