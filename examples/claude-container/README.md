# Claude Code as a Loom container actor

Build the Linux amd64 example image:

```sh
./examples/claude-container/build.sh
```

The builder verifies the pinned npm package's SHA-512 integrity, extracts only
its native executable, and builds without network access. The Dockerfile pins
its Alpine base by digest. No npm lifecycle scripts run. Set `DOCKER` to an
explicit Docker executable and `DOCKER_HOST` when using a remote daemon.

From a Loom actor, with the daemon's container backend enabled:

```ts
const claude = await loom.containers.spawn({
  image: 'loom-claude:2.1.272',
  args: ['--version'],
  network: 'none',
  limits: {memoryMb: 512, cpus: 1, pids: 64},
  ttlMs: 30_000,
  subscriber: await loom.actors.self(),
});
```

The subscriber receives the ordinary `process.output` and `process.exit`
messages. The handle also supports `write`, `closeStdin`, and `cancel` for
commands that accept interactive input. Register the actor under a tenant name
to address it through `loom.actors.named(name)`.

This image verifies real Claude Code startup with `--version`; it contains no
credentials and the example makes no inference request. Authenticated coding
sessions need an explicit credential and network configuration, plus the project
and build tools the session should use. See [Anthropic's container documentation](https://code.claude.com/docs/en/devcontainer).

To update, change the exact npm version, URL and verified integrity in
`package.json` together, then rebuild and run the container through Loom's native
smoke test. The pinned native package is currently Linux amd64 only.
