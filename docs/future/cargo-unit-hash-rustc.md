# cargoUnit for the hash-rustc driver

`nix/hash-rustc.nix` builds `tools/hash-rustc` with `rustPlatform.buildRustPackage`
and nixpkgs' cargo, while the host binaries are built by `cargoUnit.buildWorkspace`
(`nix/packages.nix`). Two build paths for Rust in one repo is one more than the repo
should have: cargoUnit gives one derivation per rustc invocation, so a one-crate edit
recompiles one crate, and the driver does not get that.

What blocks it is a macOS-only failure in cargoUnit's first stage. Rendering the driver
through `cargoUnit.buildWorkspace` fails in `cargo-unit-graph.json`, with either pinned
toolchain, and the whole build log is:

```
Auto configuration failed
8563801984:error:02FFF001:system library:func(4095):Operation not permitted:...
  fopen('/private/etc/ssl/openssl.cnf', 'rb')
```

Apple's LibreSSL, reached through the `/usr/lib/libcurl` that official
static.rust-lang.org cargo binaries link, exits 1 when it cannot read that file, and the
Nix build sandbox denies `/private/etc`: it is in neither `sandbox-paths` nor
`allowed-impure-host-deps`, and a derivation can only name paths already allowed there.

What does not fit that story, and is the actual open question: the host workspace's own
`cargo-unit-graph.json` stage runs the same cargo binary, in the same sandbox, and
succeeds. So the trigger is something this crate's graph does that the workspace's does
not (a registry lookup that `--frozen --offline` does not suppress is the obvious guess,
unconfirmed). Running the failing `cargo build --unit-graph` command by hand outside the
sandbox succeeds, which is consistent with the sandbox being the denier and inconsistent
with the crate simply being misconfigured.

Trigger: someone reproduces the difference between the two graph stages, or Nix on
macOS stops denying `/private/etc/ssl/openssl.cnf`, or the pinned toolchains stop coming
from static.rust-lang.org.

Done when: `nix/hash-rustc.nix` renders through `cargoUnit.buildWorkspace` with the
guest toolchain, `nix build .#driver --builders ''` passes on aarch64-darwin and
x86_64-linux, `result/bin/hash-rustc -vV` still equals the guest `rustc -vV`, and
`nix/packages.nix` has one Rust build path.
