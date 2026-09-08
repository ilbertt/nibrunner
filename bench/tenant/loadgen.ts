import { createHash } from "crypto";

const port = Number(process.env.PORT ?? process.env.NIBRUN_HTTP_PORT ?? 3000);
const holdMib = Number(process.env.HOLD_MIB ?? 0);
const spinPct = Math.min(100, Math.max(0, Number(process.env.SPIN_PCT ?? 0)));

// Touched, not just allocated: an untouched page costs the host nothing, which is the whole
// reason an idle microVM shows 83 MiB whatever it declares.
const held: Buffer[] = [];
for (let i = 0; i < holdMib; i++) {
  const block = Buffer.allocUnsafe(1024 * 1024);
  block.fill(i & 0xff);
  held.push(block);
}

// A fixed unit of work, so time spent is contention rather than a different amount of work.
function rounds(n: number): string {
  let digest = Buffer.from("nibrunner");
  for (let i = 0; i < n; i++) digest = createHash("sha256").update(digest).digest();
  return digest.toString("hex").slice(0, 16);
}

// Duty cycle rather than a flat spin, so a host can be loaded to a chosen fraction.
if (spinPct > 0) {
  const period = 100;
  const busy = (period * spinPct) / 100;
  (function cycle() {
    const until = Date.now() + busy;
    while (Date.now() < until) rounds(64);
    setTimeout(cycle, period - busy);
  })();
}

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });

Bun.serve({
  port,
  hostname: "0.0.0.0",
  fetch(request) {
    const url = new URL(request.url);

    if (url.pathname === "/health") return new Response("ok\n");

    if (url.pathname === "/info") {
      return json({
        holdMib,
        spinPct,
        rssMib: Math.round(process.memoryUsage.rss() / 1048576),
        uptimeS: Math.round(process.uptime()),
      });
    }

    if (url.pathname === "/work") {
      const n = Number(url.searchParams.get("rounds") ?? 20000);
      const started = performance.now();
      const digest = rounds(n);
      return json({ rounds: n, ms: +(performance.now() - started).toFixed(2), digest });
    }

    return json({ error: "not found" }, 404);
  },
});

console.log(`load tenant on ${port}: holding ${holdMib} MiB, spinning ${spinPct}%`);
