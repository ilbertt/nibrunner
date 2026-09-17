import { createRootRoute, HeadContent, Outlet, Scripts } from '@tanstack/react-router';
import { RootProvider } from 'fumadocs-ui/provider/tanstack';
import SearchDialog from '@/components/search';
import { appName, description, pageMeta, siteUrl, tagline } from '@/lib/shared';
import appCss from '@/styles/app.css?url';

const card = `${siteUrl}/og.png`;

export const Route = createRootRoute({
  head: () => ({
    meta: [
      {
        charSet: 'utf-8',
      },
      {
        name: 'viewport',
        content: 'width=device-width, initial-scale=1',
      },
      ...pageMeta({ title: `${appName} — ${tagline}`, description }),
      { property: 'og:site_name', content: appName },
      { property: 'og:type', content: 'website' },
      { property: 'og:image', content: card },
      { property: 'og:image:width', content: '1200' },
      { property: 'og:image:height', content: '630' },
      { property: 'og:image:alt', content: 'One VPS. Hundreds of apps.' },
      { name: 'twitter:card', content: 'summary_large_image' },
      { name: 'twitter:image', content: card },
    ],
    links: [
      { rel: 'stylesheet', href: appCss },
      // Safari reads no SVG favicon.
      { rel: 'icon', href: '/favicon.ico', sizes: '32x32' },
      { rel: 'icon', href: '/logo.svg', type: 'image/svg+xml' },
      { rel: 'apple-touch-icon', href: '/apple-touch-icon.png' },
    ],
  }),
  component: RootComponent,
});

function RootComponent() {
  return (
    <html lang="en" suppressHydrationWarning>
      <head>
        <HeadContent />
      </head>
      <body className="flex min-h-screen flex-col">
        <RootProvider search={{ SearchDialog }}>
          <Outlet />
        </RootProvider>
        <Scripts />
      </body>
    </html>
  );
}
