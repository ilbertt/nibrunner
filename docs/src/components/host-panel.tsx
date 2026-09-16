import { GlobeIcon, PlusIcon, XIcon } from 'lucide-react';
import { type App, MAX_APPS, SIZE_NAMES, SIZES, type Size } from '@/lib/host';

/** What the card says the app is doing, in the words the daemon would use. */
function phaseOf(app: App): string {
  switch (app.pending) {
    case 'place':
      return app.location.kind === 'queued' ? 'queued' : 'booting';
    case 'shelve':
      return 'snapshotting';
    case 'wake':
      return 'waking';
    case 'resize':
      return 'resizing';
    case 'remove':
      return 'leaving';
    default:
      break;
  }
  if (app.location.kind !== 'placed') {
    return 'queued';
  }
  return app.asleep ? 'sleeping' : 'running';
}

/** Three cells, like the calculator's steps: the one it has in its own tint, the rest plain. */
function SizePicker({
  app,
  enabled,
  onPick,
}: {
  app: App;
  enabled: boolean;
  onPick: (size: Size) => void;
}) {
  return (
    <fieldset className="flex gap-1" aria-label={`Size of ${app.name}`}>
      {SIZE_NAMES.map((size) => {
        const picked = size === app.size;
        return (
          <button
            key={size}
            type="button"
            aria-pressed={picked}
            disabled={!enabled}
            onClick={() => onPick(size)}
            className="flex flex-1 flex-col items-center rounded-sm border py-1 font-mono text-[10px] leading-tight transition-colors enabled:hover:bg-fd-accent disabled:opacity-50"
            style={
              picked
                ? { backgroundColor: app.tint, borderColor: app.tint, color: 'oklch(0.2 0 0)' }
                : undefined
            }
          >
            <span>{SIZES[size].memory}</span>
            <span className={picked ? 'opacity-70' : 'text-fd-muted-foreground'}>
              {SIZES[size].vcpu} vCPU
            </span>
          </button>
        );
      })}
    </fieldset>
  );
}

function AppRow({
  app,
  active,
  onVisit,
  onRemove,
  onResize,
  onHighlight,
}: {
  app: App;
  active: boolean;
  onVisit: () => void;
  onRemove: () => void;
  onResize: (size: Size) => void;
  onHighlight: (on: boolean) => void;
}) {
  const settled = app.location.kind === 'placed' && app.pending === null;
  const queued = app.location.kind === 'queued' && app.pending === null;
  return (
    <li
      onMouseEnter={() => onHighlight(true)}
      onMouseLeave={() => onHighlight(false)}
      onFocusCapture={() => onHighlight(true)}
      onBlurCapture={() => onHighlight(false)}
      className="flex flex-col gap-2 rounded-lg border px-3 py-2 transition-colors"
      style={{ borderColor: active ? app.tint : undefined }}
    >
      <div className="flex items-center gap-2.5">
        <span className="size-2.5 shrink-0 rounded-sm" style={{ backgroundColor: app.tint }} />
        <span className="truncate font-mono text-sm">{app.name}</span>
        <span className="ml-auto shrink-0 text-fd-muted-foreground text-xs">{phaseOf(app)}</span>
        <button
          type="button"
          disabled={!settled || app.location.kind !== 'placed'}
          onClick={onVisit}
          aria-label={`Send ${app.name} a request`}
          title="A visit from the internet: a sleeping app wakes for it"
          className="rounded-md p-1 text-fd-muted-foreground transition-colors enabled:hover:bg-fd-accent enabled:hover:text-fd-accent-foreground disabled:opacity-30"
        >
          <GlobeIcon className="size-3.5" />
        </button>
        <button
          type="button"
          disabled={!settled}
          onClick={onRemove}
          aria-label={`Remove ${app.name}`}
          title="Take it out of the document"
          className="rounded-md p-1 text-fd-muted-foreground transition-colors enabled:hover:bg-fd-accent enabled:hover:text-fd-accent-foreground disabled:opacity-30"
        >
          <XIcon className="size-3.5" />
        </button>
      </div>
      <SizePicker app={app} enabled={settled || queued} onPick={onResize} />
    </li>
  );
}

export function HostPanel({
  apps,
  highlighted,
  onAdd,
  onVisit,
  onRemove,
  onResize,
  onHighlight,
}: {
  apps: App[];
  highlighted: number | null;
  onAdd: () => void;
  onVisit: (app: number) => void;
  onRemove: (app: number) => void;
  onResize: (change: { id: number; size: Size }) => void;
  onHighlight: (app: number | null) => void;
}) {
  const listed = apps.filter((app) => app.location.kind !== 'gone');
  const full = listed.length >= MAX_APPS;
  const running = listed.filter((app) => app.location.kind === 'placed' && !app.asleep).length;
  const sleeping = listed.filter((app) => app.asleep).length;
  return (
    <div className="flex flex-col gap-4">
      <div className="flex flex-wrap items-baseline justify-between gap-x-3 gap-y-1">
        <span className="shrink-0 font-medium text-sm">Your apps</span>
        <span className="whitespace-nowrap font-mono text-fd-muted-foreground text-xs tabular-nums">
          {running} running · {sleeping} asleep · {MAX_APPS - listed.length} free
        </span>
      </div>
      <ul className="flex flex-col gap-2">
        {listed.map((app) => (
          <AppRow
            key={app.id}
            app={app}
            active={highlighted === app.id}
            onVisit={() => onVisit(app.id)}
            onRemove={() => onRemove(app.id)}
            onResize={(size) => onResize({ id: app.id, size })}
            onHighlight={(on) => onHighlight(on ? app.id : null)}
          />
        ))}
      </ul>
      <button
        type="button"
        disabled={full}
        onClick={onAdd}
        className="inline-flex w-full items-center justify-center gap-2 rounded-lg border bg-fd-secondary px-3 py-2 font-medium text-fd-secondary-foreground text-sm transition-colors enabled:hover:bg-fd-accent disabled:opacity-50"
      >
        <PlusIcon className="size-4" />
        Add an app
      </button>
    </div>
  );
}
