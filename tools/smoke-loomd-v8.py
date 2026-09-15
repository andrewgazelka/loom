#!/usr/bin/env python3
"""Exercise a prebuilt loomd through its real HTTP/WebSocket listener with bounded cleanup."""

from __future__ import annotations

import argparse
import base64
import hashlib
import http.client
import json
import os
from pathlib import Path
import re
import signal
import socket
import subprocess
import tempfile
import time
from collections.abc import Callable
from dataclasses import dataclass, field

Json = str | int | float | bool | None | list["Json"] | dict[str, "Json"]


def mapping(value: Json) -> dict[str, Json]:
    if not isinstance(value, dict):
        raise AssertionError(f"Expected JSON object: {value!r}")
    return value


def string(value: Json) -> str:
    if not isinstance(value, str):
        raise AssertionError(f"Expected string: {value!r}")
    return value


@dataclass
class Progress:
    total: int
    checks: list[str] = field(default_factory=list)

    def mark(self, check: str) -> None:
        assert check not in self.checks, check
        self.checks.append(check)
        self.emit("running")

    def emit(self, status: str, detail: str = "") -> None:
        print(
            json.dumps(
                {
                    "status": status,
                    "passed": len(self.checks),
                    "total": self.total,
                    "checks": self.checks,
                    "detail": detail,
                },
                ensure_ascii=False,
            ),
            flush=True,
        )


