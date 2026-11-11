// The Machine page stays current while it's open: the figures at the top every 5 seconds, the
// charts and the agent table every minute (a new sample is taken each minute). Paused while the
// page is hidden, so a forgotten tab doesn't keep asking.
"use strict";
(() => {
  const tiles = document.getElementById("tiles");
  if (!tiles) return;
  let lastCharts = Date.now();
  let busy = false;

  async function refreshTiles() {
    const r = await fetch("/machine/now", { headers: { accept: "application/json" } });
    if (!r.ok) return;
    const { tiles: fresh } = await r.json();
    tiles.querySelectorAll(".tile").forEach((el, i) => {
      const t = fresh[i];
      if (!t) return;
      el.querySelector(".tile-v").textContent = t.value;
      el.classList.toggle("warn-card", t.warn);
    });
  }

  async function refreshCharts() {
    const r = await fetch(location.href);
    if (!r.ok) return;
    const doc = new DOMParser().parseFromString(await r.text(), "text/html");
    const fresh = doc.getElementById("hist");
    const old = document.getElementById("hist");
    if (fresh && old) {
      old.replaceWith(document.importNode(fresh, true));
      document.dispatchEvent(new Event("af:charts"));
    }
  }

  async function tick() {
    if (document.hidden || busy) return;
    busy = true;
    try {
      await refreshTiles();
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
