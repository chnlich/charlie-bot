# SSL / HTTPS

CharlieBot can serve over HTTPS by providing TLS certificate and key files.

## Config

In `~/.charliebot/config.yaml`:

```yaml
ssl_certfile: '~/.charliebot/ssl/cert.pem'
ssl_keyfile: '~/.charliebot/ssl/key.pem'
```

Both fields are optional. When both are set, uvicorn starts with HTTPS. When omitted (or set to `null`), the server runs plain HTTP.

`~` is expanded automatically — no need to use absolute paths.

## Generating a self-signed cert

```bash
mkdir -p ~/.charliebot/ssl
openssl req -x509 -newkey rsa:4096 -nodes \
  -keyout ~/.charliebot/ssl/key.pem \
  -out ~/.charliebot/ssl/cert.pem \
  -days 365 -subj '/CN=localhost'
chmod 600 ~/.charliebot/ssl/key.pem
```

## How it works

`server.py` reads `ssl_certfile` and `ssl_keyfile` from the config and passes them to `uvicorn.run()` as `ssl_kwargs`. Uvicorn handles the TLS termination directly — no reverse proxy needed.

## Restart required

Changing SSL config requires a server restart.
