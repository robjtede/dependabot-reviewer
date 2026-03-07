_list:
    @just --list

# Check project
check:
    just --unstable --fmt --check
    nixpkgs-fmt --check .
    fd --type=file --hidden --extension=md --extension=yml --exec-batch prettier --check
    fd --hidden --extension=toml --exec-batch taplo format --check
    fd --hidden --extension=toml --exec-batch taplo lint
    cargo +nightly fmt -- --check
    @just clippy
    cargo shear

# Format project
fmt:
    just --unstable --fmt
    nixpkgs-fmt .
    fd --type=file --hidden --extension=md --extension=yml --exec-batch prettier --write
    fd --hidden --extension=toml --exec-batch taplo format
    cargo +nightly fmt

clippy:
    cargo clippy -- --deny=warnings --deny=clippy::todo

# Run the binary (passes arguments to the app)
run:
    cargo run -- $*
