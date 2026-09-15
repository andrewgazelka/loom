#!/usr/bin/env python3
"""Upload a rootfs as tenant CAS blocks and print its immutable VM image reference."""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import http.client
import json
import os
from pathlib import Path
import re
import stat
import sys
from typing import BinaryIO, TypeAlias
from urllib.parse import urlsplit

MAX_FILE_BYTES = 512 * 1024 * 1024
MAX_MANIFEST_BYTES = 16 * 1024 * 1024
MAX_ENTRIES = 65536
CHUNK_BYTES = 1024 * 1024
JsonValue: TypeAlias = (
    "None | bool | int | float | str | list[JsonValue] | dict[str, JsonValue]"
)


@dataclass(frozen=True)
class Reference:
    cid: str

    def json(self) -> dict[str, JsonValue]:
        return {"$ref": self.cid}


class Client:
    def __init__(self, url: str, token: str) -> None:
        address = urlsplit(url)
        if address.scheme not in {"http", "https"} or not address.hostname:
            raise ValueError("URL must be an HTTP or HTTPS service address")
        if address.username or address.password or address.query or address.fragment:
            raise ValueError("URL must not contain credentials, query, or fragment")
        self.host = address.hostname
        self.port = address.port
        self.secure = address.scheme == "https"
        self.route = address.path.rstrip("/") + "/v1/cas"
        self.token = token

    def upload(self, source: BinaryIO, length: int, content_type: str) -> Reference:
        connection = (
            http.client.HTTPSConnection(self.host, self.port, timeout=120)
            if self.secure
            else http.client.HTTPConnection(self.host, self.port, timeout=120)
        )
        try:
            connection.putrequest("POST", self.route)
            connection.putheader("Authorization", f"Bearer {self.token}")
            connection.putheader("Content-Type", content_type)
            connection.putheader("Content-Length", str(length))
            connection.endheaders()
            remaining = length
            while remaining:
                block = source.read(min(CHUNK_BYTES, remaining))
                if not block:
                    raise ValueError("file became shorter during upload")
                connection.send(block)
                remaining -= len(block)
            if source.read(1):
                raise ValueError("file became longer during upload")
            response = connection.getresponse()
            payload = response.read(65537)
            if len(payload) > 65536:
                raise ValueError("CAS upload response exceeds 64 KiB")
            if response.status != 200:
                detail = payload.decode("utf-8", errors="replace")
                raise ValueError(
                    f"CAS upload returned HTTP {response.status}: {detail}"
                )
            value: JsonValue = json.loads(payload)
            if not isinstance(value, dict) or set(value) != {"$ref"}:
                raise ValueError("CAS upload response must contain exactly one $ref")
            cid = value["$ref"]
            if not isinstance(cid, str) or not re.fullmatch(r"b[a-z2-7]+", cid):
                raise ValueError("CAS upload returned an invalid canonical CID")
            return Reference(cid)
        finally:
            connection.close()

    def file(self, file: BinaryIO, length: int) -> Reference:
        if length > MAX_FILE_BYTES:
            raise ValueError("rootfs file exceeds the 512 MiB upload limit")
        return self.upload(file, length, "application/octet-stream")

    def manifest(self, value: JsonValue) -> Reference:
        import io

        encoded = json.dumps(value, separators=(",", ":"), ensure_ascii=False).encode()
        if len(encoded) > MAX_MANIFEST_BYTES:
            raise ValueError("rootfs manifest exceeds 16 MiB")
        return self.upload(io.BytesIO(encoded), len(encoded), "application/json")


def checked_path(parent: str, name: str) -> str:
    path = f"{parent}/{name}" if parent else name
    if not name or name in {".", ".."} or "\\" in name or "\x00" in name:
        raise ValueError(f"unsupported rootfs entry name: {name!r}")
    if len(path.encode()) > 4096:
        raise ValueError("rootfs entry path exceeds 4096 bytes")
    return path


def permissions(info: os.stat_result, path: str) -> int:
    mode = stat.S_IMODE(info.st_mode)
    allowed = 0o1777 if stat.S_ISDIR(info.st_mode) else 0o777
    if mode & ~allowed:
        raise ValueError(f"special permission bits are unsupported: {path}")
    return mode


def same_entry(expected: os.stat_result, actual: os.stat_result, path: str) -> None:
    if expected.st_dev != actual.st_dev or expected.st_ino != actual.st_ino:
        raise ValueError(f"rootfs entry changed during import: {path}")


def walk(
    client: Client, directory_fd: int, parent: str, entries: dict[str, JsonValue]
) -> None:
    # All children are opened relative to retained directory descriptors with
    # O_NOFOLLOW. A replaced directory or symlink cannot redirect a host read.
    with os.scandir(directory_fd) as listing:
        children = sorted(listing, key=lambda entry: entry.name)
    for child in children:
        path = checked_path(parent, child.name)
        if len(entries) >= MAX_ENTRIES:
            raise ValueError("rootfs image exceeds 65536 entries")
        info = child.stat(follow_symlinks=False)
        if stat.S_ISLNK(info.st_mode):
            target = os.readlink(child.name, dir_fd=directory_fd)
            if "\x00" in target or "\\" in target or len(target.encode()) > 4096:
                raise ValueError(f"unsupported symlink target: {path}")
            entries[path] = {"type": "symlink", "target": target}
        elif stat.S_ISDIR(info.st_mode):
            descriptor = os.open(
                child.name,
                os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW,
                dir_fd=directory_fd,
            )
            try:
                same_entry(info, os.fstat(descriptor), path)
                entries[path] = {"type": "directory", "mode": permissions(info, path)}
                walk(client, descriptor, path, entries)
            finally:
                os.close(descriptor)
        elif stat.S_ISREG(info.st_mode):
            descriptor = os.open(
                child.name,
                os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK,
                dir_fd=directory_fd,
            )
            with os.fdopen(descriptor, "rb") as file:
                opened = os.fstat(file.fileno())
                same_entry(info, opened, path)
                if not stat.S_ISREG(opened.st_mode):
                    raise ValueError(f"rootfs entry is no longer regular: {path}")
                mode = permissions(opened, path)
                reference = client.file(file, opened.st_size)
                after = os.fstat(file.fileno())
                if (
                    after.st_size != opened.st_size
                    or after.st_mtime_ns != opened.st_mtime_ns
                ):
                    raise ValueError(f"rootfs file changed during upload: {path}")
            entries[path] = {
                "type": "file",
                "reference": reference.json(),
                "mode": mode,
            }
        else:
            raise ValueError(f"unsupported rootfs entry type: {path}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("rootfs", type=Path)
    parser.add_argument(
        "--url", default=os.environ.get("LOOM_URL", "http://127.0.0.1:8787")
    )
    parser.add_argument("--token", default=os.environ.get("LOOM_TOKEN"))
    parser.add_argument("--arch", choices=["x86_64"], default="x86_64")
    args = parser.parse_args()
    if not args.token:
        parser.error("provide --token or set LOOM_TOKEN")
    try:
        client = Client(args.url, args.token)
        entries: dict[str, JsonValue] = {}
        directory = os.open(args.rootfs, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        try:
            walk(client, directory, "", entries)
        finally:
            os.close(directory)
        reference = client.manifest(
            {"format": "loom.vm.rootfs.v1", "arch": args.arch, "entries": entries}
        )
        print(json.dumps(reference.json(), separators=(",", ":")))
        return 0
    except (OSError, ValueError, http.client.HTTPException) as error:
        print(f"import-vm-image: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
