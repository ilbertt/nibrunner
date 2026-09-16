import { useEffect, useState, useSyncExternalStore } from 'react';
import {
  type App,
  BOT,
  cellsAround,
  centreOf,
  crateRect,
  createApp,
  dimsFor,
  EXIT,
  type Form,
  freeSpot,
  type Location,
  MAX_APPS,
  PAD,
  PARK,
  planRoute,
  SIZES,
  type Size,
  SLEEP_AFTER_MS,
  type Spot,
  spotRect,
  type Vec2,
  type ZoneName,
} from './host';

export type Pose = { x: number; z: number; lift: number; load: number | null };

type Job =
  | { kind: 'place' | 'shelve' | 'wake' | 'remove'; app: number }
  | { kind: 'resize'; app: number; size: Size };

type Step =
  | { kind: 'move'; path: Vec2[]; parking?: boolean }
  | { kind: 'lift'; height: number }
  | { kind: 'until'; at: number }
  | { kind: 'wait'; seconds: number }
  /** Lifting is when a crate takes its new size, or is snapshotted into a slab, or restored. */
  | { kind: 'grab'; app: number; size: Size; form: Form }
  | { kind: 'release'; app: number; to: Location }
  | { kind: 'forget'; app: number };

type Running = { steps: Step[]; index: number; startedAt: number; from: Pose };

type World = {
  apps: App[];
  jobs: Job[];
  pose: Pose;
  running: Running | null;
  nextId: number;
};

/** `pace` is 1, or 0 for a reader who asked for less motion: every duration is multiplied by it. */
export type Snapshot = { apps: App[]; pose: Pose; now: number; pace: number };

const MS_PER_SECOND = 1000;

const SHORTEST_MOVE_SECONDS = 0.08;
const LIFT_SECONDS = 0.08;
const FAREWELL_SECONDS = 0.7;

/** How long a crate takes to rise out of the pad; the robot may not lift it sooner. */
export const ARRIVAL_MS = 380;

/** How long a request is seen travelling down the cable. */
export const PULSE_MS = 420;

const SLEEP_TICK_MS = 1000;

const REDUCED_MOTION_QUERY = '(prefers-reduced-motion: reduce)';

/** Two apps running and one asleep, so the page opens on every state at once. */
function startingWorld(): World {
  const apps: App[] = [];
  const opening: { spot: Spot; asleep: boolean; form: Form }[] = [
    { spot: { zone: 'floor', x0: 4.5, z0: 0.5 }, asleep: false, form: 'crate' },
    { spot: { zone: 'floor', x0: 5.5, z0: 0.5 }, asleep: false, form: 'crate' },
    { spot: { zone: 'shelf', x0: 0.5, z0: 0.5 }, asleep: true, form: 'slab' },
  ];
  for (const [id, one] of opening.entries()) {
    const app = createApp({ id, taken: apps.map((taken) => taken.name), at: 0 });
    apps.push({
      ...app,
      location: { kind: 'placed', spot: one.spot },
      asleep: one.asleep,
      form: one.form,
    });
  }
  return {
    apps,
    jobs: [],
    pose: { x: PARK.x, z: PARK.z, lift: 0, load: null },
    running: null,
    nextId: apps.length,
  };
}

function lengthOf({ from, path }: { from: Vec2; path: Vec2[] }): number {
  let length = 0;
  let at = from;
  for (const point of path) {
    length += Math.hypot(point.x - at.x, point.z - at.z);
    at = point;
  }
  return length;
}

function secondsOf({
  step,
  from,
  startedAt,
}: {
  step: Step;
  from: Pose;
  startedAt: number;
}): number {
  switch (step.kind) {
    case 'move':
      return Math.max(SHORTEST_MOVE_SECONDS, lengthOf({ from, path: step.path }) / BOT.speed);
    case 'lift':
      return LIFT_SECONDS;
    case 'until':
      return Math.max(0, step.at - startedAt) / MS_PER_SECOND;
    case 'wait':
      return step.seconds;
    default:
      return 0;
  }
}

