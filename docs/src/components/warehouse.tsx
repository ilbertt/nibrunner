import {
  type App,
  BASE,
  BOT,
  cellsAround,
  centreOf,
  crateRect,
  type Dims,
  dimsFor,
  EXIT,
  NET_PANEL,
  PAD,
  type Rect,
  ROOM,
  SIZES,
  SLAB,
  SLEEP_AFTER_MS,
  spotRect,
  type Vec2,
  ZONES,
} from '@/lib/host';
import {
  ARRIVAL_MS,
  NOTICE_MS,
  type Notice,
  type Pose,
  PULSE_MS,
  type Snapshot,
} from '@/lib/use-host';

type Vec3 = { x: number; y: number; z: number };
type Point = { px: number; py: number };
type Segment = { from: Vec3; to: Vec3 };

const UNIT_PX = 40;
const EDGE_ANGLE_DEG = 30;
const HALF_TURN_DEG = 180;
const EDGE_COS = Math.cos((EDGE_ANGLE_DEG * Math.PI) / HALF_TURN_DEG);
const EDGE_SIN = Math.sin((EDGE_ANGLE_DEG * Math.PI) / HALF_TURN_DEG);
const HALF = 0.5;
const MS_PER_SECOND = 1000;

function project({ x, y, z }: Vec3): Point {
  return { px: (x - z) * EDGE_COS * UNIT_PX, py: ((x + z) * EDGE_SIN - y) * UNIT_PX };
}

function polygonPoints(vertices: Vec3[]): string {
  return vertices
    .map((vertex) => {
      const { px, py } = project(vertex);
      return `${px.toFixed(2)},${py.toFixed(2)}`;
    })
    .join(' ');
}

// The room never changes size, so the view is cut once: the room's corners, and a margin for
// the rail over the floor and a crate dropping off the front edge.
const MARGIN = 0.8;

function viewBox(): string {
  const corners = [0, ROOM.x].flatMap((x) =>
    [0, ROOM.y].flatMap((y) => [0, ROOM.z].map((z) => project({ x, y, z }))),
  );
  const xs = corners.map((corner) => corner.px);
  const ys = corners.map((corner) => corner.py);
  const pad = MARGIN * UNIT_PX;
  const left = Math.min(...xs) - pad;
  const top = Math.min(...ys) - pad;
  return `${left} ${top} ${Math.max(...xs) + pad - left} ${Math.max(...ys) + pad - top}`;
}

const VIEW_BOX = viewBox();

function ticksUpTo(count: number): number[] {
  const ticks: number[] = [];
  for (let tick = 0; tick <= count; tick += 1) {
    ticks.push(tick);
  }
  return ticks;
}

function roomSurfaces(): Vec3[][] {
  return [
    [
      { x: 0, y: 0, z: 0 },
      { x: ROOM.x, y: 0, z: 0 },
      { x: ROOM.x, y: 0, z: ROOM.z },
      { x: 0, y: 0, z: ROOM.z },
    ],
    [
      { x: 0, y: 0, z: 0 },
      { x: 0, y: ROOM.y, z: 0 },
      { x: 0, y: ROOM.y, z: ROOM.z },
      { x: 0, y: 0, z: ROOM.z },
    ],
    [
      { x: 0, y: 0, z: 0 },
      { x: 0, y: ROOM.y, z: 0 },
      { x: ROOM.x, y: ROOM.y, z: 0 },
      { x: ROOM.x, y: 0, z: 0 },
    ],
  ];
}

function segmentKey({ from, to }: Segment): string {
  return [from, to]
    .map((vertex) => `${vertex.x},${vertex.y},${vertex.z}`)
    .sort()
    .join('|');
}

// The floor and the two walls each contribute the three edges they share with a neighbour.
function withoutSharedEdges(segments: Segment[]): Segment[] {
  const seen = new Set<string>();
  return segments.filter((segment) => {
    const key = segmentKey(segment);
    const unseen = !seen.has(key);
    seen.add(key);
    return unseen;
  });
}

function roomGrid(): Segment[] {
  const xs = ticksUpTo(ROOM.x);
  const ys = ticksUpTo(Math.floor(ROOM.y));
  const zs = ticksUpTo(ROOM.z);
  return withoutSharedEdges([
    ...xs.map((x) => ({ from: { x, y: 0, z: 0 }, to: { x, y: 0, z: ROOM.z } })),
    ...zs.map((z) => ({ from: { x: 0, y: 0, z }, to: { x: ROOM.x, y: 0, z } })),
    ...ys.map((y) => ({ from: { x: 0, y, z: 0 }, to: { x: 0, y, z: ROOM.z } })),
    ...zs.map((z) => ({ from: { x: 0, y: 0, z }, to: { x: 0, y: ROOM.y, z } })),
    ...ys.map((y) => ({ from: { x: 0, y, z: 0 }, to: { x: ROOM.x, y, z: 0 } })),
    ...xs.map((x) => ({ from: { x, y: 0, z: 0 }, to: { x, y: ROOM.y, z: 0 } })),
  ]);
}

