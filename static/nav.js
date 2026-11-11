// App-like page changes on the phone (the owner, 2026-11-11: "clicking on something should open
// a new "window" or feel more like navigation"). With cross-document view transitions (Safari
// 18.2+, Chrome 126+) a tapped link slides the next page in from the right, Back (the back bar or
// the browser's) slides it out to the right, and the tab bar switches with a quick fade. The
// direction is decided here; app.css draws it. Loaded in <head>, before the page first renders, as
// `pagereveal` needs. Browsers without view transitions just load the page.
"use strict";
(() => {
  const KEY = "af_nav";
  const store = (v) => {
    try {
      if (v === null) sessionStorage.removeItem(KEY);
      else sessionStorage.setItem(KEY, v);
    } catch {
      // no storage: every change is a push
    }
  };
  const read = () => {
    try {
      return sessionStorage.getItem(KEY);
    } catch {
      return null;
    }
  };
  document.addEventListener(
    "click",
    (e) => {
      const a = e.target.closest && e.target.closest("a[href]");
      if (!a) return;
      store(a.closest(".tabbar") ? "tab" : a.matches("[data-nav=back], .crumbs a, a.back") ? "back" : "push");
    },
    true,
  );
  const traverse = (activation) => {
    if (!activation || activation.navigationType !== "traverse" || !activation.from || !activation.entry) return null;
    return activation.entry.index < activation.from.index ? "back" : "push";
  };
  window.addEventListener("pageswap", (e) => {
    if (!e.viewTransition) return;
    const kind = traverse(e.activation) || read() || "push";
    store(kind);
    e.viewTransition.types.add(kind);
  });
  window.addEventListener("pagereveal", (e) => {
    if (!e.viewTransition) return;
    const act = window.navigation && navigation.activation;
    if (act && act.navigationType === "reload") {
      e.viewTransition.skipTransition();
      store(null);
      return;
    }
    const kind = traverse(act) || read() || "push";
    store(null);
    e.viewTransition.types.add(kind);
  });
})();
