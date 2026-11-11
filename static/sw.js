// AgentFlow's service worker: shows push notifications and opens the right page when one is
// tapped. It caches nothing; AgentFlow needs the server anyway.
"use strict";

self.addEventListener("install", () => self.skipWaiting());
self.addEventListener("activate", (e) => e.waitUntil(self.clients.claim()));

self.addEventListener("push", (e) => {
  let n = { title: "AgentFlow", body: "", url: "/" };
  try { n = Object.assign(n, e.data.json()); } catch (_) { /* keep the defaults */ }
  e.waitUntil(self.registration.showNotification(n.title, {
    body: n.body,
    tag: n.tag || undefined,
    data: { url: n.url },
    icon: "/static/icons/icon-192.png",
    badge: "/static/icons/icon-192.png",
  }));
});

self.addEventListener("notificationclick", (e) => {
  e.notification.close();
  const url = new URL((e.notification.data && e.notification.data.url) || "/", self.location.origin);
  // Only ever open AgentFlow's own pages.
  const target = url.origin === self.location.origin ? url.href : self.location.origin + "/";
  e.waitUntil((async () => {
    const wins = await self.clients.matchAll({ type: "window", includeUncontrolled: true });
    for (const w of wins) {
      if ("focus" in w) { await w.focus(); return w.navigate(target); }
    }
    return self.clients.openWindow(target);
  })());
});
