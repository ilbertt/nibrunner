import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import type { Heading, InlineCode, PhrasingContent, Root, RootContent, Table, Text } from 'mdast';
import type { MdxJsxAttribute, MdxJsxFlowElement, MdxJsxTextElement } from 'mdast-util-mdx-jsx';
import type { Plugin } from 'unified';
import { z } from 'zod';

/** What `just metrics-reference` writes: one entry per series the scrape page declares. */
const Metric = z.object({
  name: z.string(),
  help: z.string(),
  kind: z.string(),
  labels: z.array(z.string()),
});

type Metric = z.infer<typeof Metric>;

type Phrasing = PhrasingContent[];

const ELEMENT = 'MetricsReference';
const KIND_ELEMENT = 'MetricKind';
const REPOSITORY = resolve(import.meta.dirname, '../../..');
const SECTION = 2;
const APP_LABEL = 'app';
/** The leading separator the labels are joined with, dropped from the front. */
const SEPARATOR = 1;

/**
 * `<MetricsReference file="crates/nibrunnerd/metrics.json" />`, at the top level of a page,
 * becomes the reference of the scrape page: the series carrying an `app` label first, since those
 * are the ones a per-app dashboard reads, then the rest. `file` is relative to the repository. It
 * runs before the plugins that number headings and index the page, so what it writes is in the
 * table of contents and the search index like anything written by hand.
 */
export const remarkMetricsReference: Plugin<[], Root> = () => (tree) => {
  tree.children = tree.children.flatMap((node) =>
    node.type === 'mdxJsxFlowElement' && node.name === ELEMENT ? reference(node) : [node],
  );
};

function reference(node: MdxJsxFlowElement): RootContent[] {
  const metrics = read(attribute({ node, name: 'file' }));
  const perApp = metrics.filter((metric) => metric.labels.includes(APP_LABEL));
  const rest = metrics.filter((metric) => !metric.labels.includes(APP_LABEL));
  return [
    ...section({ name: 'Per app', metrics: perApp }),
    ...section({ name: 'The host', metrics: rest }),
  ];
}

function section({ name, metrics }: { name: string; metrics: Metric[] }): RootContent[] {
  if (metrics.length === 0) {
    return [];
  }
  return [
    heading({ depth: SECTION, value: name }),
    table({
      header: ['Series', 'Type', 'Labels', 'What it says'],
      rows: sorted(metrics).map((metric) => [
        [code(metric.name)],
        [kind(metric.kind)],
        labels(metric.labels),
        [text(metric.help)],
      ]),
    }),
  ];
}

/** By name, so a reader scans the table rather than the order the page happens to render in. */
function sorted(metrics: Metric[]): Metric[] {
  const byName = new Map(metrics.map((metric) => [metric.name, metric]));
  return [...byName.keys()].sort(compare).flatMap((name) => byName.get(name) ?? []);
}

const { compare } = new Intl.Collator('en');

/** Rendered by the component of that name, so the page shows the kind's icon beside its word. */
function kind(name: string): MdxJsxTextElement {
  return {
    type: 'mdxJsxTextElement',
    name: KIND_ELEMENT,
    attributes: [{ type: 'mdxJsxAttribute', name: 'kind', value: name }],
    children: [],
  };
}

function labels(names: string[]): Phrasing {
  if (names.length === 0) {
    return [text('—')];
  }
  return names.flatMap((name) => [text(', '), code(name)]).slice(SEPARATOR);
}

function attribute({ node, name }: { node: MdxJsxFlowElement; name: string }): string {
  const found = node.attributes.find(
    (candidate): candidate is MdxJsxAttribute =>
      candidate.type === 'mdxJsxAttribute' && candidate.name === name,
  );
  if (typeof found?.value !== 'string') {
    throw new Error(`<${ELEMENT}> needs a ${name}="…" attribute`);
  }
  return found.value;
}

function read(file: string): Metric[] {
  const parsed = z
    .array(Metric)
    .safeParse(JSON.parse(readFileSync(resolve(REPOSITORY, file), 'utf8')));
  if (!parsed.success) {
    throw new Error(
      `${file} is not a catalogue this reference reads:\n${z.prettifyError(parsed.error)}`,
    );
  }
  return parsed.data;
}

function table({ header, rows }: { header: string[]; rows: Phrasing[][] }): Table {
  return {
    type: 'table',
    children: [header.map((name) => [text(name)]), ...rows].map((cells) => ({
      type: 'tableRow',
      children: cells.map((children) => ({ type: 'tableCell', children })),
    })),
  };
}

function heading({ depth, value }: { depth: Heading['depth']; value: string }): Heading {
  return { type: 'heading', depth, children: [text(value)] };
}

function text(value: string): Text {
  return { type: 'text', value };
}

function code(value: string): InlineCode {
  return { type: 'inlineCode', value };
}
