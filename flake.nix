{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";

    flake-parts.url = "github:hercules-ci/flake-parts";
  };

  outputs = inputs @ { flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];

      perSystem = { pkgs, config, ... }:
        {
          # Rust is managed outside Nix (e.g. rustup / CI setup action).
          # Keep the flake focused on auxiliary tooling.
          devShells.default = pkgs.mkShell {
            packages = [
              config.formatter
              pkgs.cargo-shear
              pkgs.cargo-hack
              pkgs.fd
              pkgs.just
              pkgs.nodePackages.prettier
              pkgs.taplo
            ];
          };

          formatter = pkgs.nixpkgs-fmt;
        };
    };
}
