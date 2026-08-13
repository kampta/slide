import {
  APP_SHELL,
  staticRequestStrategy,
} from "./serviceWorkerPolicy.ts";

const CACHE_PREFIX = "slide-static-";
const CACHE_NAME = `${CACHE_PREFIX}v1`;

self.addEventListener("install", (event) => {
  event.waitUntil(
    (async () => {
      const cache = await caches.open(CACHE_NAME);
      await cache.addAll(APP_SHELL);

      // Vite fingerprints its JS/CSS files. Discover the current names from
      // the built shell so a freshly installed app can start without a network.
      const shell = await cache.match("/");
      const html = shell ? await shell.text() : "";
      const assets = [...html.matchAll(/(?:src|href)="(\/assets\/[^"]+)"/g)].map(
        (match) => match[1],
      );
      if (assets.length > 0) await cache.addAll(assets);
      await self.skipWaiting();
    })(),
  );
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    (async () => {
      const names = await caches.keys();
      await Promise.all(
        names
          .filter((name) => name.startsWith(CACHE_PREFIX) && name !== CACHE_NAME)
          .map((name) => caches.delete(name)),
      );
      await self.clients.claim();
    })(),
  );
});

self.addEventListener("fetch", (event) => {
  const request = event.request;
  if (request.method !== "GET") return;

  const url = new URL(request.url);
  if (url.origin !== self.location.origin) return;

  switch (staticRequestStrategy(url.pathname, request.mode === "navigate")) {
    case "network-only":
      return;
    case "network-first":
      event.respondWith(
        fetch(request)
          .then(async (response) => {
            if (response.ok) {
              const cache = await caches.open(CACHE_NAME);
              await cache.put(request.mode === "navigate" ? "/" : request, response.clone());
            }
            return response;
          })
          .catch(async () => {
            const cached = await caches.match(request.mode === "navigate" ? "/" : request);
            return cached ?? Response.error();
          }),
      );
      return;
    case "cache-first":
      event.respondWith(
        (async () => {
          const cached = await caches.match(request);
          if (cached) return cached;
          const response = await fetch(request);
          if (response.ok) {
            const cache = await caches.open(CACHE_NAME);
            await cache.put(request, response.clone());
          }
          return response;
        })(),
      );
  }
});
