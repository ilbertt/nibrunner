import { readFileSync } from 'node:fs';
import { basename, resolve } from 'node:path';
import GithubSlugger from 'github-slugger';
import type {
  Heading,
  InlineCode,
  Link,
  Paragraph,
  PhrasingContent,
  Root,
  RootContent,
  Table,
  Text,
} from 'mdast';
import { fromMarkdown } from 'mdast-util-from-markdown';
import type { MdxJsxAttribute, MdxJsxFlowElement } from 'mdast-util-mdx-jsx';
import type { Plugin } from 'unified';
import { z } from 'zod';

/** What schemars writes, as far as the reference reads it; any other keyword is dropped. */
const Schema = z.object({
  $id: z.string().optional(),
  $ref: z.string().optional(),
  get $defs() {
    return z.record(z.string(), Schema).optional();
  },
  title: z.string().optional(),
  description: z.string().optional(),
  type: z
    .union([z.string(), z.array(z.string())])
    .transform((type) => [type].flat())
    .optional(),
  get properties() {
    return z.record(z.string(), Schema).optional();
  },
  required: z.array(z.string()).optional(),
  get additionalProperties() {
    return z.union([Schema, z.boolean()]).optional();
  },
  get propertyNames() {
    return Schema.optional();
  },
  get items() {
    return Schema.optional();
  },
  minItems: z.number().optional(),
  maxItems: z.number().optional(),
  get anyOf() {
    return z.array(Schema).optional();
  },
  get oneOf() {
    return z.array(Schema).optional();
  },
  enum: z.array(z.unknown()).optional(),
  const: z.unknown().optional(),
  minimum: z.number().optional(),
  maximum: z.number().optional(),
  minLength: z.number().optional(),
  maxLength: z.number().optional(),
  pattern: z.string().optional(),
});
type Schema = z.infer<typeof Schema>;

/** A schema published on its own says where, and that is what the page links to. */
const Published = Schema.extend({ $id: z.string() });

type Phrasing = PhrasingContent[];

const ELEMENT = 'SchemaReference';
const REPOSITORY = resolve(import.meta.dirname, '../../..');
const DEFS = '#/$defs/';
const SECTION = 2;
const VARIANT = 3;

/**
 * `<SchemaReference file="deploy/config.schema.json" />`, at the top level of a page, becomes the
 * reference of that JSON Schema: a link to the schema, then a section per type — the root first,
 * the rest in the order the root reaches them — with a table of its properties, or its variants,
 * or the values it takes. `file` is relative to the repository. It runs before the plugins that
 * number headings and index the page, so what it writes is in the table of contents, the search
 * index and the Markdown of the page like anything written by hand.
 */
export const remarkSchemaReference: Plugin<[], Root> = () => (tree) => {
  tree.children = tree.children.flatMap((node) =>
    node.type === 'mdxJsxFlowElement' && node.name === ELEMENT ? reference(node) : [node],
  );
};

