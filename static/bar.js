// The project the desktop bar shows stays picked (the owner, 2026-11-23: "lets make it so the
// selected project stays selected in local storage"). A project's pages remember it; a page outside
// the projects (Dashboard, Log, Usage, …) shows it in the switcher, with its tabs and their counts,
// none of them lit. "All projects" in the switcher forgets it. Loaded right after the switcher, not
// deferred, so the bar is whole before the page first draws; app.js then folds the tabs that don't fit.
"use strict";
(() => {
  const KEY = "af_project";
  const sw = document.querySelector(".pswitch");
  if (!sw) return;
  const set = (v) => {
    try {
      if (v) localStorage.setItem(KEY, v);
      else localStorage.removeItem(KEY);
    } catch {
      // no storage: nothing is remembered
    }
  };
  sw.addEventListener("click", (e) => {
    if (e.target.closest && e.target.closest('a[href="/projects"]')) set(null);
  });
  if (sw.dataset.here) {
    set(sw.dataset.here);
    return;
  }
  const tpl = document.querySelector("template[data-bar-tabs]");
  let slug = null;
  try {
    slug = localStorage.getItem(KEY);
  } catch {
    return;
  }
  if (!tpl || !slug) return;
  const item = [...sw.querySelectorAll("a[data-slug]")].find((a) => a.dataset.slug === slug);
  if (!item) {
    set(null); // archived or gone
    return;
  }
  const name = item.querySelector("span").textContent;
  sw.querySelector("summary > span").textContent = name;
  item.classList.add("on");
  const counts = { questions: [item.dataset.q, true], prs: [item.dataset.pr, false], "to-test": [item.dataset.t, true] };
  const base = `/p/${encodeURIComponent(slug)}`;
  const href = (key) =>
    key === "overview" ? base : key === "plan" || key === "settings" ? `${base}/${key}` : `${base}?tab=${key}`;
  const tab = (a, menu) => {
    const key = a.dataset.key;
    const el = document.createElement("a");
    el.href = href(key);
    if (key === "settings") el.dataset.dlg = "";
    if (menu) {
      el.className = "bmi";
      el.hidden = true;
      const s = document.createElement("span");
      s.textContent = a.textContent;
      el.append(s);
    } else {
      el.textContent = a.textContent;
    }
    const [n, warn] = counts[key] || ["0", false];
    if (Number(n) > 0) {
      const c = document.createElement("span");
      c.className = warn ? "mono n y" : "mono n";
      c.textContent = n;
      el.append(c);
    }
    return el;
  };
  const tabs = [...tpl.content.querySelectorAll("a[data-key]")];
  const bar = document.createElement("nav");
  bar.className = "btabs";
  bar.setAttribute("aria-label", name);
  bar.dataset.fold = "";
  bar.append(...tabs.map((a) => tab(a, false)));
  const more = document.createElement("details");
  more.className = "btabs-more";
  more.dataset.menu = "";
  more.hidden = true;
  const sum = document.createElement("summary");
  sum.textContent = "More";
  const chev = sw.querySelector(".chev-d");
  if (chev) sum.append(chev.cloneNode(true));
  const menu = document.createElement("div");
  menu.className = "bmenu";
  menu.append(...tabs.map((a) => tab(a, true)));
  more.append(sum, menu);
  sw.after(bar, more);
})();
