/// <reference lib="webworker" />
// Service worker for the PWA: offline app shell, and the Web Share Target.
// File transfers never pass through it (they stream peer to peer); shared
// items only move from the OS share sheet into this origin's own storage.
import { cleanupOutdatedCaches, createHandlerBoundToURL, precacheAndRoute } from "workbox-precaching";
import { NavigationRoute, registerRoute } from "workbox-routing";
declare const self: ServiceWorkerGlobalScope;
precacheAndRoute(self.__WB_MANIFEST);
cleanupOutdatedCaches();
// Deep links (/devices, /inbox, …) open offline: navigations get the cached app shell.
registerRoute(new NavigationRoute(createHandlerBoundToURL("/index.html"), { denylist: [/^\/share-target/, /^\/v1\//] }));
self.addEventListener("message", (event) => {
  if (event.data?.type === "SKIP_WAITING") void self.skipWaiting();
});

/** Share sheet → "Ferry": keep the items for the app (read by `takeShared`), then open it. */
self.addEventListener("fetch", (event) => {
  const url = new URL(event.request.url);
  if (event.request.method !== "POST" || url.origin !== self.location.origin || url.pathname !== "/share-target") return;
  // The OS share sheet sends no referrer; another website posting here would.
  const referrer = event.request.referrer;
  if (referrer && new URL(referrer).origin !== self.location.origin) {
    event.respondWith(new Response("Forbidden", { status: 403 }));
    return;
  }
  event.respondWith(
    (async () => {
      const form = await event.request.formData();
      const cache = await caches.open("ferry-share");
      for (const old of await cache.keys()) await cache.delete(old);
      let n = 0;
      const parts = ["title", "text", "url"].map((k) => form.get(k)).filter((v): v is string => typeof v === "string" && !!v.trim());
      const text = [...new Set(parts.map((p) => p.trim()))].join("\n");
      if (text) {
        await cache.put(`/__shared/${n++}`, new Response(text, { headers: { "content-type": "text/plain", "x-ferry-kind": "text" } }));
      }
      for (const file of form.getAll("files")) {
        if (!(file instanceof File)) continue;
        const headers = { "content-type": file.type || "application/octet-stream", "x-ferry-kind": "file", "x-ferry-name": encodeURIComponent(file.name) };
        await cache.put(`/__shared/${n++}`, new Response(file, { headers }));
      }
      return Response.redirect("/?shared=1", 303);
    })(),
  );
});
