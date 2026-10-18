"""Mint one run-local CA and a SAN leaf so hardcoded provider URLs trust the in-process proxy."""

import dataclasses
import pathlib
import subprocess

from . import proxy

OPENSSL = pathlib.Path("/usr/bin/openssl")
RSA_BITS = "2048"
CERTIFICATE_DAYS = "2"
CA_BASIC_CONSTRAINTS = "critical,CA:TRUE"
CA_KEY_USAGE = "critical,keyCertSign,cRLSign"


@dataclasses.dataclass(frozen=True)
class Certificates:
  ca: pathlib.Path
  leaf: pathlib.Path
  private_key: pathlib.Path


def generate(scratch: pathlib.Path) -> Certificates:
  scratch.mkdir(parents=True, exist_ok=True)
  ca_key = scratch / "parity-ca.key"
  ca_certificate = scratch / "parity-ca.pem"
  leaf_key = scratch / "parity-leaf.key"
  leaf_request = scratch / "parity-leaf.csr"
  leaf_certificate = scratch / "parity-leaf.pem"
  extensions = scratch / "parity-leaf.ext"
  names = sorted(proxy.HOST_FAKE)
  extensions.write_text(
      "[server]\nsubjectAltName=" + ",".join(f"DNS:{name}" for name in names) + "\nextendedKeyUsage=serverAuth\n",
      encoding="utf-8")
  _run(["genrsa", "-out", str(ca_key), RSA_BITS])
  _run(
      [
          "req", "-x509", "-new", "-key",
          str(ca_key), "-sha256", "-days", CERTIFICATE_DAYS, "-subj", "/CN=CharlieBot parity fake CA", "-out",
          str(ca_certificate), "-addext", f"basicConstraints={CA_BASIC_CONSTRAINTS}", "-addext",
          f"keyUsage={CA_KEY_USAGE}"
      ])
  _run(["genrsa", "-out", str(leaf_key), RSA_BITS])
  _run(["req", "-new", "-key", str(leaf_key), "-subj", "/CN=parity-fake-services", "-out", str(leaf_request)])
  _run(
      [
          "x509", "-req", "-in",
          str(leaf_request), "-CA",
          str(ca_certificate), "-CAkey",
          str(ca_key), "-CAcreateserial", "-out",
          str(leaf_certificate), "-days", CERTIFICATE_DAYS, "-sha256", "-extfile",
          str(extensions), "-extensions", "server"
      ])
  return Certificates(ca_certificate, leaf_certificate, leaf_key)


def _run(arguments: list[str]) -> None:
  subprocess.run([str(OPENSSL), *arguments], check=True, stdin=subprocess.DEVNULL)