function reference(node: MdxJsxFlowElement): RootContent[] {
  const schema = read(attribute({ node, name: 'file' }));
  return [
    paragraph([text('Schema: '), link({ url: schema.$id, name: basename(schema.$id) })]),
    ...section({ name: schema.title ?? 'Root', schema, depth: SECTION }),
    ...ordered(schema).flatMap(([name, definition]) =>
      section({ name, schema: definition, depth: SECTION }),
    ),
  ];
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

function read(file: string): z.infer<typeof Published> {
  const parsed = Published.safeParse(JSON.parse(readFileSync(resolve(REPOSITORY, file), 'utf8')));
  if (!parsed.success) {
    throw new Error(
      `${file} is not a schema this reference reads:\n${z.prettifyError(parsed.error)}`,
    );
  }
  return parsed.data;
}

/**
 * Every definition the root reaches, breadth first, so a type follows what uses it; whatever it
 * does not reach comes last. The queue grows as it is walked, which is what makes it breadth first.
 */
function ordered(root: Schema): [string, Schema][] {
  const defs = root.$defs ?? {};
  const reached = new Map<string, Schema>();
  const queue = [root];
  for (const next of queue) {
    for (const name of referenced(next)) {
      const definition = defs[name];
      if (definition !== undefined && !reached.has(name)) {
        reached.set(name, definition);
        queue.push(definition);
      }
    }
  }
  return [...reached, ...Object.entries(defs).filter(([name]) => !reached.has(name))];
}

function referenced(schema: Schema): string[] {
  if (schema.$ref !== undefined) {
    return [definition(schema.$ref)];
  }
  return [
    ...properties(schema).map(([, property]) => property),
    ...(schema.items ? [schema.items] : []),
    ...(typeof schema.additionalProperties === 'object' ? [schema.additionalProperties] : []),
    ...(schema.anyOf ?? []),
    ...(schema.oneOf ?? []),
  ].flatMap(referenced);
}

function definition(ref: string): string {
  return decodeURIComponent(ref.startsWith(DEFS) ? ref.slice(DEFS.length) : ref);
}

/** Required properties first, in the order the schema lists them; the rest as they come. */
function properties(schema: Schema): [string, Schema][] {
  const all = schema.properties ?? {};
  const required = (schema.required ?? []).filter((name) => name in all);
  const rest = Object.keys(all).filter((name) => !required.includes(name));
  return [...required, ...rest].map((name) => [name, all[name]]);
}

function section({
  name,
  schema,
  depth,
}: {
  name: string;
  schema: Schema;
  depth: Heading['depth'];
}): RootContent[] {
  return [
    heading({ depth, value: name }),
    ...(schema.description === undefined ? [] : blocks(schema.description)),
    ...body(schema),
  ];
}

function body(schema: Schema): RootContent[] {
  if (schema.oneOf) {
    return variants(schema.oneOf);
  }
  if (schema.enum) {
    const values = joined({ parts: schema.enum.map((value) => [literal(value)]), separator: ', ' });
    return [paragraph([text('One of '), ...values, text('.')])];
  }
  if (schema.properties) {
    return [propertiesTable(schema)];
  }
  return [paragraph(phrase(schema))];
}

function variants(list: Schema[]): RootContent[] {
  if (list.every((variant) => variant.const !== undefined)) {
    return [
      table({
        header: ['Value', 'Description'],
        rows: list.map((variant) => [[literal(variant.const)], inline(variant.description)]),
      }),
    ];
  }
  const tag = discriminator(list);
  return [
    paragraph(
      tag === undefined
        ? [text('One of the following:')]
        : [text('One of the following, told apart by '), code(tag), text(':')],
    ),
    ...list.flatMap((variant) =>
      section({ name: variantName({ variant, tag }), schema: variant, depth: VARIANT }),
    ),
  ];
}

/** The property every variant fixes to a value of its own, if there is one. */
function discriminator(list: Schema[]): string | undefined {
  const [first, ...rest] = list;
  const shared = Object.entries(first?.properties ?? {}).find(
    ([name, property]) =>
      property.const !== undefined &&
      rest.every((variant) => variant.properties?.[name]?.const !== undefined),
  );
  return shared?.[0];
}

function variantName({ variant, tag }: { variant: Schema; tag: string | undefined }): string {
  const fixed = tag === undefined ? undefined : variant.properties?.[tag]?.const;
  if (fixed !== undefined) {
    return String(fixed);
  }
  return (variant.required ?? Object.keys(variant.properties ?? {})).join(', ') || 'variant';
}

function propertiesTable(schema: Schema): Table {
  const required = new Set(schema.required ?? []);
  return table({
    header: ['Name', 'Type', 'Description'],
    rows: properties(schema).map(([name, property]) => [
      required.has(name)
        ? [code(name)]
        : [code(name), text(' '), { type: 'emphasis', children: [text('optional')] }],
      phrase(property),
      inline(property.description),
    ]),
  });
}

/** A type, the way a table cell says it: a link for a named one, a word and its bounds otherwise. */
function phrase(schema: Schema): Phrasing {
  if (schema.$ref !== undefined) {
    const name = definition(schema.$ref);
    return [link({ url: `#${new GithubSlugger().slug(name)}`, name })];
  }
  const union = schema.anyOf ?? schema.oneOf;
  if (union) {
    return joined({ parts: union.map(phrase), separator: ' | ' });
  }
  if (schema.const !== undefined) {
    return [literal(schema.const)];
  }
  if (schema.enum) {
    return joined({ parts: schema.enum.map((value) => [literal(value)]), separator: ' | ' });
  }
  const types = schema.type ?? [];
  if (types.length === 0) {
    return [text('any')];
  }
  return joined({ parts: types.map((type) => typed({ type, schema })), separator: ' | ' });
}

/** One JSON type as a phrase, its qualifiers after a comma each. */
function typed({ type, schema }: { type: string; schema: Schema }): Phrasing {
  const qualified = (parts: Phrasing[]) => joined({ parts, separator: ', ' });
  switch (type) {
    case 'array':
      return qualified([
        [text('array of '), ...phrase(schema.items ?? {})],
        ...bounds({ min: schema.minItems, max: schema.maxItems, unit: 'items' }),
      ]);
    case 'object':
      if (typeof schema.additionalProperties !== 'object') {
        return [text('object')];
      }
      return qualified([
        [text('map of '), ...phrase(schema.additionalProperties)],
        ...matching({ pattern: schema.propertyNames?.pattern, of: 'keys' }),
      ]);
    case 'integer':
    case 'number':
      return qualified([[text(type)], ...bounds({ min: schema.minimum, max: schema.maximum })]);
    case 'string':
      return qualified([
        [text('string')],
        ...bounds({ min: schema.minLength, max: schema.maxLength, unit: 'characters' }),
        ...matching({ pattern: schema.pattern }),
      ]);
    default:
      return [text(type)];
  }
}

/** The qualifier a lower or upper bound is, or none. */
function bounds({ min, max, unit }: { min?: number; max?: number; unit?: string }): Phrasing[] {
  const of = unit === undefined ? '' : ` ${unit}`;
  if (min !== undefined && max !== undefined) {
    return [[text(`${min} to ${max}${of}`)]];
  }
  if (min !== undefined) {
    return [[text(`at least ${min}${of}`)]];
  }
  if (max !== undefined) {
    return [[text(`at most ${max}${of}`)]];
  }
  return [];
}

/** The qualifier a pattern is, or none. */
function matching({ pattern, of }: { pattern?: string; of?: string }): Phrasing[] {
  if (pattern === undefined) {
    return [];
  }
  return [[text(of === undefined ? 'matching ' : `${of} matching `), code(pattern)]];
}

function joined({ parts, separator }: { parts: Phrasing[]; separator: string }): Phrasing {
  const [first = [], ...rest] = parts;
  return [...first, ...rest.flatMap((part) => [text(separator), ...part])];
}

/** A description in one line, for a table cell. */
function inline(description: string | undefined): Phrasing {
  if (description === undefined) {
    return [];
  }
  const paragraphs = blocks(description).map((block) =>
    block.type === 'paragraph' ? block.children : [text(plain(block))],
  );
  return joined({ parts: paragraphs, separator: ' ' });
}

function plain(node: RootContent): string {
  if ('value' in node) {
    return node.value;
  }
  if ('children' in node) {
    return node.children.map(plain).join('');
  }
  return '';
}

/** A Rust doc comment wraps at a column; only a blank line is a break. */
function blocks(description: string): RootContent[] {
  return fromMarkdown(description.replace(/([^\n])\n(?!\n)/g, '$1 ')).children;
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

function paragraph(children: Phrasing): Paragraph {
  return { type: 'paragraph', children };
}

function link({ url, name }: { url: string; name: string }): Link {
  return { type: 'link', url, children: [text(name)] };
}

function literal(value: unknown): InlineCode {
  return code(JSON.stringify(value));
}

function text(value: string): Text {
  return { type: 'text', value };
}

function code(value: string): InlineCode {
  return { type: 'inlineCode', value };
}
