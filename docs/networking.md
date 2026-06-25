# Networking

CharlieBot's server binds to `127.0.0.1` over plain HTTP.

TLS, remote access, and the public name are handled outside the app by `tailscale serve`.

`charliebot_access_key` is the app-layer credential for CharlieBot requests.
