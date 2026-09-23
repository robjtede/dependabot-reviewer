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

Approval and merge skip GitHub Actions updates when the target branch contains `.github/workflows/actions.lock`. Dependabot cannot update this lockfile. This guard also applies to grouped updates, dry runs, and `--allow-non-passing-ci`. Other dependency types are not affected. If the lockfile check fails, approval and merge stop. The pull request list marks these updates as `will not merge: actions.lock`.

If a merge fails, the tool continues with the remaining pull requests. After the batch, interactive runs offer to post `@dependabot rebase` for pull requests that still have merge conflicts. Non-interactive runs print instructions instead. A rebase request does not merge the pull request; run the tool again after Dependabot updates it and CI completes. The command exits with an error if any merge failed.

Run `dependabot-reviewer --help` to see all options.

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
