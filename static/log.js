// The Log page: live by default (Pause stops it), older entries load as you scroll, Copy as query.
"use strict";
document.addEventListener("DOMContentLoaded", () => {
  const list = document.getElementById("lrows");
  if (!list) return;

  // Older entries when the loading row comes into view.
  let loading = false;
  const watch = () => {
    const more = list.querySelector(".lmore");
    if (!more || !("IntersectionObserver" in window)) return;
    const io = new IntersectionObserver(async (seen) => {
      if (!seen.some((s) => s.isIntersecting) || loading) return;
      loading = true;
      io.disconnect();
      try {
        const res = await fetch(more.dataset.next, { credentials: "same-origin" });
        if (!res.ok) throw new Error(res.statusText);
        more.insertAdjacentHTML("afterend", await res.text()); // rendered and escaped by the server
        more.remove();
      } catch (_) {
        more.textContent = "Couldn't load older entries. Scroll to try again.";
      }
      loading = false;
      watch();
    }, { rootMargin: "400px" });
    io.observe(more);
  };
  watch();

  // Live: new rows on top, until paused.
  const btn = document.getElementById("live");
  const dot = document.getElementById("livedot");
  let source = null;
  const start = () => {
    const u = "/log/stream?q=" + encodeURIComponent(btn.dataset.q || "") + "&range=" + encodeURIComponent(btn.dataset.range || "") + "&after=" + encodeURIComponent(btn.dataset.after || "0");
    source = new EventSource(u);
    source.onmessage = (m) => {
      try {
        const d = JSON.parse(m.data);
        btn.dataset.after = d.after;
        list.insertAdjacentHTML("afterbegin", d.html);
        for (const li of list.querySelectorAll("li.lrow:not(.seen)")) { li.classList.add("seen", "fresh"); setTimeout(() => li.classList.remove("fresh"), 2500); }
      } catch (_) { /* a broken message */ }
    };
    btn.querySelector("span").textContent = "Pause";
    btn.classList.remove("on");
    if (dot) { dot.textContent = "Live"; dot.classList.remove("paused"); }
  };
  const stop = () => {
    if (source) source.close();
    source = null;
    btn.querySelector("span").textContent = "Resume";
    btn.classList.add("on");
    if (dot) { dot.textContent = "Paused"; dot.classList.add("paused"); }
  };
  if (btn) {
    for (const li of list.querySelectorAll("li.lrow")) li.classList.add("seen");
    btn.addEventListener("click", () => (source ? stop() : start()));
    start();
  }

  // Copy as query.
  document.addEventListener("click", async (e) => {
    const b = e.target.closest("button[data-copy]");
    if (!b) return;
    try { await navigator.clipboard.writeText(b.dataset.copy); b.querySelector("span").textContent = "Copied"; } catch (_) { /* no clipboard */ }
  });
});
