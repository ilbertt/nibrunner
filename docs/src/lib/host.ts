export type Vec2 = { x: number; z: number };
export type Rect = { x0: number; x1: number; z0: number; z1: number };

/** What the demo host is laid out for: `max_apps`. */
export const MAX_APPS = 16;

/** The floor in cells, and how tall the walls stand. */
export const ROOM = { x: 12, y: 1.6, z: 7.5 };

export type Size = 'S' | 'M' | 'L';

export type Dims = { w: number; d: number; height: number };

/**
 * A running app is a crate as wide as its cores and as tall as its memory, the way the pricing
 * calculator draws one. Two cores wide at most: on a floor of six by four, no seven crates can
 * leave two cells side by side nowhere, so a crate always has somewhere to go.
 */
export const SIZES = {
  S: { w: 1, d: 1, height: 0.42, vcpu: 1, memory: '256 MiB' },
  M: { w: 1, d: 1, height: 0.7, vcpu: 1, memory: '512 MiB' },
  L: { w: 2, d: 1, height: 0.85, vcpu: 2, memory: '1 GiB' },
} satisfies Record<Size, Dims & { vcpu: number; memory: string }>;

/** A crate stands on a dark base set in from its sides, with room under it for the robot. */
export const BASE = { height: 0.22, inset: 0.05 };

export const SIZE_NAMES = Object.keys(SIZES) as Size[];

/**
 * Asleep, an app is its memory written to disk: one thin slab whatever it runs as, which is why
 * the shelf holds every app the host is laid out for in a corner of the floor. The floor holds
 * less than that: what the machine runs at once is what its memory holds, and past it the robot
 * has only advice.
 */
export const SLAB: Dims = { w: 1, d: 1, height: 0.14 };

export type Form = 'crate' | 'slab';

/** Base and body together: the whole of what stands on the floor. */
const CRATE_DIMS: Record<Size, Dims> = {
  S: { w: SIZES.S.w, d: SIZES.S.d, height: BASE.height + SIZES.S.height },
  M: { w: SIZES.M.w, d: SIZES.M.d, height: BASE.height + SIZES.M.height },
  L: { w: SIZES.L.w, d: SIZES.L.d, height: BASE.height + SIZES.L.height },
};

export function dimsFor({ form, size }: { form: Form; size: Size }): Dims {
  return form === 'slab' ? SLAB : CRATE_DIMS[size];
}

/** A crate stops short of its cells' edges, so neighbours read as two crates rather than one. */
const CRATE_INSET = 0.06;

/**
 * Low and flat: it slides under a crate and lifts it clear of the floor on its platter. Just
 * narrower than the base it goes under, so its name on its side is as large as it can be and
 * the base still hides it.
 */
export const BOT = { width: 0.76, height: 0.2, lift: 0.26, speed: 20 };

/**
 * Two zones: the floor, under the panel on the back wall that every running app is cabled to
 * the internet through, and the shelf where a snapshotted app waits without costing anything.
 * Both fill from the back wall outwards.
 */
export const ZONES = {
  floor: { x0: 5.5, x1: 11.5, z0: 0.5, z1: 4.5 },
  shelf: { x0: 0.5, x1: 4.5, z0: 0.5, z1: 4.5 },
} satisfies Record<string, Rect>;

export type ZoneName = keyof typeof ZONES;

/** What an app of this size takes up in a zone: its crate on the floor, a slab on the shelf. */
export function footprintIn({ zone, size }: { zone: ZoneName; size: Size }): Dims {
  return dimsFor({ form: zone === 'shelf' ? 'slab' : 'crate', size });
}

/** The pad at the room's open edge where a crate declared in the document rises. */
export const PAD: Vec2 = { x: 10.5, z: 6.25 };

/** Where a removed crate is set down: over the front edge, so it drops out of the picture. */
export const EXIT: Vec2 = { x: 4, z: 7.2 };

/** The robot's rest, out of everyone's way. */
export const PARK: Vec2 = { x: 7, z: 6.3 };

/** The internet panel on the back wall, and how high on it the cables leave. */
export const NET_PANEL = { y0: 0.15, y1: 1.3, tap: 1.2 };

