import { createShikiFactory } from 'fumadocs-core/highlight/shiki';
import { createCodeUsageGeneratorRegistry } from 'fumadocs-openapi/requests/generators';
import { createOpenAPIPageBase } from 'fumadocs-openapi/ui/base';
import document from '../../../crates/nibrunnerd/filesystem.openapi.json';

/**
 * Only the two languages this page can show. The default factory imports `shiki` whole, which
 * carries a grammar for every language it has ever supported — eight megabytes to highlight one
 * curl command and a JSON body.
 */
const shiki = createShikiFactory({
  async init() {
    const [{ createHighlighterCore }, { createJavaScriptRegexEngine }, bash, json, light, dark] =
      await Promise.all([
        import('shiki/core'),
        import('shiki/engine/javascript'),
        import('@shikijs/langs/bash'),
        import('@shikijs/langs/json'),
        import('@shikijs/themes/github-light'),
        import('@shikijs/themes/github-dark'),
      ]);
    return createHighlighterCore({
      engine: createJavaScriptRegexEngine(),
      langs: [bash.default, json.default],
      themes: [light.default, dark.default],
    });
  },
});

/**
 * Empty, so the only sample shown is the document's own `x-codeSamples`: every built-in generator
 * builds its call from a server URL, and this API is a unix socket with no URL to build one from.
 * The playground is off for the same reason — nothing in a browser can reach the socket.
 */
const OpenAPIPage = createOpenAPIPageBase({
  codeUsages: createCodeUsageGeneratorRegistry(),
  generateTypeScriptDefinitions: false,
  playground: { enabled: false },
  shiki,
});

const OPERATIONS = Object.entries(document.paths).flatMap(([path, methods]) =>
  Object.keys(methods).map((method) => ({ path, method: method as 'get' })),
);

/** The generated document, rendered by fumadocs-openapi rather than by our own remark plugin. */
export function FilesystemApi() {
  return (
    <OpenAPIPage
      operations={OPERATIONS}
      payload={{ bundled: document as never }}
      showTitle={false}
    />
  );
}
