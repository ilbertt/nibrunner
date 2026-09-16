import { type ReactNode, useState } from 'react';
import { HostPanel } from '@/components/host-panel';
import { Warehouse } from '@/components/warehouse';
import { useHost } from '@/lib/use-host';

/**
 * The apps beside the room they run in. `intro` sits above the list, and on a narrow screen the
 * room comes between them so the picture is seen before the controls. On a wide one the column
 * is the screen's height and only the list scrolls, so the room, the intro and the button never
 * move however many apps there are.
 */
export function HostDemo({ intro }: { intro: ReactNode }) {
  const { snapshot, add, remove, visit, resize } = useHost();
  const [highlighted, setHighlighted] = useState<number | null>(null);
  return (
    <div className="grid w-full items-start gap-8 lg:h-[calc(100vh-9.5rem)] lg:grid-cols-[minmax(0,19rem)_1fr] lg:grid-rows-[auto_minmax(0,1fr)] lg:gap-x-12 lg:gap-y-8">
      <div>{intro}</div>
      <div className="lg:col-start-2 lg:row-span-2 lg:row-start-1 lg:flex lg:h-full lg:min-h-0 lg:items-center lg:justify-center">
        <Warehouse
          snapshot={snapshot}
          highlighted={highlighted}
          onVisit={visit}
          onHighlight={setHighlighted}
        />
      </div>
      <div className="lg:flex lg:min-h-0 lg:flex-col lg:self-stretch">
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