function easeInOut(t: number): number {
  return t < 1 / 2 ? 2 * t * t : 1 - (2 - 2 * t) ** 2 / 2;
}

const CUBIC = 3;

/** Off the mark at once and braking into the stop: how a machine on a schedule drives. */
function easeOut(t: number): number {
  return 1 - (1 - t) ** CUBIC;
}

/** The point `along` units into the polyline that starts at `from`. */
function pointAlong({ from, path, along }: { from: Vec2; path: Vec2[]; along: number }): Vec2 {
  let left = along;
  let at = from;
  for (const point of path) {
    const leg = Math.hypot(point.x - at.x, point.z - at.z);
    if (left <= leg && leg > 0) {
      const t = left / leg;
      return { x: at.x + (point.x - at.x) * t, z: at.z + (point.z - at.z) * t };
    }
    left -= leg;
    at = point;
  }
  return at;
}

function poseAt({ from, step, t }: { from: Pose; step: Step; t: number }): Pose {
  switch (step.kind) {
    case 'move': {
      const along = lengthOf({ from, path: step.path }) * easeOut(t);
      return { ...from, ...pointAlong({ from, path: step.path, along }) };
    }
    case 'lift':
      return { ...from, lift: from.lift + (step.height - from.lift) * easeInOut(t) };
    default:
      return from;
  }
}

function update({ world, id, change }: { world: World; id: number; change: Partial<App> }): void {
  world.apps = world.apps.map((one) => (one.id === id ? { ...one, ...change } : one));
}

function appOf({ world, id }: { world: World; id: number }): App | undefined {
  return world.apps.find((one) => one.id === id);
}

/** Where an app's crate stands right now, for the robot to slide under. */
function standingAt(app: App): Vec2 | null {
  if (app.location.kind === 'placed') {
    return centreOf(spotRect({ spot: app.location.spot, size: app.size }));
  }
  return app.location.kind === 'pad' ? PAD : null;
}

/** Every crate on the floor but this one: what a loaded robot has to keep clear of. */
function cratesBeside({ world, id }: { world: World; id: number }): ReturnType<typeof crateRect>[] {
  const crates: ReturnType<typeof crateRect>[] = [];
  for (const app of world.apps) {
    const at = app.id === id ? null : standingAt(app);
    if (at !== null) {
      crates.push(
        crateRect(cellsAround({ at, dims: dimsFor({ form: app.form, size: app.size }) })),
      );
    }
  }
  return crates;
}

/** What an app is once set down here: a slab on the shelf, a crate anywhere else. */
function formAt(drop: Location): Form {
  return drop.kind === 'placed' && drop.spot.zone === 'shelf' ? 'slab' : 'crate';
}

/** Slide under the crate, lift it, carry it to `to`, set it down, and go and park. */
function carry({
  world,
  app,
  to,
  drop,
  size = app.size,
}: {
  world: World;
  app: App;
  to: Vec2;
  drop: Location;
  size?: Size;
}): Step[] {
  const from = standingAt(app);
  if (from === null) {
    return [];
  }
  const form = formAt(drop);
  return [
    { kind: 'move', path: [from] },
    { kind: 'lift', height: BOT.lift },
    { kind: 'grab', app: app.id, size, form },
    {
      kind: 'move',
      path: planRoute({
        from,
        to,
        load: dimsFor({ form, size }),
        crates: cratesBeside({ world, id: app.id }),
      }),
    },
    { kind: 'lift', height: 0 },
    { kind: 'release', app: app.id, to: drop },
    { kind: 'move', path: [PARK], parking: true },
  ];
}

function reserve({
  world,
  app,
  zone,
  size = app.size,
}: {
  world: World;
  app: App;
  zone: ZoneName;
  size?: Size;
}): Spot | null {
  const spot = freeSpot({ zone, size, apps: world.apps, except: app.id });
  if (spot !== null) {
    update({ world, id: app.id, change: { reserved: spot } });
  }
  return spot;
}

