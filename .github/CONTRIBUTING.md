# Contributing

## Development setup

```bash
git clone https://github.com/ilbertt/nibrunner.git
cd nibrunner
mise install
just build
```

`rustup` reads `rust-toolchain.toml` and installs the pinned toolchain on the first `cargo`
command. `mise install` puts `just` and `bun` on the path at the versions `mise.toml` pins.
`just` with no target lists every recipe.

The docs site has dependencies of its own: `cd docs && bun install` once, then `just docs-dev`.

## Tooling

- [Rust](https://www.rust-lang.org/) — the toolchain `rust-toolchain.toml` pins, through rustup;
  rustfmt and clippy come with it
- [just](https://just.systems/) — the recipes in `justfile`
- [mise](https://mise.jdx.dev/) — installs `just` and `bun` at the versions `mise.toml` pins
- [Bun](https://bun.sh) — runs and builds the docs site
- [Biome](https://biomejs.dev/) — linter and formatter for the docs site

## Commit messages

We use [Conventional Commits](https://www.conventionalcommits.org/). A pull request is
squash-merged with its title as the commit subject, so make sure the title is in the correct
format — `check-pr-title` refuses one that is not.

Everything else — the layout, the code style, the test lanes, what CI checks and the files that
are generated rather than written — is in [`AGENTS.md`](../AGENTS.md).
