# charliebot-antigravity

The `antigravity` backend type: `agy acp`, which runs each turn over the Agent Client Protocol. The turn's output is
translated from the raw log, and each message that arrives mid-turn is a further `session/prompt` on the same session.

## Public API

- `charliebot_antigravity::{register, USAGE_SOURCE}`, through the private `antigravity_cli`, `factory`, `lifecycle` and
  `options` modules.

## Subscription quota

The usage source runs `agy -p '/usage' --output-format json` during a quota poll and maps each configured subscription
model to exactly one group. The panel combines those groups' five-hour and weekly windows. A model that matches no
group or more than one group makes the poll fail with the model named in the account error. Member readings use only
the last successful CLI response and the shared quota rule; member selection does not spawn the CLI.