function planPlace({ world, app, now }: { world: World; app: App; now: number }): Step[] {
  const spot = reserve({ world, app, zone: 'floor' });
  if (spot === null) {
    update({ world, id: app.id, change: { pending: null } });
    return [];
  }
  update({
    world,
    id: app.id,
    change: {
      location: { kind: 'pad' },
      since: now,
      morph: { from: { ...SIZES[app.size], height: 0 }, at: now },
    },
  });
  const arrived = appOf({ world, id: app.id })!;
  return [
    { kind: 'until', at: now + ARRIVAL_MS },
    ...carry({
      world,
      app: arrived,
      to: centreOf(spotRect({ spot, size: app.size })),
      drop: { kind: 'placed', spot },
    }),
  ];
}

// A shelf with no room left is not a reason to keep an idle app running: it sleeps where it is.
function planShelve({ world, app, now }: { world: World; app: App; now: number }): Step[] {
  if (now - app.visitedAt < SLEEP_AFTER_MS) {
    update({ world, id: app.id, change: { pending: null } });
    return [];
  }
  const spot = reserve({ world, app, zone: 'shelf' });
  if (spot === null) {
    update({ world, id: app.id, change: { pending: null, asleep: true } });
    return [];
  }
  return carry({
    world,
    app,
    to: centreOf(spotRect({ spot, size: app.size })),
    drop: { kind: 'placed', spot },
  });
}

function planWake({ world, app }: { world: World; app: App }): Step[] {
  const spot = reserve({ world, app, zone: 'floor' });
  if (spot === null) {
    update({ world, id: app.id, change: { pending: null, requestedAt: null } });
    return [];
  }
  return carry({
    world,
    app,
    to: centreOf(spotRect({ spot, size: app.size })),
    drop: { kind: 'placed', spot },
  });
}

// A crate of another size wants cells of its own: it is lifted, takes its new size, and is set
// down where that size fits. A slab on the shelf is the same slab at any size, so it only takes
// note. One that would fit nowhere stays as it is.
function planResize({ world, app, size }: { world: World; app: App; size: Size }): Step[] {
  if (app.location.kind !== 'placed' || app.size === size) {
    update({ world, id: app.id, change: { pending: null } });
    return [];
  }
  if (app.location.spot.zone === 'shelf') {
    update({ world, id: app.id, change: { pending: null, size } });
    return [];
  }
  const spot = reserve({ world, app, zone: app.location.spot.zone, size });
  if (spot === null) {
    update({ world, id: app.id, change: { pending: null } });
    return [];
  }
  return carry({
    world,
    app,
    to: centreOf(spotRect({ spot, size })),
    drop: { kind: 'placed', spot },
    size,
  });
}

function planRemove({ world, app }: { world: World; app: App }): Step[] {
  const steps = carry({ world, app, to: EXIT, drop: { kind: 'gone' } });
  const parking = steps.pop();
  return parking === undefined
    ? []
    : [
        ...steps,
        { kind: 'wait', seconds: FAREWELL_SECONDS },
        { kind: 'forget', app: app.id },
        parking,
      ];
}

/** Plans a job against the world as it stands the moment the robot is free for it. */
function plan({ world, job, now }: { world: World; job: Job; now: number }): Step[] {
  const app = appOf({ world, id: job.app });
  if (app === undefined) {
    return [];
  }
  switch (job.kind) {
    case 'place':
      return planPlace({ world, app, now });
    case 'shelve':
      return planShelve({ world, app, now });
    case 'wake':
      return planWake({ world, app });
    case 'resize':
      return planResize({ world, app, size: job.size });
    default:
      return planRemove({ world, app });
  }
}

/** What a crate set down becomes: awake on the floor with a request pulsing in, or asleep. */
function landed({ world, app, to, now }: { world: World; app: App; to: Location; now: number }) {
  const onFloor = to.kind === 'placed' && to.spot.zone === 'floor';
  update({
    world,
    id: app.id,
    change: {
      location: to,
      since: now,
      reserved: null,
      pending: null,
      asleep: !onFloor,
      visitedAt: onFloor ? now : app.visitedAt,
      requestedAt: onFloor ? null : app.requestedAt,
    },
  });
  // A request that came while it was on its way to the shelf brings it straight back.
  if (!onFloor && app.requestedAt !== null) {
    update({ world, id: app.id, change: { pending: 'wake' } });
    world.jobs.unshift({ kind: 'wake', app: app.id });
  }
}