const GRID = roomGrid();
const SURFACES = roomSurfaces();

function floorQuad(rect: Rect): Vec3[] {
  return [
    { x: rect.x0, y: 0, z: rect.z0 },
    { x: rect.x1, y: 0, z: rect.z0 },
    { x: rect.x1, y: 0, z: rect.z1 },
    { x: rect.x0, y: 0, z: rect.z1 },
  ];
}

type Aabb = { x0: number; x1: number; y0: number; y1: number; z0: number; z1: number };

/** A box standing on the floor plane: its footprint, and how tall it is. */
type Solid = {
  key: string;
  rect: Rect;
  y0: number;
  y1: number;
  tint: string;
  /** How far toward the page it is washed out: a sleeping app is a ghost of one, not a see-through. */
  fade: number;
  opacity: number;
  /** The app this is part of, for pointing at it. */
  app: number | null;
  /** The name written on it, if any, and the face that carries it: a slab is read from above. */
  label: string | null;
  labelFace: 'px' | 'pz' | 'top';
  /** How brightly the strip along its base is lit: an app on the floor, serving. */
  glow: number;
  aabb: Aabb;
};

function solid({
  key,
  rect,
  y0,
  y1,
  tint,
  fade = 0,
  opacity = 1,
  app = null,
  label = null,
  labelFace = 'pz',
  glow = 0,
}: {
  key: string;
  rect: Rect;
  y0: number;
  y1: number;
  tint: string;
  fade?: number;
  opacity?: number;
  app?: number | null;
  label?: string | null;
  labelFace?: 'px' | 'pz' | 'top';
  glow?: number;
}): Solid {
  return {
    key,
    rect,
    y0,
    y1,
    tint,
    fade,
    opacity,
    app,
    label,
    labelFace,
    glow,
    aabb: { ...rect, y0, y1 },
  };
}

type Standing = {
  app: App;
  at: Vec2;
  y0: number;
  fade: number;
  opacity: number;
  dims: Dims;
  glow: number;
};

const MORPH_MS = 260;

/** An app's shape right now: the one it has, or on the way there from the one it had. */
function dimsOf({ app, now, pace }: { app: App; now: number; pace: number }): Dims {
  const to = dimsFor({ form: app.form, size: app.size });
  if (app.morph === null) {
    return to;
  }
  const t = progress({ since: app.morph.at, now, ms: MORPH_MS, pace });
  const from = app.morph.from;
  return {
    w: from.w + (to.w - from.w) * t,
    d: from.d + (to.d - from.d) * t,
    height: from.height + (to.height - from.height) * t,
  };
}

/** A slab is too thin to write on its side, so its name goes on top. */
function labelFaceOf(app: App): 'px' | 'pz' | 'top' {
  return app.form === 'slab' ? 'top' : 'pz';
}

const LID = { inset: 0.08, height: 0.04 };
const LID_MIX = 'white 30%';
const BASE_TINT = 'oklch(0.3 0.012 260)';
/** Thinner than this, and it is a slab: no lid, and the name on top. */
const SLAB_BELOW = 0.3;

/** A crate standing with its underside at `y0`: the box, and the lid set into its top. */
function crateSolids({ app, at, y0, fade, opacity, dims, glow }: Standing): Solid[] {
  const body = crateRect({
    x0: at.x - dims.w * HALF,
    x1: at.x + dims.w * HALF,
    z0: at.z - dims.d * HALF,
    z1: at.z + dims.d * HALF,
  });
  const top = y0 + dims.height;
  if (dims.height < SLAB_BELOW) {
    return [
      solid({
        key: `crate-${app.id}`,
        rect: body,
        y0,
        y1: top,
        tint: app.tint,
        fade,
        opacity,
        app: app.id,
        label: app.name,
        labelFace: 'top',
      }),
    ];
  }
  // The base takes what is left once the body has kept its share, so a crate on its way to
  // being a slab loses the base first and the body last.
  const base = Math.min(BASE.height, Math.max(0, dims.height - SLAB.height));
  return [
    solid({
      key: `base-${app.id}`,
      rect: {
        x0: body.x0 + BASE.inset,
        x1: body.x1 - BASE.inset,
        z0: body.z0 + BASE.inset,
        z1: body.z1 - BASE.inset,
      },
      y0,
      y1: y0 + base,
      tint: BASE_TINT,
      fade,
      opacity,
      app: app.id,
      glow,
    }),
    solid({
      key: `crate-${app.id}`,
      rect: body,
      y0: y0 + base,
      y1: top,
      tint: app.tint,
      fade,
      opacity,
      app: app.id,
      label: app.name,
      labelFace: labelFaceOf(app),
    }),
    solid({
      key: `lid-${app.id}`,
      rect: {
        x0: body.x0 + LID.inset,
        x1: body.x1 - LID.inset,
        z0: body.z0 + LID.inset,
        z1: body.z1 - LID.inset,
      },
      y0: top,
      y1: top + LID.height,
      tint: mixed({ tint: app.tint, mix: LID_MIX }),
      fade,
      opacity,
      app: app.id,
      // On the lid as well as the front: a crate in the row ahead hides the front.
      label: app.name,
      labelFace: 'top',
    }),
  ];
}

