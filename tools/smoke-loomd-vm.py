#!/usr/bin/env python3
"""Actual Linux/KVM daemon POC; requires configured runner/runtime and static busybox."""

from __future__ import annotations

import argparse
import http.client
import importlib.util
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time
from urllib.parse import quote
from dataclasses import dataclass

# Reuse the native daemon harness's validated shutdown and failure reporting.
SCRIPTS = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location(
    "loom_smoke", SCRIPTS / "smoke-loomd-v8.py"
)
assert spec and spec.loader
harness = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = harness
spec.loader.exec_module(harness)
Json = str | int | float | bool | None | list["Json"] | dict[str, "Json"]

PROBE = r"""#!/bin/sh
/bin/busybox stty -echo -onlcr 2>/dev/null || true
printf 'KERNEL=%s\n' "$(/bin/busybox uname -r)"
printf 'ENV=%s\nCWD=%s\n' "$LOOM_VM_PROBE" "$PWD"
/bin/busybox awk '/MemTotal:/ {print "MEM_KIB=" $2}' /proc/meminfo
/bin/busybox awk '/^processor/ {n++} END {print "CPUS=" n}' /proc/cpuinfo
# Capture diagnostics in memory: the guest tmpfs being filled also backs /tmp.
if disk_error=$(/bin/busybox dd if=/dev/zero of=/work/disk-probe bs=1048576 count=65 2>&1); then
  echo DISK_LIMIT=unbounded
elif printf '%s\n' "$disk_error" | /bin/busybox grep -q "No space left on device"; then
  echo DISK_LIMIT=bounded
else
  printf 'DISK_LIMIT=unexpected-error:%s\n' "$disk_error"
fi
/bin/busybox rm -f /work/disk-probe
if /bin/busybox nc -w 1 127.0.0.1 "$1" </dev/null >/dev/null 2>&1; then
  echo NETWORK=reachable
else
  echo NETWORK=isolated
fi
printf 'READY\n'
while IFS= read -r line; do printf 'ECHO:%s\n' "$line"; done
"""

RECEIVER = """
const LOOM_SCHEMA = "CREATE TABLE received(value TEXT)";
const main = loom.messages.json(async message => {
  if (!message?.raw) return;
  const raw = await loom.cas.get(message.raw);
  const document = await loom.cas.getJson(message.document);
  await loom.sql('INSERT INTO received VALUES (?)', [JSON.stringify({raw,document})]);
});
"""


NEGATIVE = """
const LOOM_SCHEMA = "CREATE TABLE events(kind TEXT, body TEXT)";
const main = loom.messages.json(async message => {
  if (message?.type === 'init') {
    await loom.vms.spawn({image:message.image,command:'/bin/sh',args:['-c','echo UNEXPECTED_VM_SUCCESS'],network:'none',limits:{memoryMb:256,cpus:1,rootfsMb:64},ttlMs:15000,subscriber:await loom.actors.self()});
  } else if (message?.type) await loom.sql('INSERT INTO events VALUES (?, ?)', [message.type, JSON.stringify(message)]);
});
"""


@dataclass
class VmProbe:
    client: harness.Client
    actor: str

    def events(self) -> list[dict[str, Json]]:
        rows = self.client.sql(
            "alice",
            self.actor,
            "SELECT body FROM events WHERE kind LIKE 'process.%' OR kind='down' ORDER BY rowid",
        )
        if isinstance(rows, dict):
            assert set(rows) == {"$ref"}, rows
            reference = harness.string(rows["$ref"])
            # Large command results are DAG-CBOR CAS blocks. Request the
            # authenticated JSON representation instead of decoding raw CBOR.
            response = request(
                self.client,
                "alice",
                "GET",
                "/v1/cas/" + quote(reference, safe=""),
                accept="application/json",
            )
            assert response["status"] == 200, response
            rows = decode_response(response)
        assert isinstance(rows, list), rows
        return [
            harness.mapping(json.loads(harness.string(harness.mapping(row)["body"])))
            for row in rows
        ]

    def output(self) -> str:
        output = bytearray()
        for event in self.events():
            if (
                event.get("type") == "process.output"
                and event.get("stream") == "stdout"
            ):
                values = event["bytes"]
                assert isinstance(values, list) and all(
                    isinstance(value, int) for value in values
                ), values
                output.extend(values)
        return output.decode(errors="replace").replace("\r", "")

    def wait_output(self, witness: str, occurrences: int = 1) -> str:
        deadline = min(self.client.deadline, time.monotonic() + 40)
        while time.monotonic() < deadline:
            output = self.output()
            if output.count(witness) >= occurrences:
                return output
            events = self.events()
            # A reopened actor still contains the prior incarnation's terminal
            # events until the replacement starts. Do not mislabel that history.
            current_incarnation = (
                occurrences == 1
                or sum(event.get("type") == "process.started" for event in events)
                >= occurrences
            )
            if (
                current_incarnation
                and events
                and events[-1].get("type")
                in {
                    "process.exit",
                    "process.failed",
                    "down",
                }
            ):
                raise AssertionError(
                    f"VM ended before {witness}: {json.dumps(events)[-8192:]}"
                )
            time.sleep(0.05)
        raise TimeoutError(
            f"VM missing {witness}; events={json.dumps(self.events())[-8192:]}"
        )

    def started(self) -> dict[str, Json]:
        events = [
            event for event in self.events() if event.get("type") == "process.started"
        ]
        assert events, self.events()
        return events[-1]