/** The instant steps: what a grab or a release does to the apps once the robot is there. */
function settle({ world, step, now }: { world: World; step: Step; now: number }): void {
  const app = 'app' in step ? appOf({ world, id: step.app }) : undefined;
  if (app === undefined) {
    return;
  }
  switch (step.kind) {
    case 'grab': {
      const was = dimsFor({ form: app.form, size: app.size });
      const becomes = dimsFor({ form: step.form, size: step.size });
      update({
        world,
        id: app.id,
        change: {
          location: { kind: 'bot' },
          since: now,
          size: step.size,
          form: step.form,
          morph: was === becomes ? app.morph : { from: was, at: now },
        },
      });
      world.pose = { ...world.pose, load: app.id };
      return;
    }
    case 'release':
      landed({ world, app, to: step.to, now });
      world.pose = { ...world.pose, load: null };
      return;
    case 'forget':
      world.apps = world.apps.filter((one) => one.id !== app.id);
      return;
    default:
      return;
  }
}

/** Queues what the passing of time asks for: a queued crate to be fetched, an idle app shelved. */
function sweep({ world, now }: { world: World; now: number }): void {
  for (const app of world.apps) {
    if (app.location.kind === 'queued' && app.pending === null) {
      update({ world, id: app.id, change: { pending: 'place' } });
      world.jobs.push({ kind: 'place', app: app.id });
    }
    const onFloor = app.location.kind === 'placed' && app.location.spot.zone === 'floor';
    const idle = now - Math.max(app.visitedAt, app.since) > SLEEP_AFTER_MS;
    if (onFloor && !app.asleep && app.pending === null && idle) {
      update({ world, id: app.id, change: { pending: 'shelve' } });
      world.jobs.push({ kind: 'shelve', app: app.id });
    }
  }
}

function pulsing({ world, now }: { world: World; now: number }): boolean {
  return world.apps.some((app) => now - app.visitedAt < PULSE_MS);
}

/**
 * One tick of the robot. Returns whether anything is still moving; instant steps and steps whose
 * time is up are all consumed in the one tick, so a frame never shows a stale pose.
 */
function advance({ world, now, pace }: { world: World; now: number; pace: number }): boolean {
  sweep({ world, now });
  let guard = 0;
  const mostStepsPerTick = 64;
  while (guard < mostStepsPerTick) {
    guard += 1;
    if (world.running === null) {
      const job = world.jobs.shift();
      if (job === undefined) {
        return pulsing({ world, now });
      }
      world.running = {
        steps: plan({ world, job, now }),
        index: 0,
        startedAt: now,
        from: world.pose,
      };
      continue;
    }
    const { steps, index, startedAt, from } = world.running;
    const step = steps[index];
    // Parking is only for when there is nothing else to do.
    if (step === undefined || (step.kind === 'move' && step.parking && world.jobs.length > 0)) {
      world.running = null;
      continue;
    }
    const duration = secondsOf({ step, from, startedAt }) * MS_PER_SECOND * pace;
    const t = duration === 0 ? 1 : Math.min(1, (now - startedAt) / duration);
    world.pose = poseAt({ from, step, t });
    if (t < 1) {
      return true;
    }
    settle({ world, step, now });
    world.running = { steps, index: index + 1, startedAt: startedAt + duration, from: world.pose };
  }
  return true;
}

type Host = {
  subscribe: (listener: () => void) => () => void;
  getSnapshot: () => Snapshot;
  add: () => void;
  remove: (id: number) => void;
  visit: (id: number) => void;
  resize: (change: { id: number; size: Size }) => void;
  /** Brings the page to life once it is on a screen; returns what puts it back to rest. */
  start: () => () => void;
};

