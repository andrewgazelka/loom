{
  pkgs,
  lib,
}: let
  manifest = lib.importJSON ./rust-toolchain-manifest.json;
  hostArchive =
    {
      aarch64-darwin = "host-darwin";
      x86_64-linux = "host-linux";
    }.${
      pkgs.stdenv.hostPlatform.system
    } or (throw "loom.toolchain: unsupported host platform");
  archives = map (name: pkgs.fetchurl manifest.archives.${name}) [hostArchive "wasip1" "wasip2"];
in
  pkgs.stdenv.mkDerivation {
    pname = "loom-rust-toolchain";
    inherit (manifest) version;
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
    passthru.updateScript = [
      (lib.getExe pkgs.python3)
      (toString ./update-rust-toolchain.py)
      "--gpg"
      (lib.getExe pkgs.gnupg)
    ];
    meta = {
      description = "Matched official Rust compiler, host libraries and both WASI targets for Loom";
      license = [lib.licenses.mit lib.licenses.asl20];
      mainProgram = "cargo";
      platforms = ["aarch64-darwin" "x86_64-linux"];
    };
  }
