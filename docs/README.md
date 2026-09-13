# docs

The nibrunner docs site: a [Fumadocs](https://fumadocs.dev) app on TanStack Start, whose pages are
the Markdown under `content/docs/`. Bun runs it, at the version `mise.toml` pins.

From the repo root:

```bash
just docs-install   # once
just docs-dev       # http://localhost:3000
just docs-build     # the static site, into .output/public
just docs-check     # types, then Biome: lint, formatting, import order
just docs-fix       # rewrites what docs-check would refuse, where Biome can
```

`biome.json` here says `root: false`, which is what lets an editor opened on the repo apply it.
Run Biome from the root, pointed at `docs/`, as the recipes do — started inside `docs/` it finds no
root configuration and uses none of this one.
