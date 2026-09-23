#!/usr/bin/env bash
# Rust -> Charon (LLBC) -> Aeneas (Lean) -> lake build -> gates. Exit 0 only if:
#   - the committed Lean translation equals a fresh one (no stale Generated/),
#   - every proof and every checker theorem builds (depth-5 sweep + positive control),
#   - the axiom reports match exactly (a `sorry` shows up as sorryAx and fails).
# Needs charon, aeneas and lake on PATH; versions pinned in README.md.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
rm -f "$here"/outbox/rust/*.llbc "$here"/harness/rust/*.llbc

# Outbox: a standalone crate.
(cd "$here/outbox/rust" && charon cargo --preset=aeneas)
rm -rf "$here/lean/Outbox/Generated"
aeneas -backend lean -split-files -subdir Outbox/Generated -dest "$here/lean" "$here/outbox/rust/outbox_core.llbc"

# Harness: the Loom guest file itself; only `crate::core` is translated.
(cd "$here/harness/rust" && charon cargo --preset=aeneas --start-from crate::core)
rm -rf "$here/lean/Harness/Generated"
aeneas -backend lean -split-files -subdir Harness/Generated -dest "$here/lean" "$here/harness/rust/harness.llbc"

# Fail when the translation differs from what is committed: proofs must be about this code.
if ! git -C "$here" diff --exit-code --stat -- lean/Outbox/Generated lean/Harness/Generated; then
  echo "Generated Lean differs from the committed copy: review and commit it, then rerun." >&2
  exit 1
fi

cd "$here/lean"
lake build
lake env lean AxiomsOutbox.lean
lake env lean AxiomsHarness.lean
echo "verify: all gates passed"
