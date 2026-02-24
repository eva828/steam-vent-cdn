{
  description = "Steam Workshop CDN downloader";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      fenix,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        rustToolchain = fenix.packages.${system}.stable.toolchain;
        naersk = pkgs.callPackage "${fenix}/pkgs/naersk.nix" {
          cargo = rustToolchain;
          rustc = rustToolchain;
        };
      in
      {
        packages.default = naersk.buildPackage {
          src = ./.;
          buildInputs = [ pkgs.git ];
        };

        devShells.default = pkgs.mkShell {
          buildInputs = [
            rustToolchain
          ];
          shellHook = ''
            rustc --version
          '';
        };
      }
    );
}
