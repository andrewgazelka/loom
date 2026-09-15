#!/usr/bin/env python3
"""Pin Deno and its exact native esbuild helper for both supported hosts."""

from __future__ import annotations

import argparse
import base64
from concurrent.futures import ThreadPoolExecutor
from dataclasses import asdict, dataclass
import hashlib
import json
from pathlib import Path
import re
import tempfile
import urllib.request

DIRECTORY = Path(__file__).resolve().parent


@dataclass(frozen=True)
class Host:
    deno_target: str
    esbuild_target: str


HOSTS = {
    "aarch64-darwin": Host("aarch64-apple-darwin", "darwin-arm64"),
    "x86_64-linux": Host("x86_64-unknown-linux-gnu", "linux-x64"),
}


@dataclass(frozen=True)
class Archive:
    url: str
    hash: str


def fetch(url: str, cache: Path) -> Archive:
    digest = hashlib.sha256()
    destination = cache / url.rsplit("/", 1)[-1]
    # Release tarballs are pinned by the bytes received. This is not signature
    # verification: upstream release metadata is reviewed with the updated pins.
    with urllib.request.urlopen(url, timeout=120) as response, destination.open("wb") as output:
        while chunk := response.read(1024 * 1024):
            digest.update(chunk)
            output.write(chunk)
    return Archive(url, "sha256-" + base64.b64encode(digest.digest()).decode())


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("version", help="official Deno version, e.g. 2.9.6")
    parser.add_argument("--cache-dir", type=Path)
    args = parser.parse_args()
    if not re.fullmatch(r"\d+\.\d+\.\d+", args.version):
        raise ValueError("expected an exact Deno release version")
    source_url = f"https://raw.githubusercontent.com/denoland/deno/v{args.version}/cli/tools/bundle/esbuild.rs"
    with urllib.request.urlopen(source_url, timeout=30) as response:
        source = response.read().decode()
    version = re.search(r'pub const ESBUILD_VERSION: &str = "([^"]+)";', source)
    cache_version = re.search(r"const ESBUILD_CACHE_VERSION: u8 = (\d+);", source)
    if version is None or cache_version is None or '.join(format!("esbuild-{}", target))' not in source:
        raise ValueError("Deno helper acquisition changed; review wrapper before updating")
    helper_version = version.group(1)
    helper_cache = f"esbuild-{helper_version}-{cache_version.group(1)}"
    with tempfile.TemporaryDirectory(prefix="loom-deno-update-") as temporary:
        cache = args.cache_dir or Path(temporary)
        cache.mkdir(parents=True, exist_ok=True)
        with ThreadPoolExecutor(max_workers=4) as pool:
            pending = {
                system: {
                    "deno": pool.submit(fetch, f"https://github.com/denoland/deno/releases/download/v{args.version}/deno-{host.deno_target}.zip", cache),
                    "esbuild": pool.submit(fetch, f"https://registry.npmjs.org/@esbuild/{host.esbuild_target}/-/{host.esbuild_target}-{helper_version}.tgz", cache),
                }
                for system, host in HOSTS.items()
            }
            platforms = {
                system: {
                    "helper": f"esbuild-{HOSTS[system].esbuild_target}",
                    **{name: asdict(result.result()) for name, result in assets.items()},
                }
                for system, assets in pending.items()
            }
    manifest = {
        "version": args.version,
        "esbuild_version": helper_version,
        "helper_cache": helper_cache,
        "helper_source": {
            "url": source_url,
            "hash": "sha256-" + base64.b64encode(hashlib.sha256(source.encode()).digest()).decode(),
        },
        "platforms": platforms,
    }
    destination = DIRECTORY / "deno-manifest.json"
    temporary = destination.with_suffix(".json.tmp")
    temporary.write_text(json.dumps(manifest, indent=2) + "\n")
    temporary.replace(destination)
    print(f"Pinned Deno {args.version} and esbuild {helper_version}: {destination}")


if __name__ == "__main__":
    main()
