# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/robjtede/dependabot-reviewer/compare/v0.0.0...v0.1.0) - 2026-08-01

### Added

- add gh auth token flag
- add failing CI triage prompt
- allow non-passing CI option
- allow pre-CI approve + auto merge

### Fixed

- handle structured GraphQL responses
- fall back when merge queue checks are expected
- accept double approve when needed
- fix dependabot config location
- fix multi-approve case

### Other

- add crate publishing metadata
- add draft binary release workflow
- Merge pull request #30 from robjtede/dependabot/cargo/toml-1.1.4spec-1.1.0
- Merge pull request #31 from robjtede/dependabot/cargo/impl-more-0.3.5
- Bump camino from 1.2.4 to 1.2.5
- Merge pull request #28 from robjtede/dependabot/cargo/clap-4.6.4
- Bump impl-more from 0.3.1 to 0.3.2
- Merge pull request #26 from robjtede/dependabot/cargo/toml-1.1.3spec-1.1.0
- Bump clap from 4.6.1 to 4.6.2
- Merge pull request #21 from robjtede/dependabot/github_actions/actions-rust-lang/setup-rust-toolchain-1.17.0
- Merge pull request #22 from robjtede/dependabot/cargo/camino-1.2.4
- Merge pull request #23 from robjtede/dependabot/github_actions/taiki-e/install-action-2.82.7
- Bump console from 0.16.3 to 0.16.4
- Merge pull request #17 from robjtede/dependabot/github_actions/actions-rust-lang/setup-rust-toolchain-1.16.1
- Merge pull request #18 from robjtede/dependabot/github_actions/taiki-e/install-action-2.81.3
- Bump actions/checkout from 6.0.2 to 6.0.3
- Merge pull request #15 from robjtede/dependabot/github_actions/actions-rust-lang/setup-rust-toolchain-1.16.0
- Bump taiki-e/install-action from 2.73.0 to 2.75.28
- Bump the cargo group across 1 directory with 2 updates
- Bump clap from 4.6.0 to 4.6.1
- Bump semver from 1.0.27 to 1.0.28
- Merge pull request #7 from robjtede/dependabot/github_actions/actions-rust-lang/setup-rust-toolchain-1.15.4
- Merge pull request #9 from robjtede/dependabot/cargo/octocrab-0.49.7
- Bump toml from 1.1.0+spec-1.1.0 to 1.1.2+spec-1.1.0
- fix clippy
- Merge pull request #5 from robjtede/dependabot/cargo/toml-1.1.0spec-1.1.0
- Bump octocrab from 0.49.5 to 0.49.6
- multi-repo review
- Merge pull request #1 from robjtede/dependabot/cargo/console-0.16.3
- Merge pull request #2 from robjtede/dependabot/cargo/clap-4.6.0
- Merge pull request #3 from robjtede/dependabot/cargo/toml-1.0.7spec-1.1.0
- Bump rustls-webpki in the cargo group across 1 directory
- only open unreviewed
- pin actions
- unhide depbot config
- install just
- nextest
- fmt
- default orgs in settings file
- fix nightly fmt
- handle merge queues
- debug print config path
- state tracking
- fix permissions
- fix permissions
- install nightly rust if needed
- add fd to devShell packages
- migrate CI workflow to use Determinate Nix and Flakes
- night fmt
- prevent binary link in nix build
- nix build
- nixify
- modules
- modules
- ci status and skip failing
- show urls
- cli action choice
- approve and merge
- remove justfile
- support recreate
- concurrent pr lookup
- concurrent pr comments
- use derive_more
- use error-stack
- use octocrab
- init cargo project
- mv script
- init
