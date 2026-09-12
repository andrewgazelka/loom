# The content-hashing rustc driver the daemon runs for every guest build.
#
# `tools/hash-rustc` is a rustc plugin: it `extern crate rustc_driver`s, so it
# only compiles against the crates of the exact compiler that will run it. Its
# build script bakes that compiler's sysroot into the binary (`--print
# sysroot`) and links against the compiler's dylibs (`-Wl,-rpath,<sysroot>/
# lib`), so `bin/hash-rustc` runs from the store with no toolchain on PATH.
# That is the contract the daemon checks: `hash-rustc -vV` must equal
# `$RUSTC -vV`, and both come from ./toolchain.nix's `guest` entry.
#
# Cargo here is nixpkgs', not the pinned toolchain's. Nothing in this build
# needs a nightly cargo, and the official aarch64-apple-darwin cargo cannot run
# inside a Nix build sandbox on macOS: it is linked against /usr/lib/libcurl,
# whose LibreSSL reads /private/etc/ssl/openssl.cnf at startup, and the sandbox
# denies that path, which LibreSSL reports as "Auto configuration failed"
# before exiting 1. `rustc` is unaffected, and rustc is what decides identity.
{
  pkgs,
  lib,
  toolchain,
}: let
  rustPlatform = pkgs.makeRustPlatform {
    cargo = pkgs.cargo;
    rustc = toolchain;
  };
in
  rustPlatform.buildRustPackage {
    pname = "hash-rustc";
    version = "0.1.0";
    src = lib.fileset.toSource {
      root = ../tools/hash-rustc;
      fileset = lib.fileset.unions [
        ../tools/hash-rustc/Cargo.toml
        ../tools/hash-rustc/Cargo.lock
        ../tools/hash-rustc/build.rs
        ../tools/hash-rustc/src
      ];
    };
    cargoLock.lockFile = ../tools/hash-rustc/Cargo.lock;
    # buildRustPackage puts its `rustc` argument ahead of its `cargo` argument
    # on PATH, and this rustc is a whole toolchain: without this line the cargo
    # that runs is the pinned toolchain's own, the one that cannot start inside
    # the sandbox.
    nativeBuildInputs = [pkgs.cargo];
    # Linking the compiler's own dylibs pulls in what rustc links: zlib.
    buildInputs = [pkgs.zlib];
    # The driver is a rustc plugin, so it is nightly-only by construction; the
    # pinned toolchain is a nightly, and cargo needs telling that this is
    # deliberate rather than inheriting a stable cargo's refusal.
    env.RUSTC_BOOTSTRAP = "1";
    # The gates belong to this repo's own checks. `cargo auditable` would also
    # have to be a cargo this toolchain accepts.
    auditable = false;
    doCheck = false;
    meta = {
      description = "Loom's content-hashing rustc driver, built against the pinned guest compiler";
      license = lib.licenses.mit;
      mainProgram = "hash-rustc";
      platforms = ["aarch64-darwin" "x86_64-linux"];
    };
  }