def require_vm_failure(probe: VmProbe) -> None:
    deadline = min(probe.client.deadline, time.monotonic() + 25)
    while time.monotonic() < deadline:
        events = probe.events()
        assert "UNEXPECTED_VM_SUCCESS" not in probe.output(), events
        for event in events:
            if event.get("type") == "down":
                assert event.get("reason"), event
                return
            if event.get("type") == "process.exit":
                assert isinstance(event.get("code"), int) and event["code"] != 0, event
                return
        time.sleep(0.05)
    raise TimeoutError(f"VM failure not reported: {json.dumps(probe.events())[-8192:]}")


def vm_process_tree(pid: int) -> set[int]:
    """Capture the live launcher and descendants, including its VMM child."""
    found = {pid}
    pending = [pid]
    while pending:
        parent = pending.pop()
        children = Path(f"/proc/{parent}/task/{parent}/children").read_text()
        for child in map(int, children.split()):
            if child not in found:
                found.add(child)
                pending.append(child)
    return found


def assert_vm_gone(pids: set[int]) -> None:
    # The host reaper can remove orphaned namespace children after loomd exits.
    # Require actual disappearance of every captured PID within a bounded wait.
    deadline = time.monotonic() + 5
    while True:
        remaining = [pid for pid in pids if Path(f"/proc/{pid}").exists()]
        if not remaining:
            return
        if time.monotonic() >= deadline:
            raise AssertionError(f"VM processes survived graceful shutdown: {remaining}")
        time.sleep(0.02)


def request(
    client: harness.Client,
    token: str,
    method: str,
    path: str,
    body: bytes | None = None,
    content_type: str = "application/octet-stream",
    accept: str = "application/octet-stream",
) -> dict[str, Json]:
    connection = http.client.HTTPConnection(
        "127.0.0.1", client.port, timeout=client.timeout()
    )
    try:
        connection.request(
            method,
            path,
            body,
            {
                "Authorization": f"Bearer {token}",
                "Content-Type": content_type,
                "Accept": accept,
            },
        )
        response = connection.getresponse()
        payload = response.read()
        return {"status": response.status, "body": list(payload)}
    finally:
        connection.close()


