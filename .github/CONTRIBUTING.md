# Contributing

## Tools

`rustup` reads `rust-toolchain.toml` and installs the pinned toolchain on the first `cargo`
command. `mise install` puts `just` and `bun` on the path at the versions `mise.toml` pins.
`just` with no target lists every recipe.

```bash
just build           # the whole workspace, for the machine you are on
just build-release   # one static x86_64 Linux binary, what a host runs
```

`build-release` links against musl: an x86_64 Linux box needs `musl-tools`, anything else
crosses through `zig` and `cargo-zigbuild`.

## Tests

```bash
just test          # everything that needs no kernel
just integration   # everything that does: root, Linux, nft, mke2fs, /dev/net/tun
```

The first lane is the planner, the health state machine, the backoff, the ruleset asserted as
text, the codecs against byte fixtures taken from the C headers, and the reconcile pass driven
against mocked collaborators. The second is the only place a ruleset load, a real `mke2fs` or a
tap is ever considered proven. `just integration --no-run` builds it without any of that.

## What CI checks

Every pull request runs `just fmt --check`, `just lint`, `just test`, `just integration`, and the
three checks on files written from the code rather than by hand:

- `crates/protocol/schema/*.json` is generated from the types in `crates/protocol`. After
  changing them, `just schema` and commit the result; `just check-schema` fails otherwise.
- `deploy/config.example.toml` is generated from `HostConfig::example` in
  `crates/nibrunnerd/src/config.rs`. After changing it, `just config-example` and commit the
  result; `just check-config-example` fails otherwise.
- `deploy/config.schema.json` is generated from `HostConfig::schema` in the same file. After
  changing it, `just config-schema` and commit the result; `just check-config-schema` fails
  otherwise.

`just fmt` and `just lint` cover the Rust and the docs site both.

## The docs site

`docs/` is a Fumadocs app on Bun, served at [nibrunner.dev](https://nibrunner.dev). Pages are
Markdown under `docs/content/docs/`. `bun install` in there once, then `just docs-dev`.

## Pull requests

Work goes on a branch and lands on `main` through a pull request, squash-merged with the pull
request's title as the commit subject. That title is a
[Conventional Commit](https://www.conventionalcommits.org/) — `feat:`, `fix:`, `docs:`,
`chore:`, … — and `check-pr-title` refuses one that is not.

A comment earns its place only when it says something the code cannot: a tradeoff, an external
constraint, a `Safety:` note. A comment that narrates what the code does is a sign the code
should say it instead.
