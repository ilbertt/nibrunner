import { createFileRoute, Link } from '@tanstack/react-router';
import { HomeLayout } from 'fumadocs-ui/layouts/home';
import { HostDemo } from '@/components/host-demo';
import { baseOptions } from '@/lib/layout.shared';

export const Route = createFileRoute('/')({
  component: Home,
});

function Intro() {
  return (
    <div className="flex flex-col items-start gap-4">
      <h1 className="font-semibold text-3xl tracking-tight">nibrunner</h1>
      <p className="text-fd-muted-foreground">
        MicroVM orchestrator for your VPS with built-in sleep/wake policies, backups, snapshots,
        HTTPS, custom image, logs and metrics.
      </p>
      <div className="flex flex-wrap gap-3">
        <Link
          to="/docs/$"
          params={{ _splat: 'getting-started/installation' }}
          className="rounded-lg bg-fd-primary px-3 py-2 font-medium text-fd-primary-foreground text-sm"
        >
          Quick start
        </Link>
        <Link
          to="/docs/$"
          params={{ _splat: '' }}
          className="rounded-lg border px-3 py-2 font-medium text-sm transition-colors hover:bg-fd-accent"
        >
          Docs
        </Link>
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
      <div className="flex w-full flex-1 flex-col px-4 py-10 lg:px-10 lg:py-12">
        <HostDemo intro={<Intro />} />
      </div>
    </HomeLayout>
  );
}