def decode_response(response: dict[str, Json]) -> Json:
    body = response["body"]
    assert isinstance(body, list)
    return json.loads(bytes(body))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--timeout", type=float, default=240)
    args = parser.parse_args()
    assert sys.platform == "linux", "VM smoke requires native Linux/KVM"
    assert os.access("/dev/kvm", os.R_OK | os.W_OK), "/dev/kvm unavailable"
    binary = args.binary.absolute()
    assert binary.is_file() and os.access(binary, os.X_OK), binary
    deadline = time.monotonic() + args.timeout
    progress = harness.Progress(9)
    process = None
    client = None
    with tempfile.TemporaryDirectory(prefix="loom-vm-smoke-") as temporary:
        workspace = Path(temporary)
        rootfs = workspace / "rootfs"
        (rootfs / "bin").mkdir(parents=True)
        (rootfs / "work").mkdir()
        (rootfs / "proc").mkdir()
        (rootfs / "dev").mkdir()
        (rootfs / "tmp").mkdir()
        busybox = Path(os.environ["LOOM_STATIC_BUSYBOX"]).resolve()
        (rootfs / "bin/busybox").write_bytes(busybox.read_bytes())
        (rootfs / "bin/busybox").chmod(0o755)
        (rootfs / "bin/sh").symlink_to("busybox")
        (rootfs / "work/probe.sh").write_text(PROBE)
        (rootfs / "work/probe.sh").chmod(0o755)
        tokens = [
            {"token": tenant, "tenant": tenant, "scopes": ["read", "execute", "define"]}
            for tenant in ["alice", "bob"]
        ]
        (workspace / "tokens.json").write_text(json.dumps(tokens))
        environment = os.environ.copy()
        environment.pop("LOOM_TOKEN", None)
        invocation = [
            str(binary),
            "--root",
            str(SCRIPTS.parent),
            "--db",
            str(workspace / "default.sqlite"),
            "--actors-dir",
            str(workspace / "actors"),
            "--tenant-root",
            str(workspace / "tenants"),
            "--tokens-file",
            str(workspace / "tokens.json"),
            "--bind",
            "127.0.0.1:0",
        ]
        for flag, key in {
            "--vm-runner": "LOOM_TEST_VM_RUNNER",
            "--vm-library": "LOOM_TEST_VM_LIBRARY",
            "--vm-bwrap": "LOOM_TEST_VM_BWRAP",
        }.items():
            invocation += [flag, os.environ[key]]
        runtime_roots = os.environ["LOOM_TEST_VM_RUNTIME_ROOT"].split(os.pathsep)
        assert runtime_roots and all(runtime_roots), "empty VM runtime root"
        for runtime_root in runtime_roots:
            invocation += ["--vm-runtime-root", runtime_root]
        log_path = workspace / "daemon.log"
        with log_path.open("wb") as log, socket.socket() as endpoint:
            endpoint.bind(("127.0.0.1", 0))
            endpoint.listen(2)
            endpoint.settimeout(1)
            port = endpoint.getsockname()[1]
            # Host positive control proves the network destination exists.
            with socket.create_connection(("127.0.0.1", port), timeout=1):
                accepted, _address = endpoint.accept()
                accepted.close()
            try:
                process = subprocess.Popen(
                    invocation,
                    cwd=workspace,
                    env=environment,
                    stdout=log,
                    stderr=log,
                    start_new_session=True,
                )
                client = harness.wait_listener(process, log_path, deadline)
                import_environment = environment | {"LOOM_TOKEN": "alice"}
                imported = subprocess.run(
                    [
                        sys.executable,
                        str(SCRIPTS / "import-vm-image.py"),
                        str(rootfs),
                        "--url",
                        f"http://127.0.0.1:{client.port}",
                    ],
                    env=import_environment,
                    capture_output=True,
                    timeout=client.timeout(),
                )
                assert imported.returncode == 0, imported.stderr
                image = harness.mapping(json.loads(imported.stdout))
                image_cid = harness.string(image["$ref"])
                alice = request(client, "alice", "GET", "/v1/cas/" + image_cid)
                assert alice["status"] == 200, alice
                bob = request(client, "bob", "GET", "/v1/cas/" + image_cid)
                assert bob["status"] == 404, bob
                progress.mark("streamed-cas-image-and-tenant-denial")
                client.command(
                    "alice",
                    "add",
                    {"name": "vm-negative", "lang": "typescript", "source": NEGATIVE},
                )
                invalid = request(
                    client,
                    "alice",
                    "POST",
                    "/v1/cas",
                    json.dumps({"format": "not-a-vm-image"}).encode(),
                    "application/json",
                )
                assert invalid["status"] in [200, 201], invalid
                invalid_image = harness.mapping(decode_response(invalid))
                failed_actor = harness.string(
                    harness.mapping(
                        client.command(
                            "alice",
                            "spawn",
                            {
                                "def": "vm-negative",
                                "init": {"type": "init", "image": invalid_image},
                            },
                        )
                    )["id"]
                )
                require_vm_failure(VmProbe(client, failed_actor))
                progress.mark("invalid-vm-image-reports-failure")
                receiver = client.spawn("alice", "cas-receiver", RECEIVER, None)
                source = (SCRIPTS.parent / "examples/vm-actor/main.ts").read_text()
                actor = client.spawn(
                    "alice",
                    "vm-owner",
                    source,
                    {"type": "configure", "image": image, "port": port},
                )
                probe = VmProbe(client, actor)
                client.until(
                    lambda: bool(
                        client.sql("alice", receiver, "SELECT value FROM received")
                    )
                )
                received = client.sql("alice", receiver, "SELECT value FROM received")
                data = harness.mapping(
                    json.loads(harness.string(harness.mapping(received[0])["value"]))
                )
                assert data["raw"] == [0, 1, 127, 255], data
                assert harness.mapping(data["document"])["purpose"] == "vm-poc", data
                progress.mark("guest-cas-references-and-actor-message")
                output = probe.wait_output("READY\n")
                kernel = next(
                    line.split("=", 1)[1]
                    for line in output.splitlines()
                    if line.startswith("KERNEL=")
                )
                assert kernel != os.uname().release, (
                    f"guest kernel equals host: {kernel}"
                )
                started = probe.started()
                assert (
                    started["network"] == "none"
                    and started["memoryMb"] == 256
                    and started["cpus"] == 1
                    and started["rootfsMb"] == 64
                ), started
                progress.mark("native-kvm-linux-kernel")
                assert "ENV=guest-env-雪\n" in output and "CWD=/work\n" in output, (
                    output
                )
                client.command(
                    "alice",
                    "send",
                    {
                        "id": actor,
                        "msg": {"type": "write", "value": "before restart 雪\n"},
                    },
                )
                probe.wait_output("ECHO:before restart 雪\n")
                progress.mark("vm-console-unicode-env-cwd")
                memory = int(
                    next(
                        line.split("=", 1)[1]
                        for line in output.splitlines()
                        if line.startswith("MEM_KIB=")
                    )
                )
                assert 128 * 1024 <= memory <= 256 * 1024, memory
                assert (
                    "CPUS=1\n" in output
                    and "DISK_LIMIT=bounded\n" in output
                    and "NETWORK=isolated\n" in output
                ), output
                progress.mark("guest-memory-cpu-disk-network-limits")
                first_vm = harness.string(started["vm_id"])
                first_pid = started.get("host_pid")
                assert isinstance(first_pid, int), started
                assert Path(f"/proc/{first_pid}").exists(), first_pid
                first_processes = vm_process_tree(first_pid)
                harness.graceful_stop(process, deadline)
                assert_vm_gone(first_processes)
                progress.mark("graceful-shutdown-vm-process-gone")
                environment["LOOM_DENO"] = str(
                    workspace / "disabled-admission-compiler"
                )
                log.seek(0)
                log.truncate()
                process = subprocess.Popen(
                    invocation,
                    cwd=workspace,
                    env=environment,
                    stdout=log,
                    stderr=log,
                    start_new_session=True,
                )
                client = harness.wait_listener(process, log_path, deadline)
                probe.client = client
                probe.wait_output("READY\n", 2)
                assert probe.started()["vm_id"] != first_vm
                client.command(
                    "alice",
                    "send",
                    {
                        "id": actor,
                        "msg": {"type": "write", "value": "after restart 雪\n"},
                    },
                )
                output = probe.wait_output("ECHO:after restart 雪\n")
                assert (
                    output.count("ECHO:before restart 雪\n") == 1
                    and output.count("ECHO:after restart 雪\n") == 1
                ), output
                assert client.sql(
                    "alice",
                    actor,
                    "SELECT kind,COUNT(*) AS count FROM events WHERE kind IN ('start','stop','input') GROUP BY kind ORDER BY kind",
                ) == [
                    {"kind": "input", "count": 2},
                    {"kind": "start", "count": 2},
                    {"kind": "stop", "count": 1},
                ]
                second_pid = probe.started().get("host_pid")
                assert isinstance(second_pid, int), probe.started()
                second_processes = vm_process_tree(second_pid)
                harness.graceful_stop(process, deadline)
                assert_vm_gone(second_processes)
                progress.mark("durable-vm-recreation-without-input-replay")

                # Preserve the production launcher and mask only its device source.
                # /dev/null cannot implement KVM ioctls: this is a real runner
                # failure control, not a replacement runner returning an error.
                masked_bwrap = workspace / "bwrap-without-kvm"
                real_bwrap = os.environ["LOOM_TEST_VM_BWRAP"]
                masked_bwrap.write_text(
                    "#!"
                    + sys.executable
                    + '\nimport os,sys\nargs=sys.argv[1:]\nfor i in range(len(args)-2):\n    if args[i:i+3] == ["--dev-bind","/dev/kvm","/dev/kvm"]:\n        args[i+1]="/dev/null"\n        break\nelse:\n    raise SystemExit("KVM bind missing from production invocation")\nos.execv('
                    + repr(real_bwrap)
                    + ", ["
                    + repr(real_bwrap)
                    + "] + args)\n"
                )
                masked_bwrap.chmod(0o755)
                masked_invocation = invocation.copy()
                masked_invocation[masked_invocation.index("--vm-bwrap") + 1] = str(
                    masked_bwrap
                )
                log.seek(0)
                log.truncate()
                process = subprocess.Popen(
                    masked_invocation,
                    cwd=workspace,
                    env=environment,
                    stdout=log,
                    stderr=log,
                    start_new_session=True,
                )
                client = harness.wait_listener(process, log_path, deadline)
                failed_actor = harness.string(
                    harness.mapping(
                        client.command(
                            "alice",
                            "spawn",
                            {
                                "def": "vm-negative",
                                "init": {"type": "init", "image": image},
                            },
                        )
                    )["id"]
                )
                require_vm_failure(VmProbe(client, failed_actor))
                harness.graceful_stop(process, deadline)
                progress.mark("unusable-kvm-reports-failure-without-process-fallback")

                progress.emit("passed")
            except BaseException as error:
                progress.emit(
                    "failed",
                    f"{type(error).__name__}: {error}; last command: {client.last_command if client else 'startup'}",
                )
                print(
                    "daemon log tail:\n"
                    + log_path.read_text(errors="replace")[-16384:],
                    flush=True,
                )
                raise
            finally:
                if process is not None:
                    harness.stop(process)


if __name__ == "__main__":
    main()
