# dependabot-reviewer

`dependabot-reviewer` reviews Dependabot pull requests across GitHub repositories. It can open unreviewed pull requests, approve and merge updates, close updates, or ask Dependabot to rebase or recreate them.

## Install

Install a prebuilt binary with [`cargo-binstall`](https://github.com/cargo-bins/cargo-binstall):

```sh
cargo binstall dependabot-reviewer
```

Or build and install from source:

```sh
cargo install dependabot-reviewer
```

## First run

Authenticate with GitHub CLI:

```sh
gh auth login
```

Review an organization for this run:

```sh
dependabot-reviewer --org owner
```

Or review one repository without saving any configuration:

```sh
dependabot-reviewer --repo owner/repository
```

Save an organization as the default for later runs:

```sh
dependabot-reviewer --org owner --save-default-orgs
```

Then run `dependabot-reviewer` without `--org`. In non-interactive environments, set `GITHUB_TOKEN` or use `--use-gh-auth-token`.

## Use

Select an action interactively for the Dependabot pull requests in an organization:

```sh
dependabot-reviewer --org owner
```

Use `--dry-run` to show the selected action without changing pull requests:

```sh
dependabot-reviewer --org owner --repo owner/repository --action approve-merge --dry-run
```

Use `--action close` to close selected pull requests. Add `--dry-run` to preview the action first.

Run `dependabot-reviewer --help` to see all options.

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
