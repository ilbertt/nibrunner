import { type ReactNode, useState } from 'react';
import { HostPanel } from '@/components/host-panel';
import { Warehouse } from '@/components/warehouse';
import { useHost } from '@/lib/use-host';

/**
 * The document beside the room it describes. `intro` sits above the document, and on a narrow
 * screen the room comes between them so the picture is seen before the controls.
 */
export function HostDemo({ intro }: { intro: ReactNode }) {
  const { snapshot, add, remove, visit, resize } = useHost();
  const [highlighted, setHighlighted] = useState<number | null>(null);
  return (
    <div className="grid w-full items-center gap-8 lg:grid-cols-[minmax(0,19rem)_1fr] lg:grid-rows-[auto_auto] lg:gap-x-10 lg:gap-y-8">
      <div className="lg:self-end">{intro}</div>
      <div className="lg:col-start-2 lg:row-span-2 lg:row-start-1">
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
