{
  description = "Loom actor runtime and complete guest toolchains";
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    # `index` pins a rust-overlay old enough to read `stdenv.isLinux`, whose
    # deprecation warning aborts evaluation wherever `abort-on-warn` is set.
    # Current rust-overlay reads `stdenv.hostPlatform.isLinux` instead.
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    # The host build. `index` renders Cargo's unit graph into one derivation per
    # rustc invocation (`cargoUnit`), so editing one crate rebuilds that crate and
    # its dependents instead of the whole workspace.
    index = {
      url = "github:indexable-inc/index";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.rust-overlay.follows = "rust-overlay";
    };
    # Rust -> Lean translation for `verify/` (Charon extracts MIR, Aeneas emits Lean).
    # Pinned to the rev whose Lean library `verify/lean/lakefile.toml` requires.
    aeneas.url = "github:AeneasVerif/aeneas/12a018bb0fab3333be572dadc0eab5108758552b";
  };
  outputs = {
    self,
    nixpkgs,
    index,
    aeneas,
    ...
  }: let
    systems = ["aarch64-darwin" "x86_64-linux"];
    forSystems = f: nixpkgs.lib.genAttrs systems f;
  in {
    packages = forSystems (system: let
      pkgs = import nixpkgs {inherit system;};
    in
      import ./nix/packages.nix {
        inherit pkgs;
        inherit (nixpkgs) lib;
        cargoUnit = index.lib.cargoUnitFor pkgs;
      });
    apps = forSystems (system: let
      pkgs = import nixpkgs {inherit system;};
      # Run from the repository root: `nix run .#verify`. It regenerates
      # verify/lean/*/Generated in the working tree, so it cannot run from the store.
      verify = pkgs.writeShellApplication {
        name = "loom-verify";
        runtimeInputs = [
          aeneas.packages.${system}.aeneas
          aeneas.packages.${system}.charon
          pkgs.elan
          pkgs.git
        ];
        text = ''
          if [ ! -x verify/check.sh ]; then
            echo "run from the loom repository root (verify/check.sh not found)" >&2
            exit 1
          fi
          exec verify/check.sh "$@"
        '';
      };
    in {
      repl = {
        type = "app";
        program = "${self.packages.${system}.repl}/bin/loom-repl";
      };
      verify = {
        type = "app";
        program = "${verify}/bin/loom-verify";
      };
    });
  };
}
