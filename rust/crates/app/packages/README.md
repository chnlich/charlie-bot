# charliebot-packages

The composition root both binaries share. It owns the runtime registrations and the package registration list in
the server's order. The server binary and the CLI binary both register the packages through this crate, so a
package added here reaches both and the two lists cannot drift.

## Public API

| Rust item | Role |
| --- | --- |
| `RUNTIME` | the server's runtime registrations, which run before the package list |
| `PACKAGES` | the server's package list, one entry per package |
| `registry` | the composition root both binaries share |

The list belongs to this crate: a package added to the server is added here.
