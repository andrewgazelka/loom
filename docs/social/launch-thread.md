# Loom launch thread

Draft for X. Attach [the graphic](../assets/loom-rust-repl-x.png) to post 1. Prepared for review; not posted.

## 1

I’m building Loom: a Rust REPL without function coloring.

Ordinary fn calls can suspend for I/O. You can run effects concurrently without turning the call chain into async functions.

A real example: finding the largest file in a 10,000-file tree. 🧵

## 2

The code in the image lists src and tests concurrently with loom::all(...).

Each operation returns typed Rust values. The host does the I/O and resumes the guest when the results are ready.

The function stays an ordinary fn.

## 3

The recursive scanner takes 10.46 ms with all, or 12.00 ms with fork/join.

Warm medians over 7 runs on an Apple Silicon Mac, including MCP transport and recording. Each round changes the winning file and checks the answer.

This scans metadata, not file contents.

## 4

The sequential native Rust baseline took 35.20 ms.

Loom uses parallel, batched filesystem operations. That’s why it wins this comparison. This is not a claim that Wasm beats equally optimized native code.

## 5

Effects are recorded so I can inspect what happened and replay a completed call. Code and results have content hashes.

Guests keep separate Wasm memories. Rust’s type system isn’t treated as a security boundary.

## 6

Still early: 10/12 scan gates pass. The remaining two require storage waits below 1 ms; they’re around 1.5 ms today.

The README has runnable Rust examples, the full scanner, and reproduction steps:
https://github.com/andrewgazelka/repl-maxx

