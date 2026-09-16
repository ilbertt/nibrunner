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
        One binary that turns a Linux machine with <code>/dev/kvm</code> into a microVM host.
        Describe the apps you want in a document; it boots each one in a Firecracker microVM of its
        own, sleeps the ones nobody is visiting, backs their volumes up, and serves them over HTTPS
        with logs and metrics.
      </p>
      <p className="text-fd-muted-foreground text-sm">
        Not shipping containers on a cargo ship: parcels on a warehouse floor, each one a whole
        machine of its own, up in a blink and shelved for nothing while nobody is looking.
      </p>
      <div className="flex flex-wrap gap-3">
        <Link
          to="/docs/$"
          params={{ _splat: 'quick-start' }}
          className="rounded-lg bg-fd-primary px-3 py-2 font-medium text-fd-primary-foreground text-sm"
        >
          Quick start
        </Link>
        <Link
          to="/docs/$"
          params={{ _splat: '' }}
          className="rounded-lg border px-3 py-2 font-medium text-sm transition-colors hover:bg-fd-accent"
        >
          Open Docs
        </Link>
      </div>
    </div>
  );
}

function Home() {
  return (
    <HomeLayout {...baseOptions()}>
      <div className="mx-auto flex w-full max-w-7xl flex-1 flex-col justify-center px-4 py-10 lg:py-12">
        <HostDemo intro={<Intro />} />
      </div>
    </HomeLayout>
  );
}
