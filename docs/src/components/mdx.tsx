import { Callout } from 'fumadocs-ui/components/callout';
import { Card } from 'fumadocs-ui/components/card';
import { Step, Steps } from 'fumadocs-ui/components/steps';
import defaultMdxComponents from 'fumadocs-ui/mdx';
import type { MDXComponents } from 'mdx/types';
import type { ComponentProps } from 'react';

const PANEL = 'panel-face rounded-md border-2 border-ink';

function PanelCard(props: ComponentProps<typeof Card>) {
  return <Card {...props} className={PANEL} />;
}

function PanelCallout(props: ComponentProps<typeof Callout>) {
  return <Callout {...props} className={PANEL} />;
}

export function getMDXComponents(components?: MDXComponents) {
  return {
    ...defaultMdxComponents,
    Card: PanelCard,
    Callout: PanelCallout,
    Steps,
    Step,
    ...components,
  } satisfies MDXComponents;
}

export const useMDXComponents = getMDXComponents;

declare global {
  type MDXProvidedComponents = ReturnType<typeof getMDXComponents>;
}
