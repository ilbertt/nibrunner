import { createGetUrl } from 'fumadocs-core/source';

export const appName = 'nibrunner';
export const siteUrl = 'https://nibrunner.dev';
export const tagline = 'MicroVM orchestrator for your VPS';
// The README's, word for word.
export const description = `${tagline} with built-in sleep/wake policies, backups, snapshots, HTTPS, custom image, logs and metrics`;
export const docsRoute = '/docs';
export const docsImageRoute = '/og/docs';

export const gitConfig = {
  user: 'ilbertt',
  repo: 'nibrunner',
  branch: 'main',
};

const getDocsUrl = createGetUrl(docsRoute);

/** What a page says about itself, to the tab and to whatever unfurls a link to it. */
export function pageMeta({ title, description }: { title: string; description: string }) {
  return [
    { title },
    { name: 'description', content: description },
    { property: 'og:title', content: title },
    { property: 'og:description', content: description },
    { name: 'twitter:title', content: title },
    { name: 'twitter:description', content: description },
  ];
}

export function getPageMarkdownUrl(page: { slugs: string[]; locale?: string }) {
  const segments = [...page.slugs];
  if (segments.length === 0) {
    segments.push('index.md');
  } else {
    segments[segments.length - 1] += '.md';
  }

  return { segments, url: getDocsUrl(segments, page.locale) };
}

/** @returns page slugs */
export function decodeMarkdownUrl(segments: string[]) {
  if (segments.length === 0) {
    return [];
  }

  const out = [...segments];
  out[out.length - 1] = out[out.length - 1].replace(/\.md$/, '');
  if (out.length === 1 && out[0] === 'index') {
    out.pop();
  }
  return out;
}
