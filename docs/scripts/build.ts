import { mkdir, rm } from 'node:fs/promises';
import { join } from 'node:path';

const PROJECT_DIR = join(import.meta.dir, '..');
const DIST_DIR = join(PROJECT_DIR, 'dist');
const BINARY = join(DIST_DIR, 'app');
const SERVER_ENTRY = join(PROJECT_DIR, '.output', 'server', 'index.mjs');
// What nibrun runs. `BUILD_TARGET=host` is one for this machine instead, to try it here.
const buildTarget = process.env.BUILD_TARGET || 'bun-linux-x64';

const vite = Bun.spawn(['bun', 'run', 'build:app'], {
  cwd: PROJECT_DIR,
  stderr: 'inherit',
  stdout: 'inherit',
});

if ((await vite.exited) !== 0) {
  process.exit(1);
}

await rm(DIST_DIR, { force: true, recursive: true });
await mkdir(DIST_DIR, { recursive: true });

// The site is inside: nitro's `serveStatic: 'inline'` put the prerendered pages and assets into
// the server bundle, so the binary is the whole deployment.
const result = await Bun.build({
  entrypoints: [SERVER_ENTRY],
  compile: {
    outfile: BINARY,
    ...(buildTarget === 'host' ? {} : { target: buildTarget as Bun.Build.CompileTarget }),
  },
  format: 'esm',
});

if (!result.success) {
  console.error(...result.logs);
  process.exit(1);
}
