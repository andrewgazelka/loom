{
  description = "Loom actor runtime and complete guest toolchains";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  outputs = { self, nixpkgs }: {
    packages = nixpkgs.lib.genAttrs [ "aarch64-darwin" "x86_64-linux" ] (system:
      let
        pkgs = import nixpkgs { inherit system; };
      in import ./nix/packages.nix { inherit pkgs; inherit (nixpkgs) lib; });
  };
}
