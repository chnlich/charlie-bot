# charliebot-packages

The composition root both binaries share. It owns the runtime registrations and the package registration list in
the server's order (`src/app/registrations.py:16-46`). The server binary and the CLI binary both register the
packages through this crate, so a package added here reaches both and the two lists cannot drift.

## Public API and Python source

| Rust item | Python source |
| --- | --- |
| `RUNTIME` | `server.py:262-273` |
| `PACKAGES` | `src/app/registrations.py:16-46` |
| `registry` | the server's composition root |
| `cli_registry` | the CLI's composition root: `PACKAGES` without the Python-only stubs |

## The CLI subset

`cli_registry` leaves out `PYTHON_ONLY_PACKAGES`, which currently holds `src.features.router`. A CLI verb loads the
config and resolves backend types; it renders no page and serves no route, so the no-op stub registers nothing a
verb uses. This omission is the only difference between the two binaries' registries, and this crate states it once.
