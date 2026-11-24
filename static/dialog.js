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
  const SMALL = /^\/p\/[a-z0-9-]+\/((T|Q|D|ADR|PR|TT|S|F|N)-\d+|steps\/\d+)$/;
  let box = null;
  let list = [];
  let at = -1;
  let opener = null;
  // The page under the dialog (its address before the record's).
  let under = location.pathname + location.search;

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

  // The dialog is the library's (app.js afDlg, md): its head holds the record's path, ‹ › and the
  // close button; Esc, ✕, the scrim and a pull on the phone go Back, which closes it. An answer
  // typed in it and not sent is asked about first (afDlg.leave), there and before ‹ › or a link
  // shows another record in its place.
  function build() {
    box = window.afDlg.open({ size: "md", cls: "rdlg", opener, dismiss: () => close(), closed: () => { box = null; } });
    box.tools.innerHTML = `<span class="rdlg-nav"><button type="button" class="ibtn" data-step="-1" aria-label="Previous"><svg class="i" aria-hidden="true"><use href="#i-prev"/></svg></button><button type="button" class="ibtn" data-step="1" aria-label="Next"><svg class="i" aria-hidden="true"><use href="#i-chevron"/></svg></button></span>`;
    box.tools.addEventListener("click", (e) => {
      const step = e.target.closest("[data-step]");
      if (step) go(at + Number(step.dataset.step));
    });
  }

  // `soft`: opened by the page itself (a direct link); if it can't be shown, stay on the page
  // instead of going to its address (which is this page again).
  async function show(path, soft) {
    let doc;
    try {
      const r = await fetch(path, { headers: { accept: "text/html" }, credentials: "same-origin" });
      if (!r.ok || r.redirected) throw new Error("not shown");
      doc = new DOMParser().parseFromString(await r.text(), "text/html");
    } catch {
      if (!soft) location.href = path;
      return false;
    }
    const main = doc.querySelector("main");
    if (!main) {
      if (!soft) location.href = path;
      return false;
    }
    if (!box) build();
    const body = box.body;
    body.replaceChildren(...[...main.childNodes].map((n) => document.importNode(n, true)));
    for (const el of body.querySelectorAll(".crumb-row, .back-bar")) el.remove();
    // The page's path continues in the dialog's header (D-222): its earlier parts go there, the
    // last is the record.
    const title = box.title;
    const where = doc.querySelector("nav.path");
    if (where) title.replaceChildren(document.importNode(where, true));
    else title.textContent = (doc.querySelector(".crumb-where") || {}).textContent || "";
    const nav = box.tools.querySelector(".rdlg-nav");
    nav.hidden = list.length < 2;
    nav.querySelector("[data-step='-1']").disabled = at <= 0;
    nav.querySelector("[data-step='1']").disabled = at < 0 || at >= list.length - 1;
    // An answer sent from here goes back to the page under the dialog when it's the record's
    // project's (its tab), as the form's own `next` does for the project's page (09-9). Over
    // the Dashboard it goes back there, to its "Sent to…" notice and the next question.
    for (const n of body.querySelectorAll("form[data-answer] input[name=next]")) {
      const u = new URL(under, location.origin);
      const want = new URL(n.value, location.origin);
      if (u.pathname === "/") {
        const slug = want.pathname.split("/")[2] || "";
        n.value = "/?answered=" + slug + "/" + (want.searchParams.get("answered") || "");
        continue;
      }
      if (u.pathname !== want.pathname) continue;
      u.searchParams.set("answered", want.searchParams.get("answered") || "");
      n.value = u.pathname + u.search;
    }
    body.scrollTop = 0;
    document.dispatchEvent(new CustomEvent("af:content", { detail: body }));
    return true;
  }

  async function open(path, soft) {
    const was = !!box;
    if (!was) under = location.pathname + location.search;
    if (!(await show(path, soft))) return;
    if (!was) history.pushState({ rdlg: path }, "", path);
    else history.replaceState({ rdlg: path }, "", path);
    if (!box.el.contains(document.activeElement)) box.el.querySelector(".dlg-x").focus({ preventScroll: true });
  }

  async function go(i) {
    if (i < 0 || i >= list.length) return;
    if (box && !(await window.afDlg.leave(box))) return;
    at = i;
    open(list[i]);
  }

  // Closing: Back does it (the dialog's address is a history entry); popstate does the rest.
  function close() {
    if (history.state && history.state.rdlg) history.back();
    else hide();
  }

  function hide() {
    if (box) box.close();
  }

  window.addEventListener("popstate", (e) => {
    if (e.state && e.state.rdlg) {
      // Forward again into a dialog: show it.
      show(e.state.rdlg);
    } else if (box) {
      hide();
    }
  });

  // ← → step through the list while the record is the top dialog (Esc is the controller's).
  document.addEventListener("keydown", (e) => {
    if (!box || window.afDlg.top() !== box) return;
    if (e.target.closest("input, textarea, select")) return;
    if (e.key === "ArrowLeft") go(at - 1);
    if (e.key === "ArrowRight") go(at + 1);
  });

  document.addEventListener("click", (e) => {
    if (e.defaultPrevented || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
    const a = e.target.closest("a[href]");
    const path = record(a);
    if (!path || path === location.pathname && !(box && box.el.contains(a))) return;
    e.preventDefault();
    const inside = !!box && box.el.contains(a);
    (async () => {
      if (inside && !(await window.afDlg.leave(box))) return;
      if (!inside) {
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
    })();
  });

  // A direct link to a record: the server sent its project's page; the record opens over it, and
  // Back (or ✕) leaves the project's page.
  const o = document.body.dataset;
  if (o.openDlg === "rec" && o.openUrl && o.openFrom) {
    history.replaceState(null, "", o.openFrom);
    list = [o.openUrl];
    at = 0;
    open(o.openUrl, true);
  }
})();
