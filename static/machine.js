// The Machine page stays current while it's open: the figures at the top every 5 seconds, the
// charts and the agent table every minute (a new sample is taken each minute). Paused while the
// page is hidden, so a forgotten tab doesn't keep asking.
"use strict";
(() => {
  if (!document.getElementById("tiles")) return;
  let lastCharts = Date.now();
  let busy = false;

  async function refreshTiles() {
    const r = await fetch("/machine/now", { headers: { accept: "application/json" } });
    if (!r.ok) return;
    const { tiles: fresh } = await r.json();
    document.querySelectorAll("#tiles .mtile").forEach((el, i) => {
      const t = fresh[i];
      if (!t) return;
      el.querySelector(".mtile-v").textContent = t.big;
      el.querySelector(".mtile-big small").textContent = t.small;
      el.classList.toggle("warn-card", t.warn);
    });
  }

  async function refreshCharts() {
    const r = await fetch(location.href);
    if (!r.ok) return;
    const doc = new DOMParser().parseFromString(await r.text(), "text/html");
    // The cards' last hour too.
    const freshTiles = doc.getElementById("tiles");
    const oldTiles = document.getElementById("tiles");
    if (freshTiles && oldTiles) oldTiles.replaceWith(document.importNode(freshTiles, true));
    const fresh = doc.getElementById("hist");
    const old = document.getElementById("hist");
    if (fresh && old) {
      old.replaceWith(document.importNode(fresh, true));
      document.dispatchEvent(new Event("af:charts"));
    }
  }

  // The Build servers section, when there is one.
  async function refreshBuilds() {
    const old = document.getElementById("builds");
    if (!old) return;
    const r = await fetch("/machine/builds");
    if (!r.ok) return;
    const doc = new DOMParser().parseFromString(await r.text(), "text/html");
    const fresh = doc.getElementById("builds");
    if (fresh) old.replaceWith(document.importNode(fresh, true));
  }

  async function tick() {
    if (document.hidden || busy) return;
    busy = true;
    try {
      await refreshTiles();
      await refreshBuilds();
      if (Date.now() - lastCharts >= 60_000) {
        lastCharts = Date.now();
        await refreshCharts();
      }
    } catch {
      // Offline for a moment: try again on the next tick.
    } finally {
      busy = false;
    }
  }

  setInterval(tick, 5_000);
  document.addEventListener("visibilitychange", tick);
})();
