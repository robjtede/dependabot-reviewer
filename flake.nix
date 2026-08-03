{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";

    flake-parts.url = "github:hercules-ci/flake-parts";

    x52 = {
      url = "github:x52dev/nix";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.flake-parts.follows = "flake-parts";
    };
  };

  outputs = inputs @ { flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];

      perSystem = { pkgs, config, inputs', ... }:
        {
          # Rust is managed outside Nix (e.g. rustup / CI setup action).
          # Keep the flake focused on auxiliary tooling.
          devShells.default = pkgs.mkShell {
            packages = [
              config.formatter
              inputs'.x52.packages.x52-release-tools
              pkgs.cargo-shear
              pkgs.cargo-hack
              pkgs.fd
              pkgs.just
              pkgs.nodePackages.prettier
              pkgs.taplo
            ];
          };

          devShells.ci-release = pkgs.mkShellNoCC {
            packages = [
              inputs'.x52.packages.x52-release-tools
            ];
          };

          formatter = pkgs.nixpkgs-fmt;
        };
    };
}
