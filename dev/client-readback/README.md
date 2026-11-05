# Client-timing readback

Summarizes the web client's timing telemetry per device over a time window of
CharlieBot server logs, so a poor-network fix gets its before and after numbers
from real devices.

Every page interaction the timing channel covers posts one event to
`POST /api/diag/client-events`; the server writes each one as a single
`diag_client` log line (`rust/crates/features/diag/`). This script reads those
lines from one or more log files, keeps a time window, groups by the browser
address the server logged (`forwarded_for`), and prints per device:

- `switch completed` — elapsed p50/p90/p99 and the count over 5 s
- `send click-to-echo` — p50/p90
- `socket network disconnects` — count and the reconnect-wait p50/p90/max
  (switch-caused closes are excluded by the event's own `cause` field)
- `traffic` — received bytes per tab-hour by endpoint; a tab-hour is the
  tab-time the traffic events' `window_s` fields cover, so a tab that stayed
  hidden for hours still divides out
- `page loads` — the count of each load cause

## Usage

```sh
node dev/client-readback/readback.mjs --from 2026-11-05T03:00:00Z --to 2026-11-05T14:00:00Z \
  /tmp/charliebot-logs/server_20261105_031143.log  # the log files of the window's days
node dev/client-readback/readback.mjs --from ... --to ... --device 100.64.0.8 <log>...
```

The window bounds are the log's own timestamps. A percentile is the sorted
sample's `floor(p * (n - 1))` element.

The contract test `web/tests/client_readback.test.mjs` runs this script on a
fixture log and checks every section's numbers.
