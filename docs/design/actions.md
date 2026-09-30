# Hermetic actions: a process as a pure function (local first)

Status: built 2026-09-30 as the `loom-action` crate; not yet a verb, an SDK effect or a guest import.

## What it is

An `Action` is a tool (absolute path), arguments, the complete environment, input files by content hash (from the store), the
output files it must write, and a network flag. Its key is BLAKE3 over all of that plus the tool binary's own content hash and the
platform (`crates/loom-action/src/key.rs`). `Runner::run` answers a known key from the store and runs nothing; otherwise it
lays the inputs out in a fresh scratch directory (`Store::restore_to`: a clone on APFS, else a copy; never a hard link, so a tool cannot write through to the store), runs the tool inside
`loom-process`'s sandbox, and records the result (exit code, stdout, stderr and each declared output as blobs) only when the
tool exited 0 and wrote every declared output. A failure is returned, never cached. A recorded result with a missing blob is a miss.

Local first: identity is the tool's content hash plus a caller string, not a toolchain closure, and the format is Loom's own (no
Bazel REAPI). A result is valid on this machine's runtime libraries and is not portable.

## Confinement (what the tests prove, each with a control)

The tool sees the scratch directory (read and write), its declared read-only `runtime` paths, the system loader paths, and nothing
else: no environment is inherited, an undeclared read fails, a write outside the scratch directory fails, and the network is denied
unless the action asks (checked with a loopback listener: the same script connects with `network: true` and fails without).
Darwin uses the existing deny-by-default `sandbox-exec` profile (`loom-process/src/sandbox.rs`), Linux bubblewrap. `sandbox-exec` is the
command-line front end of the same libsandbox call; a direct `sandbox_init_with_parameters` in a forked child was not used because
running non-async-signal-safe code between `fork` and `exec` in a multithreaded host is a deadlock hazard.

## Findings

* rustc as the first workload (`tests/rustc.rs`, needs `LOOM_ACTION_SYSROOT`): 80 to 90 ms to compile a one-file rlib inside the sandbox,
  0.3 to 0.7 ms when answered from the store (load about 20). Rust binaries abort at start-up (`failed to allocate a guard page`) unless the
  profile allows `sysctl-read`; that is an opt-in `ProcessSandbox::sysctl_read` (off for actors, on for actions), so an action's tool can read
  this machine's hardware and kernel facts.
* rustc embeds the working directory in an rlib. Two identical compiles in different directories differ; with `-Zremap-cwd-prefix=/work
  --remap-path-prefix=@ROOT@=/work` they are byte-identical. The runner substitutes `@ROOT@` (the scratch directory) in arguments and
  environment values, and the key hashes the text with the token, so it does not depend on where the tool ran.
* The actor process tests in `loom-actor` use the first `sh` on `PATH`; a Nix-store `sh` has libraries the sandbox does not grant and those tests
  fail. `PATH=/bin:$PATH` runs them (8 of 8 pass).

## Not done

* A verb (`run_action`), an SDK effect (`loom::process::spawn`, with a fixed effect label like `kernel` and `yield`) and the result-cache
  admission of a guest call to an action.
* Cargo as a workload: it reads `~/.cargo`, env, absolute paths and mtimes; the unit to cache is one rustc invocation (what the direct build
  path already does), with proc macros and build scripts needing declared inputs or being uncacheable.
* Incremental rustc as declared mutable scratch beside a content-addressed output (possible because the cache is per machine).
* Output directories (only regular files are declared and kept), a tree hash as an input (today a flat path to hash map plus
  `ingest_directory`), and killing a tool's descendants on timeout (only the direct child is killed).
* Without clone support (Linux today) a spilled input is copied into the scratch directory, which costs a copy per 1 MiB-plus input; a
  reflink attempt (FICLONE) would remove that. Restores were hard links until a review found a same-uid tool could write through the link into the store.
