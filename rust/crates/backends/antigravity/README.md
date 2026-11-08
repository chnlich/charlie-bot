# charliebot-antigravity

The `antigravity` backend type: `agy --print`, whose JSON envelope the runner reads once the process has exited.

## Public API

- `charliebot_antigravity::{register, USAGE_SOURCE}`, `AntigravityBackend` and its finish, through the private
  `antigravity_cli`, `factory` and `options` modules.

## Subscription quota

The usage source runs `agy -p '/usage' --output-format json` during a quota poll and maps each configured subscription
model to exactly one group. The panel combines those groups' five-hour and weekly windows. A model that matches no
group or more than one group makes the poll fail with the model named in the account error. Member readings use only
the last successful CLI response and the shared quota rule; member selection does not spawn the CLI.