const BOT_TINT = { body: 'oklch(0.74 0.17 55)', platter: 'oklch(0.36 0.02 55)' };
const NET_TINT = 'oklch(0.74 0.14 215)';

/** The platter the robot raises under a crate, and a lamp at its corner. */
const BOT_PLATTER = { width: 0.54, base: 0.19, top: 0.24 };
const BOT_LAMP = { width: 0.1, height: 0.04, inset: 0.05 };
const BOT_CLEARANCE = 0.03;
const BOT_NAME = 'nibrunner';

function square({ at, width }: { at: Vec2; width: number }): Rect {
  const half = width * HALF;
  return { x0: at.x - half, x1: at.x + half, z0: at.z - half, z1: at.z + half };
}

function botSolids({
  pose,
  load,
  snapshot,
}: {
  pose: Pose;
  load: App | null;
  snapshot: Snapshot;
}): Solid[] {
  const body = square({ at: pose, width: BOT.width });
  const solids = [
    solid({
      key: 'bot',
      rect: body,
      y0: BOT_CLEARANCE,
      y1: BOT.height,
      tint: BOT_TINT.body,
      label: BOT_NAME,
      labelFace: 'pz',
    }),
    solid({
      key: 'bot-platter',
      rect: square({ at: pose, width: BOT_PLATTER.width }),
      y0: BOT_PLATTER.base,
      y1: BOT_PLATTER.top + pose.lift,
      tint: BOT_TINT.platter,
    }),
    solid({
      key: 'bot-lamp',
      rect: {
        x0: body.x1 - BOT_LAMP.inset - BOT_LAMP.width,
        x1: body.x1 - BOT_LAMP.inset,
        z0: body.z1 - BOT_LAMP.inset - BOT_LAMP.width,
        z1: body.z1 - BOT_LAMP.inset,
      },
      y0: BOT.height,
      y1: BOT.height + BOT_LAMP.height,
      tint: NET_TINT,
    }),
  ];
  if (load !== null) {
    solids.push(
      ...crateSolids({
        app: load,
        at: pose,
        y0: pose.lift,
        fade: 0,
        opacity: 1,
        dims: dimsOf({ app: load, now: snapshot.now, pace: snapshot.pace }),
        glow: 0,
      }),
    );
  }
  return solids;
}

const TOUCHING = 1e-6;

/**
 * Whether nothing of `a` can hide any of `b`: the view looks down the (1, 1, 1) diagonal, so a
 * box wholly on the far side of another along any one axis is wholly behind it.
 */
function behind({ a, b }: { a: Aabb; b: Aabb }): boolean {
  return a.x1 <= b.x0 + TOUCHING || a.y1 <= b.y0 + TOUCHING || a.z1 <= b.z0 + TOUCHING;
}

function depthOf({ x0, x1, y0, y1, z0, z1 }: Aabb): number {
  return x0 + x1 + y0 + y1 + z0 + z1;
}

function isDrawnBefore({ a, b }: { a: Solid; b: Solid }): boolean | null {
  const aBehind = behind({ a: a.aabb, b: b.aabb });
  const bBehind = behind({ a: b.aabb, b: a.aabb });
  if (aBehind && bBehind) {
    return null;
  }
  if (aBehind || bBehind) {
    return aBehind;
  }
  return depthOf(a.aabb) < depthOf(b.aabb);
}

// Solids overlap in the picture without a total order among them, so this is a dependency
// walk rather than a sort: each solid is drawn after everything that must be under it.
function paintersOrder(solids: Solid[]): Solid[] {
  const under: number[][] = solids.map(() => []);
  for (const [i, a] of solids.entries()) {
    for (let j = i + 1; j < solids.length; j += 1) {
      const first = isDrawnBefore({ a, b: solids[j]! });
      if (first === true) {
        under[j]!.push(i);
      } else if (first === false) {
        under[i]!.push(j);
      }
    }
  }
  const drawn = new Set<number>();
  const ordered: Solid[] = [];
  function visit(index: number): void {
    if (drawn.has(index)) {
      return;
    }
    drawn.add(index);
    for (const below of under[index]!) {
      visit(below);
    }
    ordered.push(solids[index]!);
  }
  for (let index = 0; index < solids.length; index += 1) {
    visit(index);
  }
  return ordered;
}

const TOP_MIX = 'white 22%';
const LEFT_MIX = 'black 20%';
const EDGE_MIX = 'black 32%';
const PERCENT = 100;

