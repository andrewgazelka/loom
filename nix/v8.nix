# Pointer-compressed official V8 release inputs. Upstream does not publish the
# `_ptrcomp_sandbox_` variant: these isolates do not enable V8's additional
# memory-corruption cage. Host API isolation is implemented by loom-v8.
{
  pkgs,
  lib,
}: let
  manifest = lib.importJSON ./v8-manifest.json;
  dependency = (lib.importTOML ../crates/loom-v8/Cargo.toml).dependencies.v8;
  platform = manifest.platforms.${pkgs.stdenv.hostPlatform.system}
    or (throw "loom.v8: unsupported host platform");
  checked =
    if
      dependency.version
      == "=${manifest.version}"
      && dependency.features == ["v8_enable_pointer_compression"]
    then platform
    else throw "loom.v8: refresh native inputs with python3 nix/update-v8.py";
  updateScript = [(lib.getExe pkgs.python3) (toString ./update-v8.py)];
in {
  archive = (pkgs.fetchurl checked.archive).overrideAttrs (old: {
    passthru = (old.passthru or {}) // {inherit updateScript;};
  });
  bindings = pkgs.fetchurl checked.bindings;
}
