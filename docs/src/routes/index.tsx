import { createFileRoute, Link } from '@tanstack/react-router';
import { HomeLayout } from 'fumadocs-ui/layouts/home';
import { HostDemo } from '@/components/host-demo';
import { PageBackdrop } from '@/components/page-backdrop';
import { baseOptions } from '@/lib/layout.shared';
import { appName, description, gitConfig } from '@/lib/shared';

export const Route = createFileRoute('/')({
  component: Home,
});

/** The GitHub mark, as the header draws it; lucide has retired its brand icons. */
function GitHubMark({ className }: { className: string }) {
  return (
    <svg viewBox="0 0 24 24" fill="currentColor" aria-hidden="true" className={className}>
      <path d="M12 .297c-6.63 0-12 5.373-12 12 0 5.303 3.438 9.8 8.205 11.385.6.113.82-.258.82-.577 0-.285-.01-1.04-.015-2.04-3.338.724-4.042-1.61-4.042-1.61C4.422 18.07 3.633 17.7 3.633 17.7c-1.087-.744.084-.729.084-.729 1.205.084 1.838 1.236 1.838 1.236 1.07 1.835 2.809 1.305 3.495.998.108-.776.417-1.305.76-1.605-2.665-.3-5.466-1.332-5.466-5.93 0-1.31.465-2.38 1.235-3.22-.135-.303-.54-1.523.105-3.176 0 0 1.005-.322 3.3 1.23.96-.267 1.98-.399 3-.405 1.02.006 2.04.138 3 .405 2.28-1.552 3.285-1.23 3.285-1.23.645 1.653.24 2.873.12 3.176.765.84 1.23 1.91 1.23 3.22 0 4.61-2.805 5.625-5.475 5.92.42.36.81 1.096.81 2.22 0 1.606-.015 2.896-.015 3.286 0 .315.21.69.825.57C20.565 22.092 24 17.592 24 12.297c0-6.627-5.373-12-12-12" />
    </svg>
  );
}

function Intro() {
  return (
    <div className="flex flex-col items-start gap-4">
      <h1 className="font-semibold text-3xl tracking-tight">{appName}</h1>
      <p className="text-fd-muted-foreground">{description}.</p>
      <div className="flex flex-wrap gap-3">
        <Link
          to="/docs/$"
          params={{ _splat: 'getting-started/installation' }}
          className="key-face rounded bg-fd-primary px-3 py-2 font-medium text-fd-primary-foreground text-sm"
        >
          Quick start
        </Link>
        <a
          href={`https://github.com/${gitConfig.user}/${gitConfig.repo}`}
          target="_blank"
          rel="noreferrer"
          aria-label={`Star ${appName} on GitHub`}
          className="key-face inline-flex items-center gap-1.5 rounded border border-ink px-3 py-2 font-medium text-sm transition-colors hover:bg-fd-accent"
        >
          <GitHubMark className="size-4" />
          Star
        </a>
      </div>
    </div>
  );
}

/**
 * The homepage runs edge to edge so the room can fill the screen, and the header follows it: its
 * own padding lands where the page's does, so the two names line up.
 */
function Home() {
  return (
    <HomeLayout {...baseOptions()} className="lg:[--fd-layout-width:calc(100%-3rem)]">
      <PageBackdrop />
      <div className="flex w-full flex-1 flex-col px-4 py-10 lg:px-10 lg:py-12">
        <HostDemo intro={<Intro />} />
      </div>
    </HomeLayout>
  );
}