// The world is mutated in place by the runner and by the buttons, and published as snapshots: a
// job planned inside a frame sees every click that came before it, never a render behind.
function createHost(): Host {
  const world = startingWorld();
  const listeners = new Set<() => void>();
  let snapshot: Snapshot = { apps: world.apps, pose: world.pose, now: 0, pace: 1 };
  let frame = 0;
  let pace = 1;

  function publish(now: number): void {
    snapshot = { apps: world.apps, pose: world.pose, now, pace };
    for (const listener of listeners) {
      listener();
    }
  }

  function run(): void {
    if (frame !== 0) {
      return;
    }
    // The frame's own timestamp can run a few milliseconds behind a click's, and a request that
    // is still in the future when the frame reads it is a pulse before its cable starts.
    function tick(): void {
      const now = performance.now();
      const busy = advance({ world, now, pace });
      publish(now);
      frame = busy ? requestAnimationFrame(tick) : 0;
    }
    frame = requestAnimationFrame(tick);
  }

  function add(): void {
    const { apps, nextId } = world;
    if (apps.filter((app) => app.location.kind !== 'gone').length >= MAX_APPS) {
      return;
    }
    const now = performance.now();
    world.apps = [...apps, createApp({ id: nextId, taken: apps.map((app) => app.name), at: now })];
    world.nextId = nextId + 1;
    publish(now);
    run();
  }

  function remove(id: number): void {
    const app = appOf({ world, id });
    if (app === undefined || app.pending !== null || app.location.kind !== 'placed') {
      return;
    }
    update({ world, id, change: { pending: 'remove' } });
    world.jobs.push({ kind: 'remove', app: id });
    publish(performance.now());
    run();
  }

  /** A queued app just changes; one on the floor is fetched and set down where it now fits. */
  function resize({ id, size }: { id: number; size: Size }): void {
    const app = appOf({ world, id });
    if (app === undefined || app.pending !== null || app.size === size) {
      return;
    }
    const now = performance.now();
    if (app.location.kind === 'placed') {
      update({ world, id, change: { pending: 'resize' } });
      world.jobs.push({ kind: 'resize', app: id, size });
    } else {
      update({
        world,
        id,
        change: { size, morph: { from: dimsFor({ form: app.form, size: app.size }), at: now } },
      });
    }
    publish(now);
    run();
  }

  /** A request: served at once by an app on the floor, and the reason an app asleep gets up. */
  function visit(id: number): void {
    const app = appOf({ world, id });
    if (app === undefined || app.pending === 'remove') {
      return;
    }
    const now = performance.now();
    const onFloor = app.location.kind === 'placed' && app.location.spot.zone === 'floor';
    if (onFloor) {
      update({ world, id, change: { visitedAt: now, asleep: false } });
    } else if (app.location.kind === 'placed' && app.pending === null) {
      update({ world, id, change: { requestedAt: now, pending: 'wake' } });
      world.jobs.unshift({ kind: 'wake', app: id });
    } else {
      update({ world, id, change: { requestedAt: now } });
    }
    publish(now);
    run();
  }

  // The page is prerendered with the opening apps in place; once it is alive, the clock that puts
  // them to sleep starts from now. Nothing else happens until the reader does something.
  function start(): () => void {
    const now = performance.now();
    pace = window.matchMedia(REDUCED_MOTION_QUERY).matches ? 0 : 1;
    world.apps = world.apps.map((app) => ({ ...app, since: now, visitedAt: now }));
    publish(now);
    const clock = window.setInterval(() => {
      if (frame === 0) {
        sweep({ world, now: performance.now() });
        if (world.jobs.length > 0) {
          run();
        } else {
          publish(performance.now());
        }
      }
    }, SLEEP_TICK_MS);
    return () => {
      window.clearInterval(clock);
      cancelAnimationFrame(frame);
      frame = 0;
    };
  }

  return {
    subscribe(listener) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    getSnapshot: () => snapshot,
    add,
    remove,
    visit,
    resize,
    start,
  };
}

export function useHost() {
  const [host] = useState(createHost);
  const snapshot = useSyncExternalStore(host.subscribe, host.getSnapshot, host.getSnapshot);
  useEffect(() => host.start(), [host]);
  return {
    snapshot,
    add: host.add,
    remove: host.remove,
    visit: host.visit,
    resize: host.resize,
  };
}
