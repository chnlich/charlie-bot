# Networking

CharlieBot's server binds to `127.0.0.1` over plain HTTP.

TLS, remote access, and the public name are handled outside the app by `tailscale serve`.

The access key from `credentials.yaml` (`charliebot.access_key`) is the app-layer credential for
CharlieBot requests; it rides the `Authorization: Bearer` header or the `charliebot_access_key` cookie.
