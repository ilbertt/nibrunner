import { type ReactNode, useState } from 'react';
import { HostPanel } from '@/components/host-panel';
import { Warehouse } from '@/components/warehouse';
import { useHost } from '@/lib/use-host';

/**
 * The apps beside the room they run in. `intro` sits above the list, and on a narrow screen the
 * room comes between them so the picture is seen before the controls. On a wide one the room
 * stays put while the list, which can outgrow the screen, scrolls beside it.
 */
export function HostDemo({ intro }: { intro: ReactNode }) {
  const { snapshot, add, remove, visit, resize } = useHost();
  const [highlighted, setHighlighted] = useState<number | null>(null);
  return (
    <div className="grid w-full items-center gap-8 lg:grid-cols-[minmax(0,19rem)_1fr] lg:grid-rows-[auto_auto] lg:gap-x-10 lg:gap-y-8">
      <div className="lg:self-end">{intro}</div>
      <div className="lg:sticky lg:top-26 lg:col-start-2 lg:row-span-2 lg:row-start-1 lg:self-start">
        <Warehouse
          snapshot={snapshot}
          highlighted={highlighted}
          onVisit={visit}
          onHighlight={setHighlighted}
        />
      </div>
      <div className="lg:self-start">
        <HostPanel
          apps={snapshot.apps}
          highlighted={highlighted}
          onAdd={add}
          onVisit={visit}
          onRemove={remove}
          onResize={resize}
          onHighlight={setHighlighted}
        />
      </div>
    </div>
  );
}
