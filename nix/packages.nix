{
  pkgs,
  lib,
  cargoUnit,
}: let
  src = lib.fileset.toSource {
    root = ../.;
    fileset =
      lib.fileset.difference
      (lib.fileset.unions [../Cargo.toml ../Cargo.lock ../crates ../examples ../rustc ../ui])
      (lib.fileset.unions (map lib.fileset.maybeMissing [../ui/node_modules ../ui/.svelte-kit ../ui/build]));
  };
  rustSource = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [../Cargo.toml ../Cargo.lock ../crates ../examples ../rustc/vendor-config.toml];
  };
  toolchains = import ./toolchain.nix {inherit pkgs lib;};
  # `buildToolchain` compiles this repo's two binaries and appears in no
  # runtime closure; `toolchain` is what the daemon compiles guests with, and
  # what the hash-rustc driver is built against.
  buildToolchain = toolchains.host;
  toolchain = toolchains.guest;
  javascript = import ./javascript.nix {
    inherit pkgs lib;
    src = ../.;
  };
  hostTarget =
    {
      aarch64-darwin = "aarch64-apple-darwin";
      x86_64-linux = "x86_64-unknown-linux-gnu";
    }.${
      pkgs.stdenv.hostPlatform.system
    } or (throw "loom.host: unsupported host platform");
  # The daemon runs this prebuilt driver for every guest build; it is built
  # from, and pinned to, the same nightly it compiles guests with.
  driver = import ./hash-rustc.nix {inherit pkgs lib toolchain;};
  v8 = import ./v8.nix {inherit pkgs lib;};
  deno = import ./deno.nix {inherit pkgs lib;};
  # One rustc invocation per Cargo unit, so a change in one crate rebuilds that
  # crate and its dependents, not the workspace. This is the host build: what it
  # compiles with is invisible to guests, which get ./toolchain.nix's `guest`
  # entry and the driver built from it. The gates (clippy, tests, audit) belong
  # to this repo's own checks, not to packaging: this workspace exists to
  # produce two binaries.
  workspace = cargoUnit.buildWorkspace {
    pname = "loom-host";
    src = rustSource;
    workspaceRoot = rustSource;
    cargoLock = ../Cargo.lock;
    rustToolchain = buildToolchain;
    cargoTargetNames = ["host"];
    cargoTargets = [["-p" "loomd" "-p" "loom-cli" "--target" hostTarget]];
    profile = "release";
    nativeBuildInputs = [pkgs.pkg-config];
    # V8's build script otherwise downloads its native archive and bindings.
    # Scope these fixed-output inputs to V8 so updating them does not rebuild
    # unrelated Cargo units.
    packageBuildEnv = {
      v8 = {
        RUSTY_V8_ARCHIVE = v8.archive;
        RUSTY_V8_SRC_BINDING_PATH = v8.bindings;
      };
      loom-imports.LOOM_DENO = lib.getExe deno;
    } // lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
      loom-process.LOOM_BWRAP = lib.getExe pkgs.bubblewrap;
    };
    policy = {
      # This workspace's rustc is the pinned STABLE compiler in ./toolchain.nix.
      # cargoUnit otherwise passes `-Zembed-metadata=no`, which a stable rustc
      # refuses outright; its own guard reads a channel tag that only toolchains
      # built by index carry, so it cannot see this one.
      compiler.embedMetadata = true;
      denyUnusedCrateDependencies = false;
      clippy.enable = false;
      tests.enable = false;
      cargoAudit.enable = false;
      cargoMachete.enable = false;
    };
  };
  # `lib.getExe` needs a mainProgram; a unit derivation is named for its crate.
  withMainProgram = name: derivation:
    derivation.overrideAttrs (old: {
      meta = (old.meta or {}) // {mainProgram = name;};
    });
  daemon = withMainProgram "loomd" workspace.targetSets.host.binaries.loomd;
  cli = withMainProgram "loom" workspace.targetSets.host.binaries.loom;
  host = pkgs.symlinkJoin {
    name = "loom-host";
    paths = [daemon cli];
    meta = {
      description = "Loom actor host: the loomd daemon and the loom CLI";
      license = lib.licenses.mit;
      mainProgram = "loomd";
    };
  };
  sources = pkgs.runCommand "loom-runtime-sources" {} ''
    mkdir -p $out
    cp -R ${src}/Cargo.toml ${src}/Cargo.lock ${src}/crates ${src}/examples ${src}/rustc $out/
    mkdir -p $out/ui
    ln -s ${javascript.ui} $out/ui/build
  '';
  runtime = [toolchain driver pkgs.bun pkgs.binaryen pkgs.stdenv.cc pkgs.stdenv.cc.bintools pkgs.pkg-config pkgs.coreutils pkgs.findutils pkgs.gnused pkgs.gnugrep pkgs.gnutar pkgs.gzip pkgs.cacert] ++ lib.optionals pkgs.stdenv.hostPlatform.isLinux [pkgs.bubblewrap pkgs.util-linux];
  launcher = pkgs.replaceVars ./loom.sh {
    bash = lib.getExe pkgs.bash;
    runtimePath = lib.makeBinPath runtime;
    inherit sources;
    daemon = lib.getExe daemon;
    rustc = lib.getExe' toolchain "rustc";
    hashRustc = lib.getExe driver;
    certificates = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
  };
  # `loomd` is the daemon with its state directory, token, and guest toolchain
  # already arranged; `loom` is the CLI that talks to it. Two names, one for each.
  package = pkgs.stdenvNoCC.mkDerivation {
    pname = "loom";
    version = "0.1.0";
    strictDeps = true;
    dontUnpack = true;
    nativeBuildInputs = [pkgs.shellcheck];
    installPhase = ''
      runHook preInstall
      mkdir -p $out/bin $out/libexec
      cp ${launcher} $out/bin/loomd
      ln -s ${lib.getExe cli} $out/bin/loom
      ln -s ${lib.getExe pkgs.bun} $out/libexec/bun
      chmod +x $out/bin/loomd
      shellcheck $out/bin/loomd
      runHook postInstall
    '';
    passthru = {inherit host daemon cli toolchain buildToolchain driver sources workspace deno;};
    meta = {
      description = "Loom with the Rust guest toolchain";
      license = lib.licenses.mit;
      mainProgram = "loomd";
    };
  };
  replLauncher = pkgs.replaceVars ./repl.sh {
    bash = lib.getExe pkgs.bash;
    loomd = lib.getExe' package "loomd";
    cli = lib.getExe cli;
  };
  repl = pkgs.stdenvNoCC.mkDerivation {
    pname = "loom-repl";
    version = "0.1.0";
    strictDeps = true;
    dontUnpack = true;
    nativeBuildInputs = [pkgs.shellcheck];
    installPhase = ''
      runHook preInstall
      mkdir -p $out/bin
      cp ${replLauncher} $out/bin/loom-repl
      chmod +x $out/bin/loom-repl
      shellcheck $out/bin/loom-repl
      runHook postInstall
    '';
    meta = {
      description = "Loom REPL: loomd plus the browser UI, opened automatically";
      license = lib.licenses.mit;
      mainProgram = "loom-repl";
    };
  };
in {
  default = package;
  inherit host daemon cli toolchain buildToolchain driver sources repl deno;
}
