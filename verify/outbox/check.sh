#!/usr/bin/env bash
# Rust -> Charon (LLBC) -> Aeneas (Lean) -> lake build (proofs).
# Needs charon, aeneas, and lake on PATH (see README.md for the pinned versions).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
cd "$here/rust"
charon cargo --preset=aeneas
rm -rf "$here/lean/Outbox/Generated"
aeneas -backend lean -split-files -subdir Outbox/Generated -dest "$here/lean" outbox_core.llbc
cd "$here/lean"
lake build Outbox
lake env lean Axioms.lean
