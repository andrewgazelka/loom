# Native backend beside wasm (design, 2026-09-30)

Status: spike only (`docs/spikes/native-abi`). Nothing in the runtime changes yet.

## Goal

One Rust cell compiles as wasm (today) or as native code, behind the same boundary: typed arguments in
as one DAG-CBOR array, one tagged DAG-CBOR frame out, effects through `perform`. The choice is a build
target, not a different program.

## Two uses

* **Script mode** (user, 2026-09-30: "use rust as a script that does not need sandboxing, fine, but native
  speeds"): trusted code the operator runs, `dlopen`ed in the host process, no isolation, no IPC. This
  is the fast path and the first thing to build.
* **Isolated mode**: untrusted code in a sandboxed worker process (below).

## Build target and optimization level are part of the artifact key

A definition's hash ignores codegen (it is the same for `#[inline]`, for the optimization level and for
the target), so the compiled artifact is keyed by the tuple `(definition hash, target, profile)`:

| target | profile default | why |
|---|---|---|
| `wasm` | `interactive` (opt-level 0) for `eval`, `standard` (opt-level 2) for `add` | today's rule: a REPL cell trades run speed for build latency |
| `native` | `release`: opt-level 3, line tables (`-C debuginfo=1`), thin-local LTO | native exists for speed; a slow-built, fast-running script is the point |

`eval` and `add` take `target` and `profile` (`interactive`, `standard`, `release`, or an explicit
`opt-level`), and both are recorded on the artifact and in the result-cache key, so one definition can
have a wasm and a native artifact side by side and a caller can ask for either. The existing `optimize`
flag on `eval` becomes `profile: "standard"`. Expect native `release` builds of real crates to cost
seconds, not the 60 to 70 ms of the wasm interactive path (the spike's tiny cdylib rebuilt in 1.1 s
including cargo start-up and the system linker); an `interactive` native profile (opt-level 0) exists for
edit loops.

## The catch: wasm is the sandbox

Loom admits `unsafe`, `extern` free functions and raw pointers in guests because the per-tenant wasm
memory is the boundary (README, "Memory isolation"). Native code has no such boundary: it can call any
syscall. So native guests are one of two things:

1. trusted, loaded in-process with `dlopen` (the host owns the code, like a plugin); or
2. untrusted, loaded in a worker process under a deny-by-default sandbox (`loom-process`: macOS
   `sandbox-exec` profile, Linux bubblewrap, both already used by `loom-action`), with arguments and
   results in a shared-memory ring and effects as requests over it.

Recommendation: build (2) as the default and allow (1) only for code the host operator compiled. The IPC
cost of a shared-memory round trip is tens of microseconds, still far below the wasm effect path; it
must be measured, it is not in the spike.

## Staging (each step leaves the tree working)

1. **ABI spike** (done): C-ABI entry, frame, dealloc, measured.
2. **SDK split**: `loom-guest-rs` compiles for the host target. The wasm imports (`perform`, `call`,
   `spawn`, `join`, `kernel`, `yield_value`) become a `HostApi` vtable handed to `loom_init(&HostApi)` on
   load; threads use `std::thread`. `__loom_export_entry!` emits the wasm or the C export by `cfg`.
3. **Build**: the direct path builds the SDK graph for the host target (a second graph key), links a
   cdylib (macOS `ld`, Linux `mold`/`lld`) through the served compiler; target is an `eval`/`add` argument
   (`target: "native" | "wasm"`) and part of the artifact identity. Definition hashes stay the same
   (they ignore codegen), so the artifact, not the definition, records the target.
4. **Worker**: a per-tenant sandboxed process loading the cdylib, with shm framing and the effect
   channel; the host keeps every policy check (allowed labels, depth, budgets) where it is.
5. **Zero-copy data**: `loom::Bytes` and typed views (`Packed<f32>`) in the SDK; scenes, meshes and
   kernels use them instead of tuples of floats. Benefits wasm too (one copy into linear memory).

## Zero-copy, exactly

* Borrowed on decode: text, byte strings, map keys. Everything numeric is parsed.
* Encoded size of numbers: DAG-CBOR floats are always 64-bit (9 bytes); f32 has no compact form.
* Therefore bulk numeric data must be byte strings. Alignment is not guaranteed, so typed access uses
  unaligned loads (`pod_read_unaligned`), or the encoder pads to 8 and the decoder casts when aligned.
* Results: the guest encodes once into an allocation the host reads in place (native: same address
  space or shared mapping; wasm: one copy out of linear memory, as today).

## Open questions

* Does the compilation/result cache key include the target? It must.
* Cross-machine determinism is out of scope (see rgb's ruling); native float behaviour differs by CPU.
* Windows is not addressed.