function mixed({ tint, mix }: { tint: string; mix: string | null }): string {
  return mix === null ? tint : `color-mix(in oklch, ${tint}, ${mix})`;
}

/** Washed toward the page's own background, so it fades the same way in either theme. */
function faded({ colour, fade }: { colour: string; fade: number }): string {
  return fade === 0
    ? colour
    : `color-mix(in oklch, ${colour}, var(--color-fd-background) ${(fade * PERCENT).toFixed(0)}%)`;
}

type Face = { key: string; points: string; fill: string };

/** The faces this view sees of a box: its top, the side toward +x, the side toward +z. */
function facesOf(one: Solid): Face[] {
  const { x0, x1, z0, z1 } = one.rect;
  const { y0, y1 } = one;
  const face = ({ key, mix, vertices }: { key: string; mix: string | null; vertices: Vec3[] }) => ({
    key,
    points: polygonPoints(vertices),
    fill: faded({ colour: mixed({ tint: one.tint, mix }), fade: one.fade }),
  });
  return [
    face({
      key: 'right',
      mix: null,
      vertices: [
        { x: x1, y: y0, z: z0 },
        { x: x1, y: y1, z: z0 },
        { x: x1, y: y1, z: z1 },
        { x: x1, y: y0, z: z1 },
      ],
    }),
    face({
      key: 'left',
      mix: LEFT_MIX,
      vertices: [
        { x: x0, y: y0, z: z1 },
        { x: x0, y: y1, z: z1 },
        { x: x1, y: y1, z: z1 },
        { x: x1, y: y0, z: z1 },
      ],
    }),
    face({
      key: 'top',
      mix: TOP_MIX,
      vertices: [
        { x: x0, y: y1, z: z0 },
        { x: x1, y: y1, z: z0 },
        { x: x1, y: y1, z: z1 },
        { x: x0, y: y1, z: z1 },
      ],
    }),
  ];
}

const LABEL = { fontSize: 0.19, mix: 'black 58%', margin: 0.08 };
/** About what a monospace glyph is wide, in ems. */
const GLYPH_EM = 0.62;

/** The largest size the name fits a face of this width at, up to the usual one. */
function labelSize({ label, width }: { label: string; width: number }): number {
  return Math.min(LABEL.fontSize, (width - LABEL.margin) / (label.length * GLYPH_EM));
}

const LINE_HEIGHT = 1.1;

/** A hyphenated name that would shrink to fit breaks at its hyphen instead, if that reads larger. */
function labelLines({ label, width }: { label: string; width: number }): {
  lines: string[];
  size: number;
} {
  const whole = { lines: [label], size: labelSize({ label, width }) };
  const hyphen = label.lastIndexOf('-');
  if (hyphen < 1 || whole.size >= LABEL.fontSize) {
    return whole;
  }
  const lines = [label.slice(0, hyphen + 1), label.slice(hyphen + 1)];
  const size = Math.min(...lines.map((line) => labelSize({ label: line, width })));
  return size > whole.size ? { lines, size } : whole;
}

/**
 * A name on a crate. A face is a parallelogram on screen, so the text is drawn in the face's own
 * plane: one unit along the edge it reads along, one unit down.
 */
function FaceLabel({ one }: { one: Solid }) {
  const { x0, x1, z0, z1 } = one.rect;
  const cos = EDGE_COS * UNIT_PX;
  const sin = EDGE_SIN * UNIT_PX;
  const top = one.labelFace === 'top';
  const front = one.labelFace === 'pz';
  const anchor = top
    ? project({ x: x0, y: one.y1, z: z0 })
    : project({ x: front ? x0 : x1, y: one.y1, z: z1 });
  const width = top || front ? x1 - x0 : z1 - z0;
  const height = top ? z1 - z0 : one.y1 - one.y0;
  const transform = top
    ? `matrix(${cos} ${sin} ${-cos} ${sin} ${anchor.px} ${anchor.py})`
    : `matrix(${cos} ${sin * (front ? 1 : -1)} 0 ${UNIT_PX} ${anchor.px} ${anchor.py})`;
  const { lines, size } = labelLines({ label: one.label ?? '', width });
  return (
    <text
      transform={transform}
      fontSize={size}
      textAnchor="middle"
      dominantBaseline="central"
      stroke="none"
      style={{ fill: faded({ colour: mixed({ tint: one.tint, mix: LABEL.mix }), fade: one.fade }) }}
      className="pointer-events-none select-none font-mono"
    >
      {Array.from(lines.entries(), ([row, line]) => (
        <tspan
          key={line}
          x={width * HALF}
          y={height * HALF + (row - (lines.length - 1) * HALF) * LINE_HEIGHT * size}
          dominantBaseline="central"
        >
          {line}
        </tspan>
      ))}
    </text>
  );
}

const EDGE_STROKE = 0.7;
const GLOW = { lift: 0.05, width: 1.6 };

