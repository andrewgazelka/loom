{
  pkgs,
  lib,
}: let
  src = lib.fileset.toSource {
    root = ../.;
    fileset =
      lib.fileset.difference
      (lib.fileset.unions [../Cargo.toml ../Cargo.lock ../crates ../examples ../loom-wit ../loom-rustc ../loom-checker ../loom-guest-ts ../loom-ui])
      (lib.fileset.unions (map lib.fileset.maybeMissing [../loom-checker/node_modules ../loom-guest-ts/node_modules ../loom-ui/node_modules ../loom-ui/.svelte-kit ../loom-ui/build]));
  };
  rustSource = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [../Cargo.toml ../Cargo.lock ../crates ../examples ../loom-wit ../loom-rustc/vendor-config.toml];
  };
  toolchain = import ./toolchain.nix {inherit pkgs lib;};
  javascript = import ./javascript.nix {
    inherit pkgs lib;
    src = ../.;
  };
  rustPlatform = pkgs.makeRustPlatform {
    cargo = toolchain;
    rustc = toolchain;
  };
  host = rustPlatform.buildRustPackage {
    pname = "loom-host";
    version = "0.1.0";
    src = rustSource;
    cargoLock.lockFile = ../Cargo.lock;
    cargoBuildFlags = ["-p" "loomd" "-p" "loom-cli"];
    doCheck = false;
    strictDeps = true;
    nativeBuildInputs = [pkgs.pkg-config];
    buildInputs = [pkgs.openssl];
    meta = {
      description = "Loom actor host";
      license = lib.licenses.mit;
      mainProgram = "loomd";
    };
  };
  sources = pkgs.runCommand "loom-runtime-sources" {} ''
    mkdir -p $out
    cp -R ${src}/Cargo.toml ${src}/Cargo.lock ${src}/crates ${src}/examples ${src}/loom-wit ${src}/loom-rustc $out/
    cp -R ${javascript.checker} $out/loom-checker
    cp -R ${javascript.guest} $out/loom-guest-ts
    mkdir -p $out/loom-ui
    ln -s ${javascript.ui} $out/loom-ui/build
  '';
  runtime = [toolchain pkgs.bun pkgs.cargo-component pkgs.binaryen pkgs.stdenv.cc pkgs.pkg-config pkgs.coreutils pkgs.findutils pkgs.gnused pkgs.gnugrep pkgs.gnutar pkgs.gzip pkgs.cacert] ++ lib.optional pkgs.stdenv.hostPlatform.isLinux pkgs.bubblewrap;
  launcher = pkgs.replaceVars ./loom.sh {
    bash = lib.getExe pkgs.bash;
    runtimePath = lib.makeBinPath runtime;
    inherit sources;
    daemon = lib.getExe host;
    rustc = lib.getExe' toolchain "rustc";
    certificates = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
  };
  package = pkgs.stdenvNoCC.mkDerivation {
    pname = "loom";
    version = "0.1.0";
    strictDeps = true;
    dontUnpack = true;
    nativeBuildInputs = [pkgs.shellcheck];
    installPhase = ''
      runHook preInstall
      mkdir -p $out/bin $out/libexec
      cp ${launcher} $out/bin/loom
      ln -s ${lib.getExe pkgs.bun} $out/libexec/bun
      chmod +x $out/bin/loom
      shellcheck $out/bin/loom
      runHook postInstall
    '';
    passthru = {inherit host toolchain sources;};
    meta = {
      description = "Loom with TypeScript and Rust guest toolchains";
      license = lib.licenses.mit;
      mainProgram = "loom";
    };
  };
in {
  default = package;
  inherit host toolchain sources;
}
