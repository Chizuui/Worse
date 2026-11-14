/*
 * Harness phone shell service worker (served at /sw.js, scope /).
 * Caches the app shell only, network-first so desktop updates show up immediately.
 * /api/* and any non-GET request always go straight to the network and are never cached.
 */
const VERSION = 'harness-shell-v3';
const SHELL = [
  '/',
  '/assets/app.js',
  '/assets/style.css',
  '/assets/manifest.webmanifest',
  '/assets/icon.svg',
];

self.addEventListener('install', (event) => {
  event.waitUntil(
    caches
      .open(VERSION)
      .then((cache) => cache.addAll(SHELL))
      .catch(() => undefined)
      .then(() => self.skipWaiting()),
  );
});

self.addEventListener('activate', (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((keys) => Promise.all(keys.filter((key) => key !== VERSION).map((key) => caches.delete(key))))
      .then(() => self.clients.claim()),
  );
});

function isShell(url, request) {
  if (request.mode === 'navigate') return url.pathname === '/' || url.pathname === '/p';
  return SHELL.includes(url.pathname);
}

self.addEventListener('fetch', (event) => {
  const request = event.request;
  if (request.method !== 'GET') return;
  const url = new URL(request.url);
  if (url.origin !== self.location.origin) return;
  if (url.pathname.startsWith('/api/')) return;
  if (!isShell(url, request)) return;
  const key = request.mode === 'navigate' ? '/' : url.pathname;
  event.respondWith(
    fetch(request)
      .then((response) => {
        if (response && response.ok && response.type === 'basic') {
          const copy = response.clone();
          caches.open(VERSION).then((cache) => cache.put(key, copy)).catch(() => undefined);
        }
        return response;
      })
      .catch(() => caches.match(key).then((hit) => hit || offline())),
  );
});

function offline() {
  return new Response('Harness is offline. Reconnect and reload.', {
    status: 503,
    headers: { 'content-type': 'text/plain; charset=utf-8' },
  });
}
