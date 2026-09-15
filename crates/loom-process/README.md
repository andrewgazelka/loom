# Process sessions

`Supervisor` owns process groups, stdin, durable output, and terminal state.
`ProcessSession::take_input()` lets a caller write stdin while draining output.
Session cancellation kills the process group; the supervisor reaps it and records
the final state. Partial stdin writes are never replayed into another process.

## Actor process presets

Actor-driven processes must use `start_sandboxed_session(spec, sandbox)`.
`ProcessSpec.root` is their writable workspace. Place it outside the daemon's
state directories: it must not contain tenant signing keys, actor databases, or
another tenant's files. `cwd` must remain inside that workspace.

`ProcessSandbox.readonly` lists the executable and its runtime dependencies.
Use exact runtime closure paths, such as the executable's Nix closure, rather
than an entire home directory or `/nix/store`. These paths become visible to the
process, so include only files that the tenant may read. Network access defaults
to disabled; `network: true` explicitly grants the host network namespace.

Linux uses bubblewrap to expose that writable workspace, the declared read-only
runtime, and private process/device/temporary filesystems. The Nix package pins
bubblewrap through `LOOM_BWRAP`; source builds can supply that host setting or
use the supervisor's PATH. A broken explicit path is an error.

Darwin uses a deny-by-default `sandbox-exec` profile with the declared paths and
minimal system loader reads. It permits process execution and forking, while
process inspection, debugging, and sysctl access remain denied. Missing
confinement support is an error; execution never retries without confinement.
Host-only `start` and `start_session` remain available for already-authorized
host operations and container-runtime launchers. Checking a working directory
alone does not confine filesystem access.

A public preset name resolves through the tenant's registry. The actor records
the fixed driver identity before its committed outbox starts the process. Host
restart or ownership transfer records interruption; it never silently reruns the
command. Container drivers reuse session I/O while their container runtime owns
filesystem isolation.
