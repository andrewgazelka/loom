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
  };
  outputs = {
    self,
    nixpkgs,
    index,
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
    apps = forSystems (system: {
      repl = {
        type = "app";
        program = "${self.packages.${system}.repl}/bin/loom-repl";
      };
    });
  };
}
