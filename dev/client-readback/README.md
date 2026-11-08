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
- `newest message` — each page load's first chat render (`newest_message_ms`)
  and catch-up (`current_ms`), by `open_case` and `content_source`; a null
  timing counts as not reached
- `cross-origin files` — each render-blocking file the page loaded from another
  origin. A load whose `transfer_size` is 0 is a cache hit (no bytes crossed the
  network); a load whose `transfer_size` is greater than 0 is a network load.
  The rule reads `transfer_size`, which the browser reports for a cross-origin
  load only when its origin sends `Timing-Allow-Origin`; the page's CDNs
  (jsDelivr, cdnjs, Google Fonts) do, so the rule holds for the files the page
  loads. The two groups are reported apart. The `link` column is the time the
  same
  bytes (the file's `encoded_body_size`) take over the CharlieBot link, from the
  same load's same-origin timings: the round trip is the least
  request-to-first-byte time among them, and the throughput is the bytes over
  the transfer time of the largest same-origin body. `link` is the median over
  the loads that carry both.

## Usage

```sh
node dev/client-readback/readback.mjs --from 2026-11-05T03:00:00Z --to 2026-11-05T14:00:00Z \
  path/to/server.log ...  # the server log files that cover the window's days
node dev/client-readback/readback.mjs --from ... --to ... --device 100.64.0.8 <log>...
```

The window bounds are the log's own timestamps. A percentile is the sorted
sample's `floor(p * (n - 1))` element.

The contract test `web/tests/client_readback.test.mjs` runs this script on a
fixture log and checks every section's numbers.
