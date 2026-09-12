#!/usr/bin/env python3
"""Refresh official archive pins after verifying Rust's signed release manifest."""

import argparse
import base64
import hashlib
import json
from pathlib import Path
import subprocess
import tempfile
import tomllib
import urllib.request


RUST_KEY_SHA256 = "e54b09a439647e006b4831eec9785cbaaf3e07ab371c3a6ee6a68e1bdb9fbc6b"
TARGETS = {
    "host-darwin": {"package": "rust", "target": "aarch64-apple-darwin"},
    "host-linux": {"package": "rust", "target": "x86_64-unknown-linux-gnu"},
    "rust-src": {"package": "rust-src", "target": "*"},
}


def download(url: str) -> bytes:
    with urllib.request.urlopen(url, timeout=60) as response:
        return response.read()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("version", nargs="?", default="1.97.0")
    parser.add_argument("--gpg", default="gpg")
    args = parser.parse_args()
    url = f"https://static.rust-lang.org/dist/channel-rust-{args.version}.toml"
    manifest_bytes = download(url)
    key = download("https://static.rust-lang.org/rust-key.gpg.ascii")
    if hashlib.sha256(key).hexdigest() != RUST_KEY_SHA256:
        raise ValueError("Rust signing key changed; review key provenance before updating its pin")
    with tempfile.TemporaryDirectory(prefix="loom-rust-manifest-") as temporary:
        directory = Path(temporary)
        manifest_path = directory / "manifest.toml"
        signature_path = directory / "manifest.toml.asc"
        key_path = directory / "rust-key.asc"
        manifest_path.write_bytes(manifest_bytes)
        signature_path.write_bytes(download(url + ".asc"))
        key_path.write_bytes(key)
        command = [args.gpg, "--homedir", temporary, "--batch"]
        subprocess.run([*command, "--import", str(key_path)], check=True)
        subprocess.run([*command, "--verify", str(signature_path), str(manifest_path)], check=True)
    manifest = tomllib.loads(manifest_bytes.decode())
    archives = {}
    for name, selection in TARGETS.items():
        archive = manifest["pkg"][selection["package"]]["target"][selection["target"]]
        if not archive["available"]:
            raise ValueError(f"Rust {args.version} lacks {selection['target']}")
        archives[name] = {
            "url": archive["xz_url"],
            "hash": "sha256-" + base64.b64encode(bytes.fromhex(archive["xz_hash"])).decode(),
        }
    output = {"version": args.version, "date": manifest["date"], "archives": archives}
    destination = Path(__file__).with_name("rust-toolchain-manifest.json")
    destination.write_text(json.dumps(output, indent=2) + "\n")


if __name__ == "__main__":
    main()
