# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.8](https://github.com/robjtede/dependabot-reviewer/compare/v0.1.7...v0.1.8) - 2026-09-17

### Fixed

- show keyboard controls when selecting repositories

## [0.1.7](https://github.com/robjtede/dependabot-reviewer/compare/v0.1.6...v0.1.7) - 2026-09-17

### Added

- offer Dependabot rebase after conflicted merges

### Fixed

- explain PR discovery failures and rate limit retry times
- continue processing PRs after approval or merge failures
- hide non-passing CI action when all checks pass

## [0.1.6](https://github.com/robjtede/dependabot-reviewer/compare/v0.1.5...v0.1.6) - 2026-09-17

### Added

- add distinct merge action for non-passing CI

### Other

- *(deps)* bump impl-more from 0.3.5 to 0.3.7 ([#66](https://github.com/robjtede/dependabot-reviewer/pull/66))

## [0.1.5](https://github.com/robjtede/dependabot-reviewer/compare/v0.1.4...v0.1.5) - 2026-09-10

### Fixed

- stop progress rows when processing fails

### Other

- *(deps)* bump toml from 1.1.4+spec-1.1.0 to 1.1.5+spec-1.1.0

## [0.1.4](https://github.com/robjtede/dependabot-reviewer/compare/v0.1.3...v0.1.4) - 2026-08-18

### Added

- Option to close PRs.

## [0.1.3](https://github.com/robjtede/dependabot-reviewer/compare/v0.1.2...v0.1.3) - 2026-08-03

No significant changes since `0.1.2`.

## [0.1.2](https://github.com/robjtede/dependabot-reviewer/compare/v0.1.1...v0.1.2) - 2026-08-03

### Added

- Show the current action for each pull request while processing it. ([#37](https://github.com/robjtede/dependabot-reviewer/pull/37))

## [0.1.1](https://github.com/robjtede/dependabot-reviewer/compare/v0.1.0...v0.1.1) - 2026-08-01

### Added

- Guide first-time setup with GitHub CLI authentication, one-off repository runs, and saved default organizations.
- Document prebuilt-binary installation with cargo-binstall.

## [0.1.0](https://github.com/robjtede/dependabot-reviewer/compare/v0.0.0...v0.1.0) - 2026-08-01

### Added

- Review Dependabot pull requests across GitHub organizations.
- Open unreviewed pull requests in a browser.
- Approve and merge updates, including auto-merge and merge queues.
- Request that Dependabot rebases or recreates updates.
- Support interactive selection, saved default organizations, dry runs, and `gh` authentication.