/** The lit strip along the two visible edges of a crate's base. */
function glowPoints(one: Solid): string {
  const { x0, x1, z0, z1 } = one.rect;
  const y = one.y0 + GLOW.lift;
  return [
    { x: x0, y, z: z1 },
    { x: x1, y, z: z1 },
    { x: x1, y, z: z0 },
  ]
    .map((vertex) => {
      const { px, py } = project(vertex);
      return `${px.toFixed(2)},${py.toFixed(2)}`;
    })
    .join(' ');
}

function SolidShape({
  one,
  onVisit,
  onHighlight,
}: {
  one: Solid;
  onVisit: (app: number) => void;
  onHighlight: (app: number | null) => void;
}) {
  const app = one.app;
  return (
    // biome-ignore lint/a11y/noStaticElementInteractions: the picture is aria-hidden; the panel beside it carries the same actions as buttons, and this is the pointer shortcut
    <g
      opacity={one.opacity}
      style={{
        stroke: faded({ colour: mixed({ tint: one.tint, mix: EDGE_MIX }), fade: one.fade }),
      }}
      strokeWidth={EDGE_STROKE}
      strokeLinejoin="round"
      className={app === null ? undefined : 'cursor-pointer'}
      onClick={app === null ? undefined : () => onVisit(app)}
      onMouseEnter={app === null ? undefined : () => onHighlight(app)}
      onMouseLeave={app === null ? undefined : () => onHighlight(null)}
    >
      {facesOf(one).map((face) => (
        <polygon key={face.key} points={face.points} style={{ fill: face.fill }} />
      ))}
      {one.glow > 0 && (
        <polyline
          points={glowPoints(one)}
          fill="none"
          style={{ stroke: NET_TINT }}
          strokeWidth={GLOW.width}
          strokeOpacity={one.glow}
          strokeLinecap="round"
          strokeLinejoin="round"
        />
      )}
      {one.label !== null && <FaceLabel one={one} />}
    </g>
  );
}

// A crate declared in the document rises out of the pad; one struck out drops off the front edge.
const DEPARTURE = { seconds: 0.7, depth: 2.4 };

function progress({
  since,
  now,
  ms,
  pace,
}: {
  since: number;
  now: number;
  ms: number;
  pace: number;
}): number {
  const duration = ms * pace;
  return duration === 0 ? 1 : Math.min(1, Math.max(0, (now - since) / duration));
}

const ASLEEP_FADE = 0.62;
const DIMMED_FADE = 0.72;

/** How far an app is washed out: the ones not pointed at, and the ones asleep. */
function fadeOf({ app, highlighted }: { app: App; highlighted: number | null }): number {
  if (highlighted !== null && highlighted !== app.id) {
    return DIMMED_FADE;
  }
  return app.asleep ? ASLEEP_FADE : 0;
}

/** Where an app that is not on the robot stands right now, and how plainly it shows. */
function standingOf({
  app,
  snapshot,
  highlighted,
}: {
  app: App;
  snapshot: Snapshot;
  highlighted: number | null;
}): Standing | null {
  const { now, pace } = snapshot;
  const fade = fadeOf({ app, highlighted });
  const dims = dimsOf({ app, now, pace });
  switch (app.location.kind) {
    case 'placed': {
      const onFloor = app.location.spot.zone === 'floor' && !app.asleep;
      return {
        app,
        at: centreOf(spotRect({ spot: app.location.spot, size: app.size })),
        y0: 0,
        fade,
        opacity: 1,
        dims,
        glow: onFloor ? freshnessOf({ app, now }) : 0,
      };
    }
    case 'pad':
      return {
        app,
        at: PAD,
        y0: 0,
        fade,
        opacity: progress({ since: app.since, now, ms: ARRIVAL_MS, pace }),
        dims,
        glow: 0,
      };
    case 'gone': {
      const t = progress({ since: app.since, now, ms: DEPARTURE.seconds * MS_PER_SECOND, pace });
      return {
        app,
        at: EXIT,
        y0: -(t * t) * DEPARTURE.depth,
        fade: 0,
        opacity: 1 - t,
        dims,
        glow: 0,
      };
    }
    default:
      return null;
  }
}

const SNORE = { rise: 0.3, fontSize: 0.26 };

function Snore({ app, at }: { app: App; at: Vec2 }) {
  const { px, py } = project({
    x: at.x,
    y: dimsFor({ form: app.form, size: app.size }).height + SNORE.rise,
    z: at.z,
  });
  return (
    <text
      x={px}
      y={py}
      fontSize={SNORE.fontSize * UNIT_PX}
      textAnchor="middle"
      className="pointer-events-none animate-pulse select-none fill-fd-muted-foreground font-mono"
    >
      z z
    </text>
  );
}

const FLOOR_LABEL = { fontSize: 0.24, gap: 0.2 };