@dataclass
class Client:
    port: int
    deadline: float
    last_command: str = ""

    def timeout(self) -> float:
        remaining = self.deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("daemon smoke deadline exceeded")
        return min(remaining, 90.0)

    def command(self, token: str, command: str, args: dict[str, Json]) -> Json:
        self.last_command = command + " " + json.dumps(args, ensure_ascii=False)[:2048]
        connection = http.client.HTTPConnection(
            "127.0.0.1", self.port, timeout=self.timeout()
        )
        try:
            connection.request(
                "POST",
                "/v1/command",
                json.dumps({"command": command, "args": args}),
                {
                    "Authorization": f"Bearer {token}",
                    "Content-Type": "application/json",
                },
            )
            response = connection.getresponse()
            body = response.read()
            if response.status != 200:
                raise AssertionError(f"{command}: HTTP {response.status}: {body!r}")
            envelope = mapping(json.loads(body))
            if envelope.get("ok") is not True:
                raise AssertionError(f"{command}: {envelope!r}")
            return envelope.get("result")
        finally:
            connection.close()

    def until(self, check: Callable[[], bool]) -> None:
        while True:
            self.timeout()
            if check():
                return
            time.sleep(0.05)

    def sql(self, token: str, actor: str, query: str) -> Json:
        return self.command(token, "sql", {"id": actor, "query": query})

    def spawn(self, token: str, name: str, source: str, init: Json) -> str:
        self.command(
            token, "add", {"name": name, "lang": "typescript", "source": source}
        )
        actor = string(
            mapping(self.command(token, "spawn", {"def": name, "init": init}))["id"]
        )
        self.command(token, "register", {"name": name, "id": actor})
        return actor

    def websocket_echo(self) -> bool:
        key = base64.b64encode(os.urandom(16)).decode()
        with socket.create_connection(
            ("127.0.0.1", self.port), timeout=self.timeout()
        ) as connection:
            connection.sendall(
                (
                    f"GET /v1/actors/socket/websocket HTTP/1.1\r\nHost: 127.0.0.1:{self.port}\r\n"
                    f"Authorization: Bearer alice\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
                    f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
                ).encode()
            )
            header = bytearray()
            while not header.endswith(b"\r\n\r\n"):
                header.extend(receive(connection, 1))
                if len(header) > 16384:
                    raise AssertionError("oversized WebSocket handshake")
            # The init SQL commit can precede asynchronous driver activation.
            if header.startswith(b"HTTP/1.1 409 "):
                return False
            assert header.startswith(b"HTTP/1.1 101 "), header
            expected = base64.b64encode(
                hashlib.sha1(
                    (key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()
                ).digest()
            )
            assert (
                b"sec-websocket-accept: " + expected.lower() in bytes(header).lower()
            ), header
            payload = "native websocket 雪".encode()
            mask = os.urandom(4)
            connection.sendall(
                bytes([0x81, 0x80 | len(payload)])
                + mask
                + bytes(value ^ mask[index % 4] for index, value in enumerate(payload))
            )
            # This fixture sends one short text frame; reject other frame shapes.
            connection.settimeout(self.timeout())
            prefix = receive(connection, 2)
            assert prefix == bytes([0x81, len(payload)]), prefix
            assert receive(connection, len(payload)) == payload
            return True


def receive(connection: socket.socket, count: int) -> bytes:
    result = bytearray()
    while len(result) < count:
        part = connection.recv(count - len(result))
        if not part:
            raise AssertionError("WebSocket closed early")
        result.extend(part)
    return bytes(result)


RECEIVER = """
const LOOM_SCHEMA = "CREATE TABLE arrivals(value TEXT)";
const main = loom.messages.json(async (message: {value?: string}) => {
  if (message.value) await loom.sql("INSERT INTO arrivals VALUES (?)", [message.value]);
});
"""
SENDER = """
const main = loom.messages.json(async message => {
  const peer = await loom.actors.named('receiver');
  await peer.send({value: message.value});
  const process = await loom.processes.named('cat');
  await process.write(message.value + '\\n');
  await process.closeStdin();
});
"""
SOCKET = """
const LOOM_SCHEMA = "CREATE TABLE events(kind TEXT)";
const main = loom.messages.json(async message => {
  if (message.type === 'init') await loom.websockets.listen();
  if (message.type === 'websocket.message') {
    const socket = await loom.websockets.sender();
    await socket.send(message.data.text);
  }
  await loom.sql("INSERT INTO events VALUES (?)", [message.type]);
});
"""


def exercise(client: Client, progress: Progress) -> None:
    process = string(client.command("alice", "whereis", {"name": "cat"}))
    assert client.command("bob", "whereis", {"name": "cat"}) is None
    receiver = client.spawn("alice", "receiver", RECEIVER, {})
    bob_receiver = client.spawn("bob", "receiver", RECEIVER, {"value": "bob-only"})
    assert client.command("alice", "whereis", {"name": "receiver"}) == receiver
    assert client.command("bob", "whereis", {"name": "receiver"}) == bob_receiver
    client.spawn("alice", "sender", SENDER, {"value": "isolate-to-actor 雪"})
    client.until(
        lambda: (
            client.sql("alice", receiver, "SELECT value FROM arrivals")
            == [{"value": "isolate-to-actor 雪"}]
        )
    )
    progress.mark("typescript-durable-sql")
    progress.mark("named-actor-send")
    client.until(
        lambda: (
            client.sql("bob", bob_receiver, "SELECT value FROM arrivals")
            == [{"value": "bob-only"}]
        )
    )
    progress.mark("tenant-name-isolation")
    client.until(
        lambda: (
            client.sql("alice", process, "SELECT phase,code FROM process_state")
            == [{"phase": "completed", "code": 0}]
        )
    )
    output = client.sql(
        "alice",
        process,
        "SELECT CAST(bytes AS TEXT) AS output FROM process_events WHERE stream='stdout' ORDER BY seq",
    )
    assert isinstance(output, list), output
    assert (
        "".join(string(mapping(row)["output"]) for row in output)
        == "isolate-to-actor 雪\n"
    ), output
    progress.mark("named-process-stdout-exit")
    socket_actor = client.spawn("alice", "socket", SOCKET, {"type": "init"})
    client.until(
        lambda: (
            client.sql("alice", socket_actor, "SELECT kind FROM events")
            == [{"kind": "init"}]
        )
    )
    client.until(client.websocket_echo)
    progress.mark("websocket-echo")


NPM = """
import {chunk} from 'npm:lodash-es@4.17.21';
export function main(): number[][] { return chunk([1,2,3], 2); }
"""

CONTAINER = """
const LOOM_SCHEMA = "CREATE TABLE events(body TEXT); CREATE TABLE resource(cap TEXT)";
const main = loom.messages.json(async (message: {type: string, image?: string}) => {
  if (message.type === 'init') {
    const resource = await loom.containers.spawn({image: message.image, command:'/bin/cat', network:'none', ttlMs:120000, subscriber:await loom.actors.self()});
    await loom.sql('INSERT INTO resource VALUES (?)', [JSON.stringify(resource)]);
  } else if (message.type === 'write' || message.type === 'cancel') {
    const rows = await loom.sql('SELECT cap FROM resource');
    const resource = loom.processes.get(JSON.parse(rows[0].cap));
    if (message.type === 'cancel') await resource.cancel();
    else { await resource.write('container echo 雪\\n'); await resource.closeStdin(); }
  } else await loom.sql('INSERT INTO events VALUES (?)', [JSON.stringify(message)]);
});
"""


def lifecycle_source(image: str) -> str:
    # Keep the runnable POC and production smoke on exactly one source fixture.
    assert re.fullmatch(r"[A-Za-z0-9_./:@-]+", image), image
    source = (
        Path(__file__).resolve().parents[1] / "examples/container-actor/main.ts"
    ).read_text()
    default_image = "busybox@sha256:9db7b59979c38555a39def84a31fb98b5296952f9e3afd4f6f11f05b07adfab0"
    assert source.count(default_image) == 1
    return source.replace(default_image, image)


def lifecycle_output(client: Client, actor: str) -> str:
    rows = client.sql(
        "alice",
        actor,
        "SELECT body FROM events WHERE kind='process.output' ORDER BY rowid",
    )
    assert isinstance(rows, list), rows
    output = bytearray()
    for row in rows:
        event = mapping(json.loads(string(mapping(row)["body"])))
        if event.get("stream") == "stdout":
            values = event["bytes"]
            assert isinstance(values, list) and all(
                isinstance(value, int) for value in values
            )
            output.extend(values)
    return output.decode()


def lifecycle_container(client: Client, actor: str) -> str:
    rows = client.sql(
        "alice",
        actor,
        "SELECT json_extract(body,'$.container_id') AS id FROM events WHERE kind='process.started' ORDER BY rowid DESC LIMIT 1",
    )
    assert isinstance(rows, list) and len(rows) == 1, rows
    return string(mapping(rows[0])["id"])


CLAUDE = """
const LOOM_SCHEMA = "CREATE TABLE events(body TEXT)";
const main = loom.messages.json(async (message: {type: string, image?: string}) => {
  if (message.type === 'init') {
    await loom.containers.spawn({image:message.image, args:['--version'], network:'none', ttlMs:60000, subscriber:await loom.actors.self()});
  } else await loom.sql('INSERT INTO events VALUES (?)', [JSON.stringify(message)]);
});
"""


def exercise_claude(client: Client, image: str) -> None:
    actor = client.spawn(
        "alice", "actual-claude", CLAUDE, {"type": "init", "image": image}
    )
    query = "SELECT body FROM events ORDER BY rowid"

    def events() -> list[dict[str, Json]]:
        rows = client.sql("alice", actor, query)
        assert isinstance(rows, list), rows
        return [mapping(json.loads(string(mapping(row)["body"]))) for row in rows]

    wait_process_event(client, events, "process.exit")
    terminal = next(event for event in events() if event.get("type") == "process.exit")
    assert terminal.get("code") == 0, terminal
    output = bytearray()
    for event in events():
        if event.get("type") == "process.output" and event.get("stream") == "stdout":
            values = event["bytes"]
            assert isinstance(values, list) and all(
                isinstance(value, int) for value in values
            )
            output.extend(values)
    assert output.decode().strip() == "2.1.272 (Claude Code)", output


def graceful_stop(process: subprocess.Popen[bytes], deadline: float) -> None:
    if process.poll() is None:
        process.send_signal(signal.SIGTERM)
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise TimeoutError("deadline expired before graceful shutdown")
    status = process.wait(timeout=min(remaining, 15))
    assert status == 0, f"daemon graceful shutdown exit status {status}"


def stop(process: subprocess.Popen[bytes]) -> None:
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        process.wait(timeout=3)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait(timeout=2)


def wait_process_event(
    client: Client, events: Callable[[], list[dict[str, Json]]], expected: str
) -> dict[str, Json]:
    deadline = min(client.deadline, time.monotonic() + 30)
    latest: list[dict[str, Json]] = []
    while time.monotonic() < deadline:
        client.timeout()
        latest = events()
        for event in latest:
            if event.get("type") == expected:
                return event
            if event.get("type") in {
                "process.failed",
                "process.exit",
                "process.cancelled",
                "down",
            }:
                raise AssertionError(
                    f"process terminated before {expected}: {json.dumps(latest, ensure_ascii=False)[-8192:]}"
                )
        time.sleep(0.05)
    raise TimeoutError(
        f"waiting for {expected}; actual events: {json.dumps(latest, ensure_ascii=False)[-8192:]}"
    )


def container_absent(
    result: subprocess.CompletedProcess[bytes], container_id: str
) -> bool:
    if result.returncode == 0:
        return False
    # Docker CLI versions capitalize this differently. Accept only the exact
    # missing-object diagnostic for the inspected ID, never transport failures.
    diagnostic = result.stderr.decode(errors="replace").strip().lower()
    pattern = (
        r"(?:error response from daemon: |error: )?no such (?:object|container): "
        + re.escape(container_id.lower())
    )
    assert re.fullmatch(pattern, diagnostic), result.stderr
    return True


def exercise_containers(
    client: Client, image: str, docker: Path, host: str | None
) -> None:
    invocation = [str(docker)] + (["--host", host] if host else [])
    for mode in ["write", "cancel"]:
        actor = client.spawn(
            "alice", "container-" + mode, CONTAINER, {"type": "init", "image": image}
        )

        def events() -> list[dict[str, Json]]:
            rows = client.sql("alice", actor, "SELECT body FROM events ORDER BY rowid")
            assert isinstance(rows, list), rows
            return [mapping(json.loads(string(mapping(row)["body"]))) for row in rows]

        started = wait_process_event(client, events, "process.started")
        container_id = string(started["container_id"])
        control = subprocess.run(
            invocation + ["inspect", container_id],
            capture_output=True,
            timeout=client.timeout(),
        )
        assert control.returncode == 0, control.stderr
        client.command("alice", "send", {"id": actor, "msg": {"type": mode}})
        terminal = "process.exit" if mode == "write" else "process.cancelled"
        wait_process_event(client, events, terminal)
        if mode == "write":
            terminal_event = next(
                event for event in events() if event.get("type") == terminal
            )
            assert terminal_event.get("code") == 0, terminal_event
            output = bytearray()
            for event in events():
                if (
                    event.get("type") == "process.output"
                    and event.get("stream") == "stdout"
                ):
                    values = event["bytes"]
                    assert isinstance(values, list) and all(
                        isinstance(value, int) for value in values
                    )
                    output.extend(values)
            assert output.decode() == "container echo 雪\n", output

        def removed() -> bool:
            result = subprocess.run(
                invocation + ["inspect", container_id],
                capture_output=True,
                timeout=client.timeout(),
            )
            return container_absent(result, container_id)

        client.until(removed)


def wait_listener(
    process: subprocess.Popen[bytes], log_path: Path, deadline: float
) -> Client:
    while True:
        text = log_path.read_text(errors="replace")
        match = re.search(r"loomd listening on 127\.0\.0\.1:(\d+)", text)
        if match:
            return Client(int(match.group(1)), deadline)
        if process.poll() is not None or time.monotonic() >= deadline:
            raise AssertionError(f"daemon failed to listen: {text}")
        time.sleep(0.05)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path, help="fixed prebuilt loomd executable")
    parser.add_argument(
        "--root", type=Path, default=Path(__file__).resolve().parents[1]
    )
    parser.add_argument("--timeout", type=float, default=180)
    parser.add_argument(
        "--npm",
        action="store_true",
        help="admit pinned external npm import and verify offline restart",
    )
    parser.add_argument("--docker-executable", type=Path)
    parser.add_argument("--docker-host")
    parser.add_argument("--docker-image", help="preloaded Docker image with /bin/cat")
    parser.add_argument(
        "--claude-image",
        help="preloaded actual Claude Code image; runs --version without credentials",
    )
    args = parser.parse_args()
    binary = args.binary.absolute()
    assert binary.is_file() and os.access(binary, os.X_OK), binary
    deadline = time.monotonic() + args.timeout
    progress = Progress(
        5
        + int(args.npm)
        + 2 * int(bool(args.docker_image))
        + int(bool(args.claude_image))
    )
    client: Client | None = None
    with tempfile.TemporaryDirectory(prefix="loomd-v8-smoke-") as temporary:
        workspace = Path(temporary)
        process_root = workspace / "process"
        process_root.mkdir()
        busybox = Path(os.environ["LOOM_STATIC_BUSYBOX"]).resolve()
        assert busybox.is_file() and os.access(busybox, os.X_OK), busybox
        tokens = [
            {"token": tenant, "tenant": tenant, "scopes": ["read", "execute", "define"]}
            for tenant in ["alice", "bob"]
        ]
        (workspace / "tokens.json").write_text(json.dumps(tokens))
        presets = [
            {
                "tenant": "alice",
                "name": "cat",
                "sandbox": {"readonly": [str(busybox)], "network": False},
                "spec": {
                    "machine": "local",
                    "program": str(busybox),
                    "args": ["cat"],
                    "cwd": str(process_root),
                    "root": str(process_root),
                    "env": {},
                    "capture_paths": [],
                },
            }
        ]
        (workspace / "presets.json").write_text(json.dumps(presets))
        log_path = workspace / "daemon.log"
        with log_path.open("wb") as log:
            environment = os.environ.copy()
            # An inherited token conflicts with the explicit token-file CLI.
            environment.pop("LOOM_TOKEN", None)
            invocation = [
                str(binary),
                "--root",
                str(args.root.resolve()),
                "--db",
                str(workspace / "default.sqlite"),
                "--actors-dir",
                str(workspace / "actors"),
                "--tenant-root",
                str(workspace / "tenants"),
                "--tokens-file",
                str(workspace / "tokens.json"),
                "--processes-file",
                str(workspace / "presets.json"),
                "--bind",
                "127.0.0.1:0",
            ]
            if args.docker_executable:
                invocation += [
                    "--docker-executable",
                    str(args.docker_executable.resolve()),
                ]
            if args.docker_host:
                invocation += ["--docker-host", args.docker_host]
            process = subprocess.Popen(
                invocation,
                cwd=workspace,
                env=environment,
                stdout=log,
                stderr=log,
                start_new_session=True,
            )
            try:
                while True:
                    text = log_path.read_text(errors="replace")
                    match = re.search(r"loomd listening on 127\.0\.0\.1:(\d+)", text)
                    if match:
                        break
                    if process.poll() is not None or time.monotonic() >= deadline:
                        raise AssertionError(f"loomd failed to listen: {text}")
                    time.sleep(0.05)
                client = Client(int(match.group(1)), deadline)
                exercise(client, progress)
                if args.docker_image:
                    assert args.docker_executable, (
                        "--docker-image requires --docker-executable"
                    )
                    exercise_containers(
                        client,
                        args.docker_image,
                        args.docker_executable,
                        args.docker_host,
                    )
                    progress.mark("container-stdin-exit-cancel-removal")
                if args.claude_image:
                    assert args.docker_executable, (
                        "--claude-image requires --docker-executable"
                    )
                    exercise_claude(client, args.claude_image)
                    progress.mark("actual-claude-code-version")
                if args.docker_image:
                    client.command(
                        "alice",
                        "add",
                        {
                            "name": "lifecycle",
                            "lang": "typescript",
                            "source": lifecycle_source(args.docker_image),
                        },
                    )
                if args.npm:
                    client.command(
                        "alice",
                        "add",
                        {"name": "npm-chunk", "lang": "typescript", "source": NPM},
                    )
                    assert mapping(
                        client.command("alice", "run", {"target": "npm-chunk"})
                    )["output"] == [[1, 2], [3]]
                    graceful_stop(process, deadline)
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
                    while True:
                        text = log_path.read_text(errors="replace")
                        match = re.search(
                            r"loomd listening on 127\.0\.0\.1:(\d+)", text
                        )
                        if match:
                            break
                        if process.poll() is not None or time.monotonic() >= deadline:
                            raise AssertionError(f"offline restart failed: {text}")
                        time.sleep(0.05)
                    client = Client(int(match.group(1)), deadline)
                    assert mapping(
                        client.command("alice", "run", {"target": "npm-chunk"})
                    )["output"] == [[1, 2], [3]]
                    progress.mark("npm-admission-offline-restart")
                if args.docker_image:
                    actor = string(
                        mapping(
                            client.command(
                                "alice", "spawn", {"def": "lifecycle", "init": None}
                            )
                        )["id"]
                    )
                    client.until(lambda: lifecycle_output(client, actor) == "ready\n")
                    first_container = lifecycle_container(client, actor)
                    client.command(
                        "alice",
                        "send",
                        {
                            "id": actor,
                            "msg": {"type": "write", "value": "before restart 雪\n"},
                        },
                    )
                    client.until(
                        lambda: (
                            lifecycle_output(client, actor)
                            == "ready\nbefore restart 雪\n"
                        )
                    )
                    docker_command = [str(args.docker_executable)] + (
                        ["--host", args.docker_host] if args.docker_host else []
                    )
                    alive = subprocess.run(
                        docker_command + ["inspect", first_container],
                        capture_output=True,
                        timeout=client.timeout(),
                    )
                    assert alive.returncode == 0, alive.stderr
                    graceful_stop(process, deadline)

                    def shutdown_removed() -> bool:
                        inspected = subprocess.run(
                            docker_command + ["inspect", first_container],
                            capture_output=True,
                            timeout=client.timeout(),
                        )
                        return container_absent(inspected, first_container)

                    client.until(shutdown_removed)
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
                    client = wait_listener(process, log_path, deadline)
                    client.until(
                        lambda: (
                            lifecycle_output(client, actor)
                            == "ready\nbefore restart 雪\nready\n"
                        )
                    )
                    second_container = lifecycle_container(client, actor)
                    assert second_container != first_container
                    client.command(
                        "alice",
                        "send",
                        {
                            "id": actor,
                            "msg": {"type": "write", "value": "after restart 雪\n"},
                        },
                    )
                    client.until(
                        lambda: (
                            lifecycle_output(client, actor)
                            == "ready\nbefore restart 雪\nready\nafter restart 雪\n"
                        )
                    )
                    assert client.sql(
                        "alice",
                        actor,
                        "SELECT kind, COUNT(*) AS count FROM events WHERE kind IN ('start','stop','input') GROUP BY kind ORDER BY kind",
                    ) == [
                        {"kind": "input", "count": 2},
                        {"kind": "start", "count": 2},
                        {"kind": "stop", "count": 1},
                    ]
                    progress.mark("lifecycle-shutdown-restart-resource-roundtrip")
                graceful_stop(process, deadline)
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
                stop(process)


if __name__ == "__main__":
    main()
