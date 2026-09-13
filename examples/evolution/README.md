# Scripted function updates

Use the same commands from scripts and agents. An update changes the named definition and rebuilds its dependents. A type error leaves live names unchanged and returns a durable repair session. Submit all caller fixes together to that session.

```sh
export LOOM_URL=http://127.0.0.1:8787
export LOOM_TOKEN_FILE=/path/to/token
bun examples/evolution/update.ts start increment ./increment.rs
```

Pass the hash observed when the agent read the code as the final argument:

```sh
bun examples/evolution/update.ts start increment ./increment.rs ORIGINAL_HASH
```

The script sends that hash as `expected_hash`. If another agent changed that name, the update fails instead of overwriting its work. Without the final argument, the script reads the current hash immediately before submitting. That default only detects changes during submission; agents generating edits over time should pass their original hash.

When the response reports `needs_repair`, retain its update `id` and `revision`. Read the session, inspect its diagnostics, and generate a JSON repair manifest:

```sh
bun examples/evolution/update.ts view UPDATE_ID
```

```json
{
  "calculate": {
    "source": "pub fn main() -> String { increment::main() + \"!\" }"
  },
  "report": {
    "source": "pub fn main() -> String { calculate::main() + \"?\" }"
  }
}
```

Names in the manifest identify existing definitions. Dependencies retain their existing aliases unless a repair supplies `deps`; the update rewrites pins to the new versions. Each source must typecheck against those updated dependencies.

```sh
bun examples/evolution/update.ts repair UPDATE_ID REVISION ./repairs.json
```

The server publishes the repaired graph together after every affected definition builds. Old hashes remain executable. Further compiler errors return another `needs_repair` revision. A stale revision requires reading the session again before submitting further work. A namespace conflict retains the session's edits. Inspect the conflict, then explicitly rebase against the current namespace:

```sh
bun examples/evolution/update.ts rebase UPDATE_ID REVISION
```

Rebase preserves disjoint changes by other agents and attempts the staged update again. If any explicitly edited definition changed concurrently, rebase returns `conflict` instead of overwriting that edit. Resolve that disagreement against the new code in a new update. Scripts should branch on the returned status; this example never retries automatically.

To discard the pending update:

```sh
bun examples/evolution/update.ts abort UPDATE_ID REVISION
```

The script prints JSON. Exit status `0` means the command succeeded, `2` means repairs remain, `3` means a structured conflict, and `1` means an HTTP or command error. Rejected stale requests may use status `1` with server diagnostics. HTTP requests abort after ten minutes by default; set `LOOM_UPDATE_TIMEOUT_MS` to change that bound. A client timeout does not prove the server cancelled the update.

For the executable eight-gate integration check, run against a disposable daemon:

```sh
LOOM_URL=http://127.0.0.1:8817 LOOM_TOKEN_FILE=/path/to/isolated/token bun scripts/e2e-evolution.ts
```

The script prints a recovery ID before submitting the update. If the connection
fails, use that ID with `view` to recover the session. Set
`LOOM_UPDATE_REQUEST_ID` to reuse a caller-chosen ID; repeating the same initial
request returns its stored outcome, while different inputs with that ID are
rejected. A pending session can resume through `update_repair` with an empty
`changes` object using the API.
