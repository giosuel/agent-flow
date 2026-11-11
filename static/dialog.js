// Records open over the page instead of replacing it (the owner, 2026-11-11: "sometimes, we could
// just use a dialog instead of a full blown window. like for tasks, on desktop this could be a
// normal dialog and on mobile like a overlay dialog panel that covers the full screen and comes
// from below"; for "tasks, questions, decisions, PRs and more you find useful where we don't have a
// ton of data to show").
//
// A click on a link to one of those records loads its page and shows its content in a dialog (a
// sheet from below on the phone). The address changes to the record's, so Back closes it and a
// reload shows its page; ‹ › step through the other records of the list it was opened from. No
// "open as a page" link (the owner, 2026-11-11: "remove the "open as a page" everywhere").
// Modified clicks, other records (builds, releases, drafts) and pages without the script open the
// page as before.
"use strict";
(() => {
  const SMALL = /^\/p\/[a-z0-9-]+\/(T|Q|D|ADR|PR|TT|S|F)-\d+$/;
  let box = null;
  let list = [];
  let at = -1;
  let opener = null;

  const record = (a) => {
    if (!a || a.target || a.hasAttribute("download")) return null;
    let url;
    try {
      url = new URL(a.href, location.href);
    } catch {
      return null;
    }
    if (url.origin !== location.origin || url.search || url.hash || !SMALL.test(url.pathname)) return null;
    return url.pathname;
  };

  function build() {
    box = document.createElement("div");
    box.className = "rdlg-back";
    box.innerHTML = `<div class="rdlg" role="dialog" aria-modal="true" aria-labelledby="rdlg-title">
      <div class="rdlg-grab" aria-hidden="true"></div>
      <div class="rdlg-head"><span class="sub" id="rdlg-title"></span><span class="grow"></span>
        <span class="rdlg-nav"><button type="button" class="rdlg-btn" data-step="-1" aria-label="Previous">‹</button><button type="button" class="rdlg-btn" data-step="1" aria-label="Next">›</button></span>
        <button type="button" class="rdlg-btn rdlg-x" aria-label="Close">×</button></div>
      <div class="rdlg-body"></div></div>`;
    box.addEventListener("click", (e) => {
      if (e.target === box || e.target.closest(".rdlg-x")) close();
      const step = e.target.closest("[data-step]");
      if (step) go(at + Number(step.dataset.step));
    });
    // On the phone, pulling the sheet down by its handle or head closes it.
    let y0 = null;
    box.addEventListener("touchstart", (e) => {
      if (e.target.closest(".rdlg-grab, .rdlg-head") && !e.target.closest("button, a")) y0 = e.touches[0].clientY;
    }, { passive: true });
    box.addEventListener("touchmove", (e) => {
      if (y0 === null) return;
      const dy = Math.max(0, e.touches[0].clientY - y0);
      box.querySelector(".rdlg").style.transform = `translateY(${dy}px)`;
    }, { passive: true });
    box.addEventListener("touchend", (e) => {
      if (y0 === null) return;
      const dy = e.changedTouches[0].clientY - y0;
      y0 = null;
      const sheet = box.querySelector(".rdlg");
      sheet.style.transform = "";
      if (dy > 110) close();
    });
    document.body.append(box);
  }

  async function show(path) {
    let doc;
    try {
      const r = await fetch(path, { headers: { accept: "text/html" }, credentials: "same-origin" });
      if (!r.ok || r.redirected) throw new Error("not shown");
      doc = new DOMParser().parseFromString(await r.text(), "text/html");
    } catch {
      location.href = path;
      return false;
    }
    const main = doc.querySelector("main");
    if (!main) {
      location.href = path;
      return false;
    }
    if (!box) build();
    const body = box.querySelector(".rdlg-body");
    body.replaceChildren(...[...main.childNodes].map((n) => document.importNode(n, true)));
    for (const el of body.querySelectorAll(".crumb-row, .back-bar")) el.remove();
    box.querySelector("#rdlg-title").textContent = (doc.querySelector(".crumb-where") || {}).textContent || "";
    const nav = box.querySelector(".rdlg-nav");
    nav.hidden = list.length < 2;
    box.querySelector("[data-step='-1']").disabled = at <= 0;
    box.querySelector("[data-step='1']").disabled = at < 0 || at >= list.length - 1;
    body.scrollTop = 0;
    document.dispatchEvent(new CustomEvent("af:content", { detail: body }));
    return true;
  }

  async function open(path) {
    if (!(await show(path))) return;
    if (!box.classList.contains("open")) {
      document.documentElement.classList.add("rdlg-on");
      requestAnimationFrame(() => box.classList.add("open"));
      history.pushState({ rdlg: path }, "", path);
    } else {
      history.replaceState({ rdlg: path }, "", path);
    }
    box.querySelector(".rdlg-x").focus({ preventScroll: true });
  }

  function go(i) {
    if (i < 0 || i >= list.length) return;
    at = i;
    open(list[i]);
  }

  // Closing: Back does it (the dialog's address is a history entry); popstate does the rest.
  function close() {
    if (history.state && history.state.rdlg) history.back();
    else hide();
  }

  function hide() {
    if (!box) return;
    box.classList.remove("open");
    document.documentElement.classList.remove("rdlg-on");
    setTimeout(() => {
      if (box && !box.classList.contains("open")) box.querySelector(".rdlg-body").replaceChildren();
    }, 250);
    if (opener && document.contains(opener)) opener.focus({ preventScroll: true });
  }

  window.addEventListener("popstate", (e) => {
    if (e.state && e.state.rdlg) {
      // Forward again into a dialog: show it.
      show(e.state.rdlg).then((ok) => ok && box.classList.add("open"));
    } else if (box && box.classList.contains("open")) {
      hide();
    }
  });

  document.addEventListener("keydown", (e) => {
    if (!box || !box.classList.contains("open")) return;
    if (e.key === "Escape") close();
    if (e.target.closest("input, textarea, select")) return;
    if (e.key === "ArrowLeft") go(at - 1);
    if (e.key === "ArrowRight") go(at + 1);
  });

  document.addEventListener("click", (e) => {
    if (e.defaultPrevented || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
    const a = e.target.closest("a[href]");
    const path = record(a);
    if (!path || path === location.pathname && !(box && box.contains(a))) return;
    e.preventDefault();
    if (!(box && box.contains(a))) {
      // The list it came from: links to such records in the same list or card.
      const scope = a.closest("ul, ol, tbody, .card, section") || document;
      list = [...new Set([...scope.querySelectorAll("a[href]")].map(record).filter(Boolean))];
      opener = a;
    }
    at = list.indexOf(path);
    if (at < 0) {
      list = [path];
      at = 0;
    }
    open(path);
  });
})();
