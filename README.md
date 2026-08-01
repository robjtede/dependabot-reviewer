# dependabot-reviewer

`dependabot-reviewer` reviews Dependabot pull requests across GitHub repositories. It can open unreviewed pull requests, approve and merge updates, or ask Dependabot to rebase or recreate updates.

The program uses `GITHUB_TOKEN`. In an interactive terminal, it can also use the token from `gh auth token`.

## Install

Install a prebuilt binary with [`cargo-binstall`](https://github.com/cargo-bins/cargo-binstall):

```sh
cargo binstall dependabot-reviewer
```

Or build and install from source:

```sh
cargo install dependabot-reviewer
```

## Use

Select an action interactively for the Dependabot pull requests in an organization:

```sh
dependabot-reviewer --org owner
```

Use `--dry-run` to show the selected action without changing pull requests:

```sh
dependabot-reviewer --org owner --repo owner/repository --action approve-merge --dry-run
```

Run `dependabot-reviewer --help` to see all options.

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
