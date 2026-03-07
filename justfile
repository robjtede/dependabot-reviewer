_list:
    @just --list

# Check project.
check:
    just --unstable --fmt --check
    nixpkgs-fmt --check .
    fd --type=file --hidden --extension=md --extension=yml --exec-batch prettier --check
    fd --hidden --extension=toml --exec-batch taplo format --check
    fd --hidden --extension=toml --exec-batch taplo lint
    # TODO: figure out how to run cargo +nightly fmt within nix direnv
    rustup run nightly rustfmt --edition=2024 src/main.rs --check
    @just clippy
    cargo shear

# Format project.
fmt:
    just --unstable --fmt
    nixpkgs-fmt .
    fd --type=file --hidden --extension=md --extension=yml --exec-batch prettier --write
    fd --hidden --extension=toml --exec-batch taplo format
    # TODO: figure out how to run cargo +nightly fmt within nix direnv
    rustup run nightly rustfmt --edition=2024 src/main.rs

clippy:
    cargo clippy -- --deny=warnings --deny=clippy::todo

# Run the binary via Nix (passes arguments to the app).
run *args:
    nix run . -- {{ args }}

# Build the binary in Nix.
build:
    nix build --no-link --print-out-paths .
