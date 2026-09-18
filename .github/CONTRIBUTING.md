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
- [mise](https://mise.jdx.dev/) — installs `just` and `bun`
- [Bun](https://bun.sh) — runs and builds the docs site
- [Biome](https://biomejs.dev/) — linter and formatter for the docs site

## Commit messages

We use [Conventional Commits](https://www.conventionalcommits.org/). A pull request is
squash-merged with its title as the commit subject, so make sure the title is in the correct
format — `check-pr-title` refuses one that is not.

Everything else — the layout, the code style, the test lanes, what CI checks and the files that
are generated rather than written — is in [`AGENTS.md`](../AGENTS.md).
