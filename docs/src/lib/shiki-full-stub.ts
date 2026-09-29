import { createShikiFactory } from 'fumadocs-core/highlight/shiki';

/**
 * fumadocs-openapi's `utils/create-page` statically imports `fumadocs-core/highlight/shiki/full`
 * for its fallback, so every grammar Shiki ships — eight megabytes — lands in the bundle even
 * through `ui/base`, which says it is the entry without them. This stands in for it. Nothing ever
 * calls it: the page is built with a `shiki` of its own, and `options.shiki ?? defaultShikiFactory`
 * never reaches the right-hand side.
 */
export const defaultShikiFactory = createShikiFactory({
  init() {
    throw new Error('the filesystem reference highlights through its own two-language Shiki');
  },
});

export const wasmShikiFactory = defaultShikiFactory;
