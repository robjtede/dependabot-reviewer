_list:
    @just --list

# Check project
check:
    just --unstable --fmt --check
    # nixpkgs-fmt --check .
    fd --type=file --hidden --extension=md --extension=yml --exec-batch prettier --check
    fd --hidden --extension=toml --exec-batch taplo format --check
    fd --hidden --extension=toml --exec-batch taplo lint
    cargo +nightly fmt -- --check
    cargo clippy --workspace --all-features

# Format project
fmt:
    just --unstable --fmt
    # nixpkgs-fmt .
    fd --type=file --hidden --extension=md --extension=yml --exec-batch prettier --write
    fd --hidden --extension=toml --exec-batch taplo format
    cargo +nightly fmt
