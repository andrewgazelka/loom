{
  pkgs,
  lib,
  src,
}: let
  catalogs = lib.importJSON ./javascript-locks.json;
  platform = pkgs.stdenv.hostPlatform;
  os =
    if platform.isDarwin
    then "darwin"
    else "linux";
  cpu =
    if platform.isAarch64
    then "arm64"
    else "x64";
  matches = value: current: let
    values =
      if builtins.isList value
      then value
      else [value];
  in
    values == [] || builtins.elem current values;
  packageSource = directory:
    lib.fileset.toSource {
      root = src + "/${directory}";
      fileset =
        lib.fileset.difference
        (lib.fileset.fileFilter (
          file:
            file.hasExt "ts"
            || file.hasExt "js"
            || file.hasExt "svelte"
            || file.hasExt "css"
            || file.hasExt "html"
            || file.name == "package.json"
            || file.name == "bun.lock"
            || file.name == "tsconfig.json"
        ) (src + "/${directory}"))
        (
          lib.fileset.unions (
            map (name: src + "/${directory}/${name}") (
              builtins.filter (name: builtins.pathExists (src + "/${directory}/${name}")) [
                "node_modules"
                "build"
                ".svelte-kit"
              ]
            )
          )
        );
    };
  dependencies = directory: let
    catalog = catalogs."${directory}";
    selected =
      builtins.filter (
        package:
          matches package.os os
          && matches package.cpu cpu
          # These packages encode libc in their name, not their Bun lock metadata.
          && !(platform.isLinux && lib.hasSuffix "-musl" package.name)
      )
      catalog.packages;
    unpack = package: let
      archive = pkgs.fetchurl {inherit (package) url hash;};
      destination = lib.escapeShellArg package.path;
      inherit (package) bins binDirectory;
    in ''
      mkdir -p ${destination}
      tar -xzf ${archive} -C ${destination} --strip-components=1 --no-same-owner
      ${lib.concatStringsSep "\n" (
        lib.mapAttrsToList (name: target: ''
          mkdir -p ${lib.escapeShellArg binDirectory}
          ln -s ${lib.escapeShellArg ("../" + package.installedName + "/" + target)} ${
            lib.escapeShellArg (binDirectory + "/" + name)
          }
        '')
        bins
      )}
    '';
  in
    assert lib.assertMsg (
      builtins.hashString "sha256" (builtins.readFile (src + "/${directory}/bun.lock"))
      == catalog.lockHash
    ) "loom: ${directory}/bun.lock changed; run bun nix/update-javascript-locks.ts";
      pkgs.stdenvNoCC.mkDerivation {
        pname = "${directory}-dependencies";
        version = "0.1.0";
        strictDeps = true;
        dontUnpack = true;
        nativeBuildInputs = [pkgs.nodejs] ++ lib.optional platform.isLinux pkgs.autoPatchelfHook;
        buildInputs = lib.optional platform.isLinux pkgs.stdenv.cc.cc.lib;
        installPhase = ''
          runHook preInstall
          mkdir -p "$out"
          cd "$out"
          ${lib.concatMapStringsSep "\n" unpack selected}
          patchShebangs --build "$out/node_modules"
          runHook postInstall
        '';
      };
  project = directory:
    pkgs.stdenvNoCC.mkDerivation {
      pname = directory;
      version = "0.1.0";
      src = packageSource directory;
      strictDeps = true;
      dontBuild = true;
      installPhase = ''
        runHook preInstall
        mkdir -p "$out"
        cp -R . "$out/"
        ln -s ${dependencies directory}/node_modules "$out/node_modules"
        runHook postInstall
      '';
    };
in {
  checker = project "checker";
  guest = project "guest-ts";
  ui = pkgs.stdenvNoCC.mkDerivation {
    pname = "ui";
    version = "0.1.0";
    src = packageSource "ui";
    strictDeps = true;
    nativeBuildInputs = [
      pkgs.bun
      pkgs.nodejs
    ];
    configurePhase = ''
      runHook preConfigure
      export HOME="$TMPDIR/ui-home"
      mkdir -p "$HOME"
      ln -s ${dependencies "ui"}/node_modules node_modules
      runHook postConfigure
    '';
    buildPhase = ''
      runHook preBuild
      bun run check
      bun run build
      runHook postBuild
    '';
    installPhase = ''
      runHook preInstall
      cp -R build "$out"
      test -s "$out/index.html"
      runHook postInstall
    '';
  };
}
