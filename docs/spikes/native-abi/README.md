# Native guests beside wasm: a measured spike

Question (user, 2026-09-30): compile the same cell as native code or as wasm, keep the DAG-CBOR
boundary, and make encode/decode zero-copy for the most part.

`guest/` is a cdylib with the entry shape `loom-build` generates for wasm, behind a C ABI:
`loom_call_<entry>(args_ptr, args_len, *mut Frame) -> i32`, arguments one DAG-CBOR array, result one
tagged frame (byte 0, then DAG-CBOR), freed with `loom_dealloc`. `host/` dlopens it and times calls.
No Loom crates are involved, so effects, handlers and isolation are not measured.

Machine: this Mac, load average 13, `cargo build --release --offline` from the repo's nightly
(2026-08-24), medians of 20 to 200 calls, one run each (not repeated on a quiet machine).

| call | native | wasm today (Loom, same machine class) |
|---|---|---|
| `primes(200_000)` | 100 us (load 13) | 7,950 us at wasm `interactive` (opt-level 0), **506 us at wasm optimized** (`eval optimize: true`, opt-level 2; load 35, median of 15) |
| 424-command scene out, no effects | 13 us | 7,800 us with one effect dispatch per command |
| sum 1M f32 passed as a CBOR array of floats (9 MB) | 3,600 us | not measured |
| the same 1M f32 as one CBOR byte string (4 MB) | 576 us, most of it the sum loop | not measured |
| load a module | 1.1 ms `dlopen` | 10 to 100 ms compile per new module, 0 when cached |
| rebuild after an edit | 1.1 s (`cargo build`, includes cargo start-up and ld) | 60 to 70 ms (served compiler, pre-armed lld) |

What it says:

* DAG-CBOR is not zero-copy as a whole. Strings and byte strings borrow from the input slice (the
  spike's `Packed<'de>` does, via `visit_borrowed_bytes`). Every number is decoded, and a float is a
  one-byte header plus 8 bytes, so an `f32` array costs 9 bytes and a parse per element.
* So "mostly zero-copy" means: bulk numeric data travels as CBOR byte strings of packed little-endian
  values. The decoder hands out a borrowed `&[u8]`; reading it is an unaligned load (a byte string's
  offset is not aligned), which is free on arm64 and x86-64. That gave 6x on this input and shrank it
  from 9 MB to 4 MB. The scene case shows the cost of the current shape (tuples of floats): 19 KB out
  for 424 commands.
* Correction (same day): the first comparison put native against wasm built at opt-level 0, which is what
  `eval` uses by default. Against optimized wasm the sieve is about 5x slower than native (506 us against
  100 us, measured at different machine loads, so the ratio is rough), not 80x. Most of the gap in the
  first table was the build profile, not wasm. Only the effect-dispatch rows are a real structural cost of
  the wasm path (about 18 us per effect, which native in-process calls would not pay).
