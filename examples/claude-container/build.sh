#!/usr/bin/env bash
set -euo pipefail
source_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
context=$(mktemp -d)
trap 'rm -rf -- "$context"' EXIT
python3 - "$source_dir/package.json" "$context" <<'PY'
import base64
import hashlib
import json
import pathlib
import sys
import tarfile
import urllib.request

manifest = json.loads(pathlib.Path(sys.argv[1]).read_text())
context = pathlib.Path(sys.argv[2])
archive = context / "package.tgz"
with urllib.request.urlopen(manifest["url"], timeout=60) as response:
    with archive.open("wb") as output:
        while chunk := response.read(1024 * 1024):
            output.write(chunk)
with archive.open("rb") as downloaded:
    integrity = "sha512-" + base64.b64encode(hashlib.file_digest(downloaded, "sha512").digest()).decode()
if integrity != manifest["integrity"]:
    raise SystemExit("Claude Code package integrity mismatch")
with tarfile.open(archive) as package:
    member = package.getmember("package/claude")
    if not member.isfile():
        raise SystemExit("Claude Code package has no regular executable")
    with package.extractfile(member) as source, (context / "claude").open("wb") as target:
        while chunk := source.read(1024 * 1024):
            target.write(chunk)
(context / "claude").chmod(0o755)
archive.unlink()
PY
cp -- "$source_dir/Dockerfile" "$context/Dockerfile"
"${DOCKER:-docker}" build --network=none --platform=linux/amd64 -t "${1:-loom-claude:2.1.272}" "$context"