export type Spot = { zone: ZoneName; x0: number; z0: number };

export function spotRect({ spot, size }: { spot: Spot; size: Size }): Rect {
  const { w, d } = footprintIn({ zone: spot.zone, size });
  return { x0: spot.x0, x1: spot.x0 + w, z0: spot.z0, z1: spot.z0 + d };
}

export function centreOf(rect: Rect): Vec2 {
  return { x: (rect.x0 + rect.x1) / 2, z: (rect.z0 + rect.z1) / 2 };
}

/** The cells something this big covers when its middle is here. */
export function cellsAround({ at, dims }: { at: Vec2; dims: Dims }): Rect {
  const { w, d } = dims;
  return { x0: at.x - w / 2, x1: at.x + w / 2, z0: at.z - d / 2, z1: at.z + d / 2 };
}

/** The crate itself, a little inside its cells. */
export function crateRect(cells: Rect): Rect {
  return {
    x0: cells.x0 + CRATE_INSET,
    x1: cells.x1 - CRATE_INSET,
    z0: cells.z0 + CRATE_INSET,
    z1: cells.z1 - CRATE_INSET,
  };
}

export type Location =
  | { kind: 'queued' }
  | { kind: 'pad' }
  | { kind: 'bot' }
  | { kind: 'placed'; spot: Spot }
  | { kind: 'gone' };

export type App = {
  id: number;
  name: string;
  tint: string;
  size: Size;
  /** A crate while it runs; a slab, its snapshot, once it is put to sleep. */
  form: Form;
  location: Location;
  /** When it got where it is; arrivals and departures animate from it. */
  since: number;
  /** The last request it served: the pulse down its cable, and how fresh its traffic looks. */
  visitedAt: number;
  /** Set when a request finds it asleep, until it is back on the floor. */
  requestedAt: number | null;
  /** Declared, but with no room on the floor for it yet; it waits for something to leave. */
  stranded: boolean;
  asleep: boolean;
  /** What the robot has been asked to do with it and has not finished yet. */
  pending: 'place' | 'shelve' | 'wake' | 'resize' | 'remove' | null;
  /** The cells kept for it while it is on its way. */
  reserved: Spot | null;
  /** What it is growing or shrinking from: a resize, or a snapshot taken or restored. */
  morph: { from: Dims; at: number } | null;
};

// What one runs on a box of one's own: things built for oneself, and the single-binary kind of
// open source, which is the program the docs describe. Nothing that wants a runtime and a
// database beside it. Enough for the host's sixteen, the first three standing at the start.
const NAMES = [
  'context-use',
  'pocketbase',
  'gitea',
  'blog',
  'open-connector',
  'api',
  'telegram-bot',
  'docs',
  'vaultwarden',
  'ntfy',
  'memos',
  'mcp',
  'meilisearch',
  'headscale',
  'agent',
  'syncthing',
];

// One family of muted tints, far enough apart to tell neighbours apart, none near the robot's
// orange or the internet's cyan.
const TINTS = [
  'oklch(0.72 0.09 250)',
  'oklch(0.72 0.09 160)',
  'oklch(0.74 0.1 25)',
  'oklch(0.7 0.08 300)',
  'oklch(0.72 0.1 345)',
  'oklch(0.7 0.08 130)',
  'oklch(0.72 0.07 275)',
  'oklch(0.74 0.09 190)',
];

const SIZE_CYCLE: Size[] = ['S', 'M', 'L', 'S', 'M', 'S', 'L', 'S'];

export function createApp({ id, taken, at }: { id: number; taken: string[]; at: number }): App {
  return {
    id,
    name: NAMES.find((name) => !taken.includes(name)) ?? `app-${id}`,
    tint: TINTS[id % TINTS.length]!,
    size: SIZE_CYCLE[id % SIZE_CYCLE.length]!,
    form: 'crate',
    location: { kind: 'queued' },
    since: at,
    visitedAt: at,
    requestedAt: null,
    stranded: false,
    asleep: false,
    pending: null,
    reserved: null,
    morph: null,
  };
}

/** Long enough to be seen running; short enough that the shelving is seen too. */
export const SLEEP_AFTER_MS = 20_000;