/** Written on the floor just in front of a rectangle, in the floor's own plane. */
function FloorLabel({ rect, text }: { rect: Rect; text: string }) {
  const anchor = project({ x: rect.x0, y: 0, z: rect.z1 + FLOOR_LABEL.gap });
  const cos = EDGE_COS * UNIT_PX;
  const sin = EDGE_SIN * UNIT_PX;
  return (
    <text
      transform={`matrix(${cos} ${sin} ${-cos} ${sin} ${anchor.px} ${anchor.py})`}
      x={(rect.x1 - rect.x0) * HALF}
      y={FLOOR_LABEL.fontSize}
      fontSize={FLOOR_LABEL.fontSize}
      textAnchor="middle"
      className="pointer-events-none select-none fill-fd-muted-foreground font-mono"
    >
      {text}
    </text>
  );
}

const WALL_LABEL = { fontSize: 0.22, above: 0.08 };

/** Written on the back wall, above the lattice. */
function WallLabel({ text }: { text: string }) {
  const anchor = project({
    x: ZONES.floor.x0,
    y: NET_PANEL.y1 + WALL_LABEL.above + WALL_LABEL.fontSize,
    z: 0,
  });
  return (
    <text
      transform={`matrix(${EDGE_COS * UNIT_PX} ${EDGE_SIN * UNIT_PX} 0 ${UNIT_PX} ${anchor.px} ${anchor.py})`}
      x={0}
      y={0}
      fontSize={WALL_LABEL.fontSize}
      className="pointer-events-none select-none fill-fd-muted-foreground font-mono"
    >
      {text}
    </text>
  );
}

const LATTICE_STEP = 0.5;
const LATTICE_STROKE = 0.6;

/** The internet, on the back wall: a lattice with traffic running along it, always. */
function InternetPanel() {
  const { x0, x1 } = ZONES.floor;
  const { y0, y1 } = NET_PANEL;
  const verticals: number[] = [];
  for (let x = x0; x <= x1 + TOUCHING; x += LATTICE_STEP) {
    verticals.push(x);
  }
  const horizontals: number[] = [];
  for (let y = y0; y <= y1 + TOUCHING; y += LATTICE_STEP) {
    horizontals.push(y);
  }
  return (
    <g style={{ stroke: NET_TINT }} strokeWidth={LATTICE_STROKE}>
      <polygon
        points={polygonPoints([
          { x: x0, y: y0, z: 0 },
          { x: x1, y: y0, z: 0 },
          { x: x1, y: y1, z: 0 },
          { x: x0, y: y1, z: 0 },
        ])}
        style={{ fill: NET_TINT }}
        fillOpacity={0.08}
        strokeOpacity={0.5}
      />
      <g strokeOpacity={0.3}>
        {verticals.map((x) => {
          const from = project({ x, y: y0, z: 0 });
          const to = project({ x, y: y1, z: 0 });
          return <line key={x} x1={from.px} y1={from.py} x2={to.px} y2={to.py} />;
        })}
      </g>
      <g strokeOpacity={0.55} strokeDasharray="3 7" className="flow">
        {horizontals.map((y) => {
          const from = project({ x: x0, y, z: 0 });
          const to = project({ x: x1, y, z: 0 });
          return <line key={y} x1={from.px} y1={from.py} x2={to.px} y2={to.py} />;
        })}
      </g>
    </g>
  );
}

const CABLE = { width: 1.5, restOpacity: 0.5, trafficOpacity: 0.95, dash: '4 6' };
const TAP = { width: 0.16, height: 0.1 };

/** Cables run up from the socket to a rail over the floor, along it, and drop onto the crate. */
const RAIL_Y = 1.5;

/** How long the drop takes to reach a crate that has just landed. */
const PLUG_MS = 220;

/**
 * The cable from the wall to a crate: up to the rail, along it to above the crate, and down —
 * as far down as it has got, for one still plugging in.
 */
function cablePoints({ app, at, plug }: { app: App; at: Vec2; plug: number }): Vec3[] {
  const top = dimsFor({ form: 'crate', size: app.size }).height + LID.height;
  return [
    { x: at.x, y: NET_PANEL.tap, z: 0 },
    { x: at.x, y: RAIL_Y, z: 0 },
    { x: at.x, y: RAIL_Y, z: at.z },
    { x: at.x, y: RAIL_Y - (RAIL_Y - top) * plug, z: at.z },
  ];
}

function polylinePoints(points: Vec3[]): string {
  return points
    .map((vertex) => {
      const { px, py } = project(vertex);
      return `${px.toFixed(2)},${py.toFixed(2)}`;
    })
    .join(' ');
}

function distance({ a, b }: { a: Vec3; b: Vec3 }): number {
  return Math.hypot(b.x - a.x, b.y - a.y, b.z - a.z);
}

