# The Deno CLI supplies admission-time npm/jsr/HTTPS resolution. Guest isolates
# receive only its immutable bundle and never run this tool or fetch modules.
{
  pkgs,
  lib,
}: let
  manifest = lib.importJSON ./deno-manifest.json;
  platform = manifest.platforms.${pkgs.stdenv.hostPlatform.system}
    or (throw "loom.deno: unsupported host platform");
  esbuild = pkgs.stdenvNoCC.mkDerivation {
    pname = "loom-esbuild";
    version = manifest.esbuild_version;
    src = pkgs.fetchurl platform.esbuild;
    strictDeps = true;
    dontStrip = true;
    dontConfigure = true;
    dontBuild = true;
    installPhase = ''
      runHook preInstall
      install -Dm755 bin/esbuild $out/bin/esbuild
      runHook postInstall
    '';
    doInstallCheck = true;
    installCheckPhase = ''
      runHook preInstallCheck
      test "$($out/bin/esbuild --version)" = '${manifest.esbuild_version}'
      runHook postInstallCheck
    '';
    meta = {
      description = "Exact esbuild helper used by Loom's Deno admission bundler";
      license = lib.licenses.mit;
      mainProgram = "esbuild";
      platforms = lib.attrNames manifest.platforms;
    };
  };
  deno = pkgs.stdenvNoCC.mkDerivation {
    pname = "loom-deno-cli";
    version = manifest.version;
    src = pkgs.fetchurl platform.deno;
    strictDeps = true;
    dontUnpack = true;
    dontBuild = true;
    dontStrip = true;
    nativeBuildInputs = [pkgs.unzip] ++ lib.optional pkgs.stdenv.hostPlatform.isLinux pkgs.autoPatchelfHook;
    buildInputs = lib.optional pkgs.stdenv.hostPlatform.isLinux pkgs.stdenv.cc.cc.lib;
    installPhase = ''
      runHook preInstall
      mkdir -p $out/bin
      unzip -j $src deno -d $out/bin
      chmod +x $out/bin/deno
      runHook postInstall
    '';
    doInstallCheck = true;
    installCheckPhase = ''
      runHook preInstallCheck
      export DENO_NO_UPDATE_CHECK=1
      $out/bin/deno --version | head -n1 | grep -Fx 'deno ${manifest.version} (stable, release, ${pkgs.stdenv.hostPlatform.rust.rustcTarget})'
      runHook postInstallCheck
    '';
    meta = {
      description = "Pinned official Deno CLI for Loom JavaScript admission";
      license = lib.licenses.mit;
      mainProgram = "deno";
      platforms = lib.attrNames manifest.platforms;
    };
  };
  closure = pkgs.closureInfo {rootPaths = [deno esbuild pkgs.cacert];};
  profile = pkgs.runCommand "loom-deno.sb" {} ''
    cat > $out <<'PROFILE'
    (version 1)
    (deny default)
    (allow process* sysctl-read mach-lookup network*)
    (allow file-read-metadata)
    ; macOS 27 dyld CacheFinder reads the root directory before loading libraries.
    (allow file-read* (literal "/"))
    (allow file-read* file-write* (subpath (param "IMPORT_ROOT")))
    (allow file-read* (subpath "/System/Library") (subpath "/usr/lib")
      (literal "/dev/null") (literal "/dev/random") (literal "/dev/urandom")
      (literal "/private/etc/resolv.conf")
      (literal "/private/etc/hosts") (literal "/private/etc/localtime"))
    (allow file-write* (literal "/dev/null"))
    PROFILE
    while IFS= read -r dependency; do
      printf '(allow file-read* (subpath "%s"))\n' "$dependency" >> $out
    done < ${closure}/store-paths
  '';
  isolation = if pkgs.stdenv.hostPlatform.isLinux then lib.replaceStrings
    ["@closure@" "@bwrap@" "@deno@" "@certificates@"]
    ["${closure}/store-paths" (lib.getExe pkgs.bubblewrap) (lib.getExe deno) "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"]
    (builtins.readFile ./deno-linux.sh)
    else lib.replaceStrings ["@profile@" "@deno@"] [(toString profile) (lib.getExe deno)] (builtins.readFile ./deno-darwin.sh);
  launcher = pkgs.replaceVars ./deno.sh {
    bash = lib.getExe pkgs.bash;
    realpath = lib.getExe' pkgs.coreutils "realpath";
    inherit isolation;
    deno = lib.getExe deno;
    esbuild = lib.getExe esbuild;
    helperCache = manifest.helper_cache;
    helperName = platform.helper;
    mkdir = lib.getExe' pkgs.coreutils "mkdir";
    mktemp = lib.getExe' pkgs.coreutils "mktemp";
    ln = lib.getExe' pkgs.coreutils "ln";
    mv = lib.getExe' pkgs.coreutils "mv";
    rm = lib.getExe' pkgs.coreutils "rm";
    rmdir = lib.getExe' pkgs.coreutils "rmdir";
  };
in
  pkgs.stdenvNoCC.mkDerivation {
    pname = "loom-deno";
    version = manifest.version;
    strictDeps = true;
    dontUnpack = true;
    nativeBuildInputs = [pkgs.shellcheck];
    installPhase = ''
      runHook preInstall
      mkdir -p $out/bin
      cp ${launcher} $out/bin/deno
      chmod +x $out/bin/deno
      shellcheck $out/bin/deno
      runHook postInstall
    '';
    doInstallCheck = true;
    installCheckPhase = ''
      runHook preInstallCheck
      # On Darwin the launcher confines Deno with sandbox-exec, and macOS refuses a
      # sandbox nested inside the Nix build sandbox ("Operation not permitted"). Run the
      # check wherever sandbox-exec works; say so where it cannot.
      # The probe execs sandbox-exec: inside the Darwin build sandbox even `test -x` on it
      # reports false while exec fails with EPERM, so only an exec attempt is reliable.
      if ${lib.boolToString pkgs.stdenv.hostPlatform.isDarwin} && ! /usr/bin/sandbox-exec -p '(version 1)(allow default)' /usr/bin/true 2>/dev/null; then
        echo "loom-deno: skipping install check, nested sandbox-exec is not permitted in this build sandbox"
      else
        export LOOM_IMPORT_ROOT=$TMPDIR
        export DENO_DIR=$TMPDIR/deno-cache
        mkdir -p "$DENO_DIR"
        mkdir -p fixture
        printf '%s\n' 'export const answer = 42;' > fixture/main.ts
        $out/bin/deno bundle --no-config --no-lock --no-npm --platform=browser --format=iife --output=fixture/bundle.js fixture/main.ts
        test -s fixture/bundle.js
        test "$(readlink "$DENO_DIR/dl/${manifest.helper_cache}/${platform.helper}")" = '${lib.getExe esbuild}'
      fi
      runHook postInstallCheck
    '';
    passthru = {
      inherit deno esbuild;
      updateScript = [(lib.getExe pkgs.python3) (toString ./update-deno.py) manifest.version];
    };
    meta = {
      description = "Loom's pinned Deno bundler with its pinned native helper";
      license = lib.licenses.mit;
      mainProgram = "deno";
      platforms = lib.attrNames manifest.platforms;
    };
  }
