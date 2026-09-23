#!/usr/bin/env bash
# Rust -> Charon (LLBC) -> Aeneas (Lean) -> lake build -> gates. Exit 0 only if:
#   - the committed Lean translation equals a fresh one (no stale Generated/),
#   - every proof and every checker theorem builds (depth-5 sweep + positive control),
#   - the axiom reports match exactly (a `sorry` shows up as sorryAx and fails).
# Needs charon, aeneas and lake on PATH; versions pinned in README.md.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
rm -f "$here"/outbox/rust/*.llbc "$here"/harness/rust/*.llbc

# The translator and the Lean library it targets must be the same Aeneas rev.
lake_rev=$(sed -n 's/^rev = "\([0-9a-f]*\)"/\1/p' "$here/lean/lakefile.toml")
tool_rev=$(aeneas -version | awk '{print $2}')
case "$lake_rev" in
  "$tool_rev"*) ;;
  *) echo "aeneas $tool_rev does not match lakefile.toml rev $lake_rev" >&2; exit 1 ;;
esac

# Outbox: a standalone crate.
(cd "$here/outbox/rust" && charon cargo --preset=aeneas)
rm -rf "$here/lean/Outbox/Generated"
aeneas -backend lean -split-files -subdir Outbox/Generated -dest "$here/lean" "$here/outbox/rust/outbox_core.llbc"

# Harness: the Loom guest file itself; only `crate::core` is translated.
(cd "$here/harness/rust" && charon cargo --preset=aeneas --start-from crate::core)
rm -rf "$here/lean/Harness/Generated"
aeneas -backend lean -split-files -subdir Harness/Generated -dest "$here/lean" "$here/harness/rust/harness.llbc"

# Fail when the translation differs from what is committed: proofs must be about this code.
# `status` also sees new untracked files and staged-but-uncommitted copies.
stale=$(git -C "$here" status --porcelain -- lean/Outbox/Generated lean/Harness/Generated)
if [ -n "$stale" ]; then
  printf 'Generated Lean differs from the committed copy:\n%s\nReview and commit it, then rerun.\n' "$stale" >&2
  exit 1
fi

cd "$here/lean"
lake build
lake env lean AxiomsOutbox.lean
lake env lean AxiomsHarness.lean
echo "verify: all gates passed"
