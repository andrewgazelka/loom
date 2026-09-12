# Nix host checks

The Nix host build (`nix/packages.nix`, `cargoUnit.buildWorkspace`) turns every quality gate off: `policy.clippy.enable`, `policy.tests.enable`, `cargoAudit`, `cargoMachete`, and `denyUnusedCrateDependencies` are all `false`, so `nix build .` produces binaries and asserts nothing about them. cargoUnit can render them instead: per-unit clippy derivations joined per package, per-test-target and per-case test derivations, and an offline lockfile-only `cargo-audit`, each cached per unit so a one-crate edit re-checks one crate. Its clippy runs a `clippy-driver` tied to index's pinned nightly, while this workspace pins a stable toolchain in `nix/toolchain.nix`, so turning clippy on means either passing this toolchain's own clippy or accepting a second compiler in the graph.

Trigger: CI exists (see [ci.md](ci.md)), so there is one place that would run these and one definition of the gate.

Done when: `nix flake check --builders ''` runs clippy and the workspace tests through cargoUnit and fails on a planted warning and a planted failing test, and the same run on an unchanged tree rebuilds nothing.
