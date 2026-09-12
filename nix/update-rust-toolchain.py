#!/usr/bin/env python3
"""Refresh official archive pins after verifying Rust's signed release manifests."""

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
HOSTS = {
    "aarch64-darwin": "aarch64-apple-darwin",
    "x86_64-linux": "x86_64-unknown-linux-gnu",
}
# Which official archives each pinned toolchain is assembled from. `host` is the
# compiler that builds loomd and the loom CLI: one combined archive and the
# standard-library source. `guest` is the compiler the daemon ships to guests:
# `rustc-dev` carries the compiler's own crates, which tools/hash-rustc links
# against (`extern crate rustc_driver`), `llvm-tools-preview` carries the
# `llvm-objcopy` its object cache runs, and guests compile to wasm32.
ENTRIES = {
    "host": {
        "host": ("rust",),
        "shared": (("rust-src", "*"),),
    },
    "guest": {
        "host": ("rustc", "cargo", "rust-std", "rustc-dev", "llvm-tools-preview"),
        "shared": (("rust-src", "*"), ("rust-std", "wasm32-unknown-unknown")),
    },
}
MANIFEST = Path(__file__).with_name("rust-toolchain-manifest.json")


def download(url: str) -> bytes:
    with urllib.request.urlopen(url, timeout=60) as response:
        return response.read()


def channel_manifest(channel: str, gpg: str) -> dict:
    """Fetch one release manifest and verify Rust's detached signature over it."""
    # A dated channel ("nightly-2026-08-24") names that day's archived manifest;
    # a bare version ("1.97.0") names a stable release manifest.
    name, _, date = channel.partition("-")
    prefix = f"https://static.rust-lang.org/dist/{date}/" if date else "https://static.rust-lang.org/dist/"
    url = f"{prefix}channel-rust-{name}.toml"
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
        command = [gpg, "--homedir", temporary, "--batch"]
        subprocess.run([*command, "--import", str(key_path)], check=True)
        subprocess.run([*command, "--verify", str(signature_path), str(manifest_path)], check=True)
    manifest = tomllib.loads(manifest_bytes.decode())
    if date and manifest["date"] != date:
        raise ValueError(f"{url} is dated {manifest['date']}, not {date}")
    return manifest


def entry(channel: str, selection: dict, gpg: str) -> dict:
    manifest = channel_manifest(channel, gpg)

    def archive(package: str, target: str) -> dict[str, str]:
        pinned = manifest["pkg"][package]["target"][target]
        if not pinned["available"]:
            raise ValueError(f"Rust {channel} lacks {package} for {target}")
        return {
            "url": pinned["xz_url"],
            "hash": "sha256-" + base64.b64encode(bytes.fromhex(pinned["xz_hash"])).decode(),
        }

    archives = {
        system: {package: archive(package, target) for package in selection["host"]}
        for system, target in HOSTS.items()
    }
    archives["shared"] = {
        package if target == "*" else f"{package}-{target}": archive(package, target)
        for package, target in selection["shared"]
    }
    version = manifest["pkg"]["rust"]["version"].split()[0]
    date = manifest["date"]
    return {
        "channel": channel,
        # A dated channel reuses one version string every day, so the store path
        # would not move when the pin does; the date makes each pin its own.
        "version": version if channel == version else f"{version}-{date}",
        "date": date,
        "archives": archives,
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--entry", choices=sorted(ENTRIES), action="append")
    parser.add_argument("--channel", help="new channel for the single selected entry")
    parser.add_argument("--gpg", default="gpg")
    args = parser.parse_args()
    if args.channel and (args.entry is None or len(args.entry) != 1):
        raise SystemExit("--channel applies to exactly one --entry")
    pinned = json.loads(MANIFEST.read_text())["toolchains"]
    names = args.entry or sorted(ENTRIES)
    for name in names:
        channel = args.channel if args.channel else pinned[name]["channel"]
        pinned[name] = entry(channel, ENTRIES[name], args.gpg)
    MANIFEST.write_text(json.dumps({"toolchains": pinned}, indent=2) + "\n")


if __name__ == "__main__":
    main()
