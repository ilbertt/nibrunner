import { CalloutDescription, CalloutTitle } from 'fumadocs-ui/components/callout';
import { Card } from 'fumadocs-ui/components/card';
import { Step, Steps } from 'fumadocs-ui/components/steps';
import defaultMdxComponents from 'fumadocs-ui/mdx';
import {
  ChartColumnIcon,
  CircleCheckIcon,
  CircleXIcon,
  GaugeIcon,
  InfoIcon,
  LightbulbIcon,
  SigmaIcon,
  TrendingUpIcon,
  TriangleAlertIcon,
} from 'lucide-react';
import type { MDXComponents } from 'mdx/types';
import type { ComponentProps, CSSProperties, ReactNode } from 'react';

const PANEL = 'panel-face rounded-md border-2 border-ink';

function PanelCard(props: ComponentProps<typeof Card>) {
  return <Card {...props} className={PANEL} />;
}

const CALLOUT_ICONS = {
  info: InfoIcon,
  warning: TriangleAlertIcon,
  error: CircleXIcon,
  success: CircleCheckIcon,
  idea: LightbulbIcon,
};

type CalloutKind = keyof typeof CALLOUT_ICONS;
type CalloutType = CalloutKind | 'warn' | 'tip';

function kindOf(type: CalloutType): CalloutKind {
  if (type === 'warn') {
    return 'warning';
  }
  if (type === 'tip') {
    return 'info';
  }
  return type;
}

/**
 * Fumadocs' callout is a card with a coloured bar down its inside and the text indented past the
 * icon; here the panel itself is the callout — border, rivets, tint and icon in the type's colour —
 * and the icon is its top-left rivet, with the title beside it and the text under the title, both
 * pulled into that corner so the other rivets keep clear of them. Same props, so the content is
 * unchanged.
 */
function Callout({
  type = 'info',
  title,
  icon,
  children,
}: {
  type?: CalloutType;
  title?: ReactNode;
  icon?: ReactNode;
  children?: ReactNode;
}) {
  const kind = kindOf(type);
  const Icon = CALLOUT_ICONS[kind];
  return (
    <div
      className="panel-face my-4 rounded-md border-(--callout-color) border-2 bg-(--callout-color)/10 p-4 text-fd-foreground text-sm [--color-ink:var(--callout-color)] [--rivet-top-left:none]"
      style={{ '--callout-color': `var(--color-fd-${kind})` } as CSSProperties}
    >
      <div className="-ms-2.5 -mt-2.5 flex gap-2">
        {icon ?? <Icon className="mt-0.5 size-4 shrink-0 text-(--callout-color)" />}
        <div className="flex min-w-0 flex-1 flex-col gap-2">
          {title && <CalloutTitle>{title}</CalloutTitle>}
          <CalloutDescription>{children}</CalloutDescription>
        </div>
      </div>
    </div>
  );
}

/** What a scraper rates a series as, and so how a reader is meant to query it. */
const METRIC_KIND_ICONS = {
  counter: TrendingUpIcon,
  gauge: GaugeIcon,
  histogram: ChartColumnIcon,
  summary: SigmaIcon,
};

type MetricKindName = keyof typeof METRIC_KIND_ICONS;

function isMetricKind(kind: string): kind is MetricKindName {
  return kind in METRIC_KIND_ICONS;
}

/**
 * A kind in the metrics reference. The word stays — it is what a reader searches the page for and
 * what a screen reader says — and the icon is beside it, so a column of eighty rows is scanned by
 * shape rather than read. Muted rather than coloured: a kind is not a status, and four colours
 * here would read as four severities.
 */
function MetricKind({ kind }: { kind: string }) {
  if (!isMetricKind(kind)) {
    return kind;
  }
  const Icon = METRIC_KIND_ICONS[kind];
  return (
    <span className="inline-flex items-center gap-1.5 whitespace-nowrap">
      <Icon aria-hidden className="size-3.5 shrink-0 text-fd-muted-foreground" />
      {kind}
    </span>
  );
}

export function getMDXComponents(components?: MDXComponents) {
  return {
    ...defaultMdxComponents,
    Card: PanelCard,
    Callout,
    MetricKind,
    Steps,
    Step,
    ...components,
  } satisfies MDXComponents;
}

export const useMDXComponents = getMDXComponents;

declare global {
  type MDXProvidedComponents = ReturnType<typeof getMDXComponents>;
}
