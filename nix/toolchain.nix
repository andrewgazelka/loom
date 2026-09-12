# The two pinned Rust toolchains, assembled from official signed archives
# (`./update-rust-toolchain.py` refreshes the pins).
#
# `host` builds loomd and the loom CLI. `guest` is the compiler the daemon
# compiles guest definitions with, and the one tools/hash-rustc is built
# against: a rustc plugin only links against the exact compiler that produced
# its crates, and the daemon refuses a driver whose `-vV` differs from the
# guest compiler's.
{
  pkgs,
  lib,
}: let
  manifest = lib.importJSON ./rust-toolchain-manifest.json;
  toolchain = name: entry: let
    host =
      entry.archives.${pkgs.stdenv.hostPlatform.system}
      or (throw "loom.toolchain.${name}: unsupported host platform");
    archives = map pkgs.fetchurl (lib.attrValues host ++ lib.attrValues entry.archives.shared);
  in
    pkgs.stdenv.mkDerivation {
      pname = "loom-rust-toolchain";
      inherit (entry) version;
      strictDeps = true;
      dontUnpack = true;
      dontConfigure = true;
      dontBuild = true;
      dontStrip = true;
      nativeBuildInputs = [pkgs.xz] ++ lib.optional pkgs.stdenv.hostPlatform.isLinux pkgs.autoPatchelfHook;
      buildInputs = lib.optionals pkgs.stdenv.hostPlatform.isLinux [
        pkgs.stdenv.cc.cc.lib
        pkgs.zlib
        pkgs.zstd
        pkgs.openssl
        pkgs.curl
        pkgs.libxml2
      ];
      installPhase = ''
        runHook preInstall
        for archive in ${lib.escapeShellArgs archives}; do
          mkdir extraction
          tar -xJf "$archive" -C extraction --strip-components=1
          patchShebangs extraction/install.sh
          extraction/install.sh --prefix="$out" --disable-ldconfig
          rm -rf extraction
        done
        runHook postInstall
      '';
      passthru.channel = entry.channel;
      passthru.badTargetPlatforms = [];
      passthru.targetPlatforms = ["aarch64-darwin" "x86_64-linux"];
      passthru.updateScript = [
        (lib.getExe pkgs.python3)
        (toString ./update-rust-toolchain.py)
        "--gpg"
        (lib.getExe pkgs.gnupg)
        "--entry"
        name
      ];
      meta = {
        description = "Rust ${entry.channel} for Loom's ${name} builds, with the standard-library source";
        license = [lib.licenses.mit lib.licenses.asl20];
        mainProgram = "cargo";
        platforms = ["aarch64-darwin" "x86_64-linux"];
      };
    };
in
  lib.mapAttrs toolchain manifest.toolchains