function overlaps({ a, b }: { a: Rect; b: Rect }): boolean {
  return a.x0 < b.x1 && b.x0 < a.x1 && a.z0 < b.z1 && b.z0 < a.z1;
}

/** Every cell rectangle held in a zone: crates standing there, and cells kept for ones coming. */
function heldIn({ zone, apps }: { zone: ZoneName; apps: App[] }): Rect[] {
  const held: Rect[] = [];
  for (const app of apps) {
    if (app.location.kind === 'placed' && app.location.spot.zone === zone) {
      held.push(spotRect({ spot: app.location.spot, size: app.size }));
    }
    if (app.reserved !== null && app.reserved.zone === zone) {
      held.push(spotRect({ spot: app.reserved, size: app.size }));
    }
  }
  return held;
}

/**
 * The first cells a crate fits in, nearest the back wall and then leftmost, so a zone fills as a
 * wall of crates and a gap left by one that went is the next one's place.
 */
export function freeSpot({
  zone,
  size,
  apps,
  except = null,
}: {
  zone: ZoneName;
  size: Size;
  apps: App[];
  /** An app whose own cells do not count, because it is the one about to move. */
  except?: number | null;
}): Spot | null {
  const bounds = ZONES[zone];
  const held = heldIn({ zone, apps: apps.filter((app) => app.id !== except) });
  const { w, d } = footprintIn({ zone, size });
  for (let z0 = bounds.z0; z0 + d <= bounds.z1; z0 += 1) {
    for (let x0 = bounds.x0; x0 + w <= bounds.x1; x0 += 1) {
      const spot = { zone, x0, z0 };
      const rect = spotRect({ spot, size });
      if (!held.some((one) => overlaps({ a: one, b: rect }))) {
        return spot;
      }
    }
  }
  return null;
}

// Route planning for a loaded robot: a cell per quarter unit, crates on the floor grown by what
// the load hangs over the robot's edges, and the diagonal allowed.
const STEP = 0.25;
const COLS = Math.round(ROOM.x / STEP) + 1;
const ROWS = Math.round(ROOM.z / STEP) + 1;
const NEIGHBOURS = [
  { di: 1, dj: 0 },
  { di: -1, dj: 0 },
  { di: 0, dj: 1 },
  { di: 0, dj: -1 },
  { di: 1, dj: 1 },
  { di: 1, dj: -1 },
  { di: -1, dj: 1 },
  { di: -1, dj: -1 },
];

function cellOf(at: Vec2): { i: number; j: number } {
  return { i: Math.round(at.x / STEP), j: Math.round(at.z / STEP) };
}

function pointOf({ i, j }: { i: number; j: number }): Vec2 {
  return { x: i * STEP, z: j * STEP };
}

function blockedMap(blocked: Rect[]): boolean[] {
  const map = new Array<boolean>(COLS * ROWS).fill(false);
  for (let j = 0; j < ROWS; j += 1) {
    for (let i = 0; i < COLS; i += 1) {
      const { x, z } = pointOf({ i, j });
      map[j * COLS + i] = blocked.some(
        (rect) => x > rect.x0 && x < rect.x1 && z > rect.z0 && z < rect.z1,
      );
    }
  }
  return map;
}

function octile({ a, b }: { a: { i: number; j: number }; b: { i: number; j: number } }): number {
  const di = Math.abs(a.i - b.i);
  const dj = Math.abs(a.j - b.j);
  return Math.max(di, dj) + (Math.SQRT2 - 1) * Math.min(di, dj);
}

type Search = { map: boolean[]; goal: number; cameFrom: Int32Array; cost: Float64Array };

function relax({ search, from, to }: { search: Search; from: number; to: number }): boolean {
  const i = to % COLS;
  const j = Math.floor(to / COLS);
  const diagonal = Math.abs(i - (from % COLS)) === 1 && Math.abs(j - Math.floor(from / COLS)) === 1;
  const next = search.cost[from]! + (diagonal ? Math.SQRT2 : 1);
  if ((search.map[to] && to !== search.goal) || next >= search.cost[to]!) {
    return false;
  }
  search.cost[to] = next;
  search.cameFrom[to] = from;
  return true;
}

