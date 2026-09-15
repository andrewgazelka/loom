#!/usr/bin/env python3
"""Refresh native V8 inputs from the exact dependency in loom-v8/Cargo.toml."""

import base64
from concurrent.futures import ThreadPoolExecutor
from dataclasses import asdict, dataclass
import hashlib
import json
from pathlib import Path
import tomllib
import urllib.request


DIRECTORY = Path(__file__).resolve().parent
HOSTS = {
    "aarch64-darwin": "aarch64-apple-darwin",
    "x86_64-linux": "x86_64-unknown-linux-gnu",
}


@dataclass(frozen=True)
class Archive:
    url: str
    hash: str


def fetch(url: str) -> Archive:
    digest = hashlib.sha256()
    # Upstream supplies no signed release manifest. These pins identify bytes
    # served by GitHub; updating them requires reviewing the generated diff.
    with urllib.request.urlopen(url, timeout=120) as response:
        while chunk := response.read(1024 * 1024):
            digest.update(chunk)
    return Archive(url=url, hash="sha256-" + base64.b64encode(digest.digest()).decode())


def main() -> None:
    cargo = tomllib.loads((DIRECTORY.parent / "crates/loom-v8/Cargo.toml").read_text())
    dependency = cargo["dependencies"]["v8"]
    requirement = dependency["version"]
    if not requirement.startswith("="):
        raise ValueError("loom-v8 must pin an exact v8 version before refreshing native inputs")
    if dependency["features"] != ["v8_enable_pointer_compression"]:
        raise ValueError("native archive selection expects v8_enable_pointer_compression")
    version = requirement.removeprefix("=")
    prefix = f"https://github.com/denoland/rusty_v8/releases/download/v{version}"
    with ThreadPoolExecutor(max_workers=4) as pool:
        pending = {
            system: {
                "archive": pool.submit(fetch, f"{prefix}/librusty_v8_ptrcomp_release_{target}.a.gz"),
                "bindings": pool.submit(fetch, f"{prefix}/src_binding_ptrcomp_release_{target}.rs"),
            }
            for system, target in HOSTS.items()
        }
        platforms = {
            system: {name: asdict(result.result()) for name, result in assets.items()}
            for system, assets in pending.items()
        }
    output = json.dumps({"version": version, "platforms": platforms}, indent=2) + "\n"
    manifest = DIRECTORY / "v8-manifest.json"
    temporary = manifest.with_suffix(".json.tmp")
    temporary.write_text(output)
    temporary.replace(manifest)
    print(f"Updated {manifest} for V8 {version}")


if __name__ == "__main__":
    main()