/** The point `t` of the way along a polyline, by length rather than by corner. */
function pointAlong({ points, t }: { points: Vec3[]; t: number }): Vec3 {
  const legs: number[] = [];
  let total = 0;
  for (let index = 1; index < points.length; index += 1) {
    legs.push(distance({ a: points[index - 1]!, b: points[index]! }));
    total += legs[index - 1]!;
  }
  let left = Math.min(1, Math.max(0, t)) * total;
  for (const [index, leg] of legs.entries()) {
    const a = points[index]!;
    const b = points[index + 1]!;
    if (left <= leg || index === legs.length - 1) {
      const share = leg === 0 ? 1 : Math.min(1, left / leg);
      return {
        x: a.x + (b.x - a.x) * share,
        y: a.y + (b.y - a.y) * share,
        z: a.z + (b.z - a.z) * share,
      };
    }
    left -= leg;
  }
  return points[points.length - 1]!;
}

/** The wall socket a cable leaves from. */
function tapPoints(x: number): string {
  return polygonPoints([
    { x: x - TAP.width * HALF, y: NET_PANEL.tap - TAP.height * HALF, z: 0 },
    { x: x + TAP.width * HALF, y: NET_PANEL.tap - TAP.height * HALF, z: 0 },
    { x: x + TAP.width * HALF, y: NET_PANEL.tap + TAP.height * HALF, z: 0 },
    { x: x - TAP.width * HALF, y: NET_PANEL.tap + TAP.height * HALF, z: 0 },
  ]);
}

/** Traffic thins out as an app goes unvisited, until there is none and it is put to sleep. */
function freshnessOf({ app, now }: { app: App; now: number }): number {
  return 1 - Math.min(1, Math.max(0, (now - app.visitedAt) / SLEEP_AFTER_MS));
}

function plugOf({ app, now, pace }: { app: App; now: number; pace: number }): number {
  return progress({ since: app.since, now, ms: PLUG_MS, pace });
}

/** The cable itself, and the traffic running along it once it is plugged in. */
function Cable({ app, at, snapshot }: { app: App; at: Vec2; snapshot: Snapshot }) {
  const { now, pace } = snapshot;
  const plug = plugOf({ app, now, pace });
  const points = polylinePoints(cablePoints({ app, at, plug }));
  return (
    <g style={{ stroke: NET_TINT, fill: NET_TINT }} strokeLinecap="round" strokeLinejoin="round">
      <polygon points={tapPoints(at.x)} stroke="none" />
      <polyline
        points={points}
        fill="none"
        strokeWidth={CABLE.width}
        strokeOpacity={CABLE.restOpacity}
      />
      {plug >= 1 && (
        <polyline
          points={points}
          fill="none"
          strokeWidth={CABLE.width}
          strokeDasharray={CABLE.dash}
          strokeOpacity={CABLE.trafficOpacity * freshnessOf({ app, now })}
          className="flow"
        />
      )}
    </g>
  );
}

const PULSE = { radius: 3.4, halo: 8, haloOpacity: 0.3 };

/** A request on its way down the cable. */
function Pulse({ app, at, now }: { app: App; at: Vec2; now: number }) {
  const t = (now - app.visitedAt) / PULSE_MS;
  const { px, py } = project(pointAlong({ points: cablePoints({ app, at, plug: 1 }), t }));
  return (
    <g style={{ fill: NET_TINT }}>
      <circle cx={px} cy={py} r={PULSE.halo} fillOpacity={PULSE.haloOpacity} />
      <circle cx={px} cy={py} r={PULSE.radius} />
    </g>
  );
}

const BUBBLE = { rise: 1.1, padX: 8, height: 22, fontSize: 11, radius: 6, tail: 5, fadeShare: 0.2 };
/** About what a monospace glyph is wide at the bubble's size, in pixels. */
const BUBBLE_GLYPH_PX = 6.7;

/** What the robot has to say, over its head, for a moment. */
function Speech({ notice, pose, snapshot }: { notice: Notice; pose: Pose; snapshot: Snapshot }) {
  const { now, pace } = snapshot;
  const t = progress({ since: notice.at, now, ms: NOTICE_MS, pace });
  if (t >= 1) {
    return null;
  }
  const opacity = Math.min(1, (1 - t) / BUBBLE.fadeShare);
  const { px, py } = project({ x: pose.x, y: BUBBLE.rise, z: pose.z });
  const width = notice.text.length * BUBBLE_GLYPH_PX + BUBBLE.padX * 2;
  const left = px - width * HALF;
  const top = py - BUBBLE.height;
  return (
    <g opacity={opacity} className="pointer-events-none">
      <rect
        x={left}
        y={top}
        width={width}
        height={BUBBLE.height}
        rx={BUBBLE.radius}
        className="fill-fd-popover stroke-fd-border"
      />
      <polygon
        points={`${px - BUBBLE.tail},${py} ${px + BUBBLE.tail},${py} ${px},${py + BUBBLE.tail}`}
        className="fill-fd-popover"
      />
      <text
        x={px}
        y={top + BUBBLE.height * HALF}
        fontSize={BUBBLE.fontSize}
        textAnchor="middle"
        dominantBaseline="central"
        className="fill-fd-popover-foreground font-mono"
      >
        {notice.text}
      </text>
    </g>
  );
}

