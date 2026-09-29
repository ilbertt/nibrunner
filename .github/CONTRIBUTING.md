# Contributing

## Development setup

```bash
git clone https://github.com/ilbertt/nibrunner.git
cd nibrunner
mise install
just build
```

The docs site: `cd docs && bun install`, then `just docs-dev`.

## Tooling

- [Rust](https://www.rust-lang.org/) — pinned by `rust-toolchain.toml`
- [just](https://just.systems/) — task runner
- [mise](https://mise.jdx.dev/) — installs the tools pinned in `mise.toml`
- [Bun](https://bun.sh) — runs and builds the docs site
- [Biome](https://biomejs.dev/) — linter and formatter for the docs site

## Commit messages

We use [Conventional Commits](https://www.conventionalcommits.org/). A pull request is
squash-merged with its title as the commit subject, so make sure the title is in the correct
format — `check-pr-title` refuses one that is not.

## Releases

1. Run the `prepare-release` workflow. It opens a PR with the next date-based version and changelog.
   GitHub Actions must be allowed to create pull requests in the repository's Actions settings.
2. Review the PR, approve its workflow runs if GitHub requests it, and merge once checks pass.
3. Fetch `main`, tag the PR's merge commit as `vYYYY.M.D-N`, and push the tag. The `release` workflow
   builds and publishes the binaries, guest image and checksums using the committed release notes.

The date is UTC, and the counter starts at 1 each day, following the highest existing tag for that
date. `git-cliff` generates the changelog from Conventional Commits since the previous tag. These
are regular GitHub releases, even though Cargo treats the counter as a prerelease suffix.

Everything else — the layout, the code style, the test lanes, what CI checks and the files that
are generated rather than written — is in [`AGENTS.md`](../AGENTS.md).