function neighboursOf(index: number): number[] {
  const i = index % COLS;
  const j = Math.floor(index / COLS);
  const found: number[] = [];
  for (const { di, dj } of NEIGHBOURS) {
    const ni = i + di;
    const nj = j + dj;
    if (ni >= 0 && ni < COLS && nj >= 0 && nj < ROWS) {
      found.push(nj * COLS + ni);
    }
  }
  return found;
}

/** A* over the cells; the way back from the goal, or nothing if the goal cannot be reached. */
function search({ map, start, goal }: { map: boolean[]; start: number; goal: number }): number[] {
  const cost = new Float64Array(COLS * ROWS).fill(Number.POSITIVE_INFINITY);
  const cameFrom = new Int32Array(COLS * ROWS).fill(-1);
  const state: Search = { map, goal, cameFrom, cost };
  const goalCell = { i: goal % COLS, j: Math.floor(goal / COLS) };
  const open = new Set<number>([start]);
  cost[start] = 0;
  while (open.size > 0) {
    let current = -1;
    let best = Number.POSITIVE_INFINITY;
    for (const index of open) {
      const guess =
        cost[index]! + octile({ a: { i: index % COLS, j: Math.floor(index / COLS) }, b: goalCell });
      if (guess < best) {
        best = guess;
        current = index;
      }
    }
    open.delete(current);
    if (current === goal) {
      break;
    }
    for (const next of neighboursOf(current)) {
      if (relax({ search: state, from: current, to: next })) {
        open.add(next);
      }
    }
  }
  if (cameFrom[goal] === -1) {
    return [];
  }
  const way: number[] = [];
  for (let at = goal; at !== start; at = cameFrom[at]!) {
    way.push(at);
  }
  return way.reverse();
}

const SIGHT_STEP = 0.1;

function clearBetween({ map, from, to }: { map: boolean[]; from: Vec2; to: Vec2 }): boolean {
  const length = Math.hypot(to.x - from.x, to.z - from.z);
  const samples = Math.ceil(length / SIGHT_STEP);
  for (let sample = 1; sample < samples; sample += 1) {
    const t = sample / samples;
    const { i, j } = cellOf({ x: from.x + (to.x - from.x) * t, z: from.z + (to.z - from.z) * t });
    if (map[j * COLS + i]) {
      return false;
    }
  }
  return true;
}

/** Straightens the cell walk into as few legs as the crates allow. */
function pulled({ map, from, points }: { map: boolean[]; from: Vec2; points: Vec2[] }): Vec2[] {
  const legs: Vec2[] = [];
  let at = from;
  let index = 0;
  while (index < points.length) {
    let reach = index;
    for (let probe = points.length - 1; probe > index; probe -= 1) {
      if (clearBetween({ map, from: at, to: points[probe]! })) {
        reach = probe;
        break;
      }
    }
    at = points[reach]!;
    legs.push(at);
    index = reach + 1;
  }
  return legs;
}

/**
 * The legs of a trip from `from` to `to` that keeps `load` clear of `crates`. An empty robot
 * fits under a crate's base and goes straight.
 */
export function planRoute({
  from,
  to,
  load,
  crates,
}: {
  from: Vec2;
  to: Vec2;
  load: Dims | null;
  crates: Rect[];
}): Vec2[] {
  if (load === null || crates.length === 0) {
    return [to];
  }
  const { w, d } = load;
  const clearance = { x: w / 2 - CRATE_INSET, z: d / 2 - CRATE_INSET };
  const map = blockedMap(
    crates.map((crate) => ({
      x0: crate.x0 - clearance.x,
      x1: crate.x1 + clearance.x,
      z0: crate.z0 - clearance.z,
      z1: crate.z1 + clearance.z,
    })),
  );
  const start = cellOf(from);
  const goal = cellOf(to);
  const way = search({ map, start: start.j * COLS + start.i, goal: goal.j * COLS + goal.i });
  if (way.length === 0) {
    return [to];
  }
  const points = way.map((index) => pointOf({ i: index % COLS, j: Math.floor(index / COLS) }));
  points[points.length - 1] = to;
  return pulled({ map, from, points });
}
