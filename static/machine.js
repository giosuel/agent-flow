// The Machines pages stay current while they're open (D-277): every 5 seconds the page asks for
// itself again and swaps in its live parts, the elements with data-live and an id (AgentFlow's
// figures, the build machines, a machine's jobs right now and its history). A part with the focus
// in it waits for the next round, so a button about to be pressed stays put. Paused while the page
// is hidden, so a forgotten tab doesn't keep asking.
"use strict";
(() => {
  const parts = () => [...document.querySelectorAll("[data-live][id]")];
  if (!parts().length) return;
  let busy = false;

  async function tick() {
    if (document.hidden || busy) return;
    busy = true;
    try {
      const r = await fetch(location.pathname + location.search, { headers: { accept: "text/html" } });
      if (!r.ok || r.redirected) return;
      const doc = new DOMParser().parseFromString(await r.text(), "text/html");
      for (const old of parts()) {
        const fresh = doc.getElementById(old.id);
        if (!fresh || fresh.outerHTML === old.outerHTML) continue;
        if (old.contains(document.activeElement)) continue;
        const node = document.importNode(fresh, true);
        if (window.afKeepSort) window.afKeepSort(old, node);
        old.replaceWith(node);
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