const GRID_STROKE = 0.5;
const PAD_STROKE = 0.8;
const PAD_DASH = '3 3';
const ZONE_FILL_OPACITY = 0.06;
const RESERVED_FILL = { waiting: 0.18, coming: 0.08 };

/**
 * Cells kept for a crate on its way, lit until it lands; when a request is what sent for it,
 * the socket it will be cabled to is lit as well, waiting.
 */
function Reservation({ app }: { app: App }) {
  const spot = app.reserved!;
  const rect = spotRect({ spot, size: app.size });
  const waiting = app.requestedAt !== null;
  return (
    <g style={{ stroke: NET_TINT, fill: NET_TINT }} className="animate-pulse">
      <polygon
        points={polygonPoints(floorQuad(rect))}
        fillOpacity={waiting ? RESERVED_FILL.waiting : RESERVED_FILL.coming}
        strokeWidth={PAD_STROKE}
        strokeDasharray={PAD_DASH}
      />
      {waiting && spot.zone === 'floor' && (
        <polygon points={tapPoints(centreOf(rect).x)} stroke="none" />
      )}
    </g>
  );
}

export function Warehouse({
  snapshot,
  highlighted,
  onVisit,
  onHighlight,
}: {
  snapshot: Snapshot;
  highlighted: number | null;
  onVisit: (app: number) => void;
  onHighlight: (app: number | null) => void;
}) {
  const { apps, pose, now } = snapshot;
  const standing = apps.flatMap((app) => {
    const place = standingOf({ app, snapshot, highlighted });
    return place === null ? [] : [place];
  });
  const load = apps.find((app) => app.id === pose.load) ?? null;
  const solids = paintersOrder([
    ...standing.flatMap((place) => crateSolids(place)),
    ...botSolids({ pose, load, snapshot }),
  ]);
  const onFloor = standing.filter(
    (place) =>
      place.app.location.kind === 'placed' &&
      place.app.location.spot.zone === 'floor' &&
      !place.app.asleep,
  );
  const asleep = standing.filter(
    (place) => place.app.asleep && place.app.location.kind === 'placed',
  );
  const reserved = apps.filter((app) => app.reserved !== null);
  const padCells = cellsAround({ at: PAD, dims: SIZES.L });

  return (
    <svg
      viewBox={VIEW_BOX}
      aria-hidden="true"
      className="h-auto max-h-full w-full select-none"
      preserveAspectRatio="xMidYMid meet"
    >
      <g className="fill-fd-accent/50">
        {SURFACES.map((surface) => (
          <polygon key={polygonPoints(surface)} points={polygonPoints(surface)} />
        ))}
      </g>
      <g className="stroke-fd-muted-foreground/20" strokeWidth={GRID_STROKE}>
        {GRID.map((segment) => {
          const from = project(segment.from);
          const to = project(segment.to);
          return <line key={segmentKey(segment)} x1={from.px} y1={from.py} x2={to.px} y2={to.py} />;
        })}
      </g>
      <InternetPanel />
      <WallLabel text="internet" />
      <polygon
        points={polygonPoints(floorQuad(ZONES.floor))}
        style={{ fill: NET_TINT }}
        fillOpacity={ZONE_FILL_OPACITY}
      />
      <g
        fill="none"
        className="stroke-fd-muted-foreground/50"
        strokeWidth={PAD_STROKE}
        strokeDasharray={PAD_DASH}
      >
        <polygon points={polygonPoints(floorQuad(ZONES.shelf))} />
        <polygon points={polygonPoints(floorQuad(padCells))} />
      </g>
      <FloorLabel rect={ZONES.shelf} text="snapshots" />
      <FloorLabel rect={ZONES.floor} text="/dev/kvm" />
      <FloorLabel rect={{ x0: 0, x1: ROOM.x, z0: 0, z1: ROOM.z }} text="your VPS" />
      <FloorLabel rect={padCells} text="desired" />
      {reserved.map((app) => (
        <Reservation key={app.id} app={app} />
      ))}
      <polygon
        points={polygonPoints(floorQuad(square({ at: pose, width: BOT.width })))}
        className="fill-fd-foreground/10"
      />
      {solids.map((one) => (
        <SolidShape key={one.key} one={one} onVisit={onVisit} onHighlight={onHighlight} />
      ))}
      {onFloor.map((place) => (
        <Cable key={place.app.id} app={place.app} at={place.at} snapshot={snapshot} />
      ))}
      {onFloor
        .filter((place) => now - place.app.visitedAt < PULSE_MS)
        .map((place) => (
          <Pulse key={place.app.id} app={place.app} at={place.at} now={now} />
        ))}
      {asleep.map((place) => (
        <Snore key={place.app.id} app={place.app} at={place.at} />
      ))}
      {snapshot.notice !== null && (
        <Speech notice={snapshot.notice} pose={pose} snapshot={snapshot} />
      )}
    </svg>
  );
}
