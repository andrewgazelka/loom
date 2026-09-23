#!/usr/bin/env bash
# Rust -> Charon (LLBC) -> Aeneas (Lean) -> lake build (proofs + checkers) -> axiom report.
# Needs charon, aeneas and lake on PATH; versions pinned in README.md.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"

# Outbox: a standalone crate.
(cd "$here/outbox/rust" && charon cargo --preset=aeneas)
rm -rf "$here/lean/Outbox/Generated"
aeneas -backend lean -split-files -subdir Outbox/Generated -dest "$here/lean" "$here/outbox/rust/outbox_core.llbc"

# Harness: the Loom guest file itself; only `crate::core` is translated.
(cd "$here/harness/rust" && charon cargo --preset=aeneas --start-from crate::core)
rm -rf "$here/lean/Harness/Generated"
aeneas -backend lean -split-files -subdir Harness/Generated -dest "$here/lean" "$here/harness/rust/harness.llbc"

cd "$here/lean"
lake build
lake env lean AxiomsOutbox.lean
lake env lean AxiomsHarness.lean
