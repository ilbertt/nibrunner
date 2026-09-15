import tailwindcss from '@tailwindcss/vite';
import { tanstackStart } from '@tanstack/react-start/plugin/vite';
import react from '@vitejs/plugin-react';
import { fumadocsMdx } from 'fumadocs-mdx/vite';
import { nitro } from 'nitro/vite';
import { defineConfig } from 'vite';
import { remarkSchemaReference } from './src/lib/remark-schema-reference.js';

export default defineConfig({
  server: {
    port: 3000,
  },
  plugins: [
    fumadocsMdx({
      globalOptions: {
        mdxOptions: {
          remarkPlugins: (defaults) => [remarkSchemaReference, ...defaults],
        },
      },
    }),
    tailwindcss(),
    tanstackStart(),
    react(),
    nitro({ preset: 'bun', serveStatic: 'inline' }),
  ],
  resolve: {
    tsconfigPaths: true,
    alias: {
      tslib: 'tslib/tslib.es6.js',
    },
  },
});
