# charliebot-packages

The composition root both binaries share. It owns the runtime registrations and the package registration list in
the server's order. The server binary and the CLI binary both register the packages through this crate, so a
package added here reaches both and the two lists cannot drift.

## Public API

| Rust item | Role |
| --- | --- |
| `registry` | the composition root both binaries share |

The two lists, `RUNTIME` and `PACKAGES`, are internal to this crate; `registry` reads both.
The list belongs to this crate: a package added to the server is added here.
