{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";

    flake-parts.url = "github:hercules-ci/flake-parts";

    crane.url = "github:ipetkov/crane";
  };

  outputs = inputs @ { flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];

      perSystem = { pkgs, config, lib, ... }:
        let
          # crane.mkLib is a top-level (system-independent) flake output, not a
          # per-system one, so we reach it through the raw `inputs` set rather
          # than through flake-parts' per-system `inputs'` accessor.
          craneLib = inputs.crane.mkLib pkgs;

          # Filter sources to only Rust/Cargo-relevant files so that changes
          # to flake.nix, README, CI configs, etc. do not invalidate the
          # cargo build cache.
          src = craneLib.cleanCargoSource (craneLib.path ./.);

          # Arguments shared across all crane derivations.
          # Anything placed here affects the cache key of cargoArtifacts,
          # so keep it to things that are truly common / structural.
          commonArgs = {
            inherit src;

            # Prevent accidental host-dependency leakage; also required for
            # correct cross-compilation behaviour.
            strictDeps = true;

            buildInputs = lib.optionals pkgs.stdenv.isDarwin [
              # libiconv is not part of the default macOS stdenv but is needed
              # by many Rust crates that link against system libraries.
              pkgs.libiconv
            ];

            # Add extra *native* build tools here, e.g.:
            # nativeBuildInputs = [ pkgs.pkg-config ];
          };

          # Build only the Cargo dependencies in isolation.
          # Crane replaces the real source with a stub so that this derivation
          # is only invalidated when Cargo.lock / Cargo.toml files change, not
          # when application source changes.  The resulting store path can be
          # shared (e.g. via Cachix) across all subsequent crane derivations in
          # this flake.
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;

          # The final application package – builds only the workspace crates,
          # reusing the pre-built dependency artifacts above.
          dependabot-reviewer = craneLib.buildPackage (commonArgs // {
            inherit cargoArtifacts;

            # Set doCheck = false here if you run tests via a separate
            # cargoNextest / cargoTest check derivation below, to avoid
            # running tests twice.
            # doCheck = false;
          });
        in
        {
          # ── Packages ────────────────────────────────────────────────────────
          packages = {
            default = dependabot-reviewer;
            inherit dependabot-reviewer;
          };

          # ── Dev shell ────────────────────────────────────────────────────────
          # craneLib.devShell automatically provides cargo, rustc, clippy and
          # rustfmt from the same toolchain used to build.  Passing `checks`
          # here means all build inputs from above checks are available
          # interactively.
          devShells.default = craneLib.devShell {
            packages = [
              config.formatter
              pkgs.just
              pkgs.taplo
              pkgs.cargo-hack
              pkgs.cargo-shear
              pkgs.nodePackages.prettier
            ];
          };

          formatter = pkgs.nixpkgs-fmt;
        };
    };
}
