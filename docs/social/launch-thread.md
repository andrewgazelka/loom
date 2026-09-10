# Loom launch thread

Attach [the code-rendered dark graphic](../assets/loom-grep-dark.png) to post 1. [Graphic source](async-example.rs). Prepared for review; not posted.

## 1

I'm building Loom: a Rust REPL without function coloring.

Fork a job. Start two one-second timers. Join. Their waits overlap, using ordinary Rust functions.

Ordinary fn. No async or .await. 🧵

## 2

The child starts one timer; the parent starts another. Loom suspends each job while its timer runs.

scope.fork(...) starts a job; job.join() returns typed results. The scope waits for forgotten handles too.

No argument structs or value wrappers.

## 3

While a job waits for a file or timer, the runtime can run other jobs.

Effects are recorded for replay. Definitions and results have content hashes.

Use the same runtime from the browser REPL or a coding agent over MCP.

## 4

The repo also has a recursive text-search example. It searches UTF-8 files for a literal string, skips symlinks and returns sorted paths.

It's small: no regex, ignore-file handling or streaming.

The graphic uses two timers to show fork/join.

## 5

Benchmark details are in the README: the workload, warm-run timings and native comparison.

The text-search example has correctness checks. Metadata-scan timings measure a different workload.

## 6

Shared jobs are one trust domain. Separate executions have separate memories.

User code must pass safe-code admission; pinned SDK/std internals remain trusted. This is not a formal proof.

Code and setup:
https://github.com/andrewgazelka/repl-maxx
