// AgentFlow's small helpers. The pages are plain forms and links and work without JavaScript
// wherever they can.
"use strict";

// Pages show times in this browser's time zone: the server reads it from the af_tz cookie. When
// the cookie was missing (the first visit) or the zone changed (travel, another device), set it
// and reload once, so this page uses it too. Zone names ("Europe/Zurich") are valid cookie values.
(() => {
  let zone;
  try {
    zone = Intl.DateTimeFormat().resolvedOptions().timeZone;
  } catch {
    return;
  }
  if (!zone || !/^[A-Za-z0-9/_+-]{1,64}$/.test(zone)) return;
  const want = `af_tz=${zone}`;
  if (document.cookie.split("; ").includes(want)) return;
  const secure = location.protocol === "https:" ? "; Secure" : "";
  document.cookie = `${want}; Path=/; Max-Age=31536000; SameSite=Lax${secure}`;
  // Once per zone and tab, and only if the cookie stuck: never a reload loop.
  try {
    if (sessionStorage.getItem("af_tz_reloaded") === zone) return;
    sessionStorage.setItem("af_tz_reloaded", zone);
  } catch {
    return;
  }
  if (document.cookie.split("; ").includes(want)) location.reload();
})();

// Dialogs in the page's own style instead of the browser's (the owner, 2026-11-17: "remove all
// the simple alerts and replace them with actual styled dialogs that fit into the design"): a
// centred card over a dimmed page, on the phone too. window.afDialog({title, text, ok, cancel,
// danger}) resolves true for OK; without cancel it's a message with one button.
window.afDialog = ({ title, text = "", ok = "OK", cancel = null, danger = false }) =>
  new Promise((resolve) => {
    const back = document.createElement("div");
    back.className = "cdlg-back";
    back.innerHTML = `<div class="cdlg" role="${cancel ? "alertdialog" : "dialog"}" aria-modal="true" aria-labelledby="cdlg-t" aria-describedby="cdlg-x">
      <h2 id="cdlg-t"></h2><p id="cdlg-x" class="sub"></p>
      <div class="cdlg-btns">${cancel ? '<button type="button" class="btn ghost" data-v="0"></button>' : ""}<button type="button" class="btn${danger ? " solid-danger" : ""}" data-v="1"></button></div></div>`;
    back.querySelector("#cdlg-t").textContent = title;
    const body = back.querySelector("#cdlg-x");
    body.textContent = text;
    body.hidden = !text;
    back.querySelector('[data-v="1"]').textContent = ok;
    if (cancel) back.querySelector('[data-v="0"]').textContent = cancel;
    const opener = document.activeElement;
    const done = (v) => {
      back.classList.remove("open");
      document.removeEventListener("keydown", key, true);
      setTimeout(() => back.remove(), 160);
      if (opener && opener.focus) opener.focus();
      resolve(v);
    };
    const key = (e) => {
      if (e.key === "Escape") { e.preventDefault(); done(false); }
      // Focus stays in the dialog.
      if (e.key === "Tab") {
        const b = [...back.querySelectorAll("button")];
        const i = b.indexOf(document.activeElement);
        e.preventDefault();
        b[(i + (e.shiftKey ? b.length - 1 : 1)) % b.length].focus();
      }
    };
    back.addEventListener("click", (e) => {
      const btn = e.target.closest("button[data-v]");
      if (btn) done(btn.dataset.v === "1");
      else if (e.target === back) done(false);
    });
    document.addEventListener("keydown", key, true);
    document.body.append(back);
    requestAnimationFrame(() => back.classList.add("open"));
    // The safe choice has focus, so Enter never removes something by accident.
    back.querySelector(cancel ? '[data-v="0"]' : '[data-v="1"]').focus();
  });

// Buttons with data-confirm ask first (no inline handlers: the CSP forbids them). The question's
// first sentence is the title, the rest the text; the button's own label confirms, red when it
// removes or ends something.
document.addEventListener("click", (e) => {
  const el = e.target.closest("[data-confirm]");
  if (!el || el.dataset.confirmed) return;
  e.preventDefault();
  e.stopImmediatePropagation();
  const q = el.dataset.confirm;
  const cut = q.search(/[?.]\s/);
  const title = cut < 0 ? q : q.slice(0, cut + 1);
  const text = cut < 0 ? "" : q.slice(cut + 2);
  const label = (el.dataset.confirmOk || el.textContent || el.getAttribute("aria-label") || "OK").trim() || "OK";
  const danger = el.classList.contains("danger") || /^(remove|revoke|delete|rotate|sign out|switch off|turn off|cancel job)/i.test(label) || el.dataset.danger !== undefined;
  window.afDialog({ title, text, ok: label, cancel: "Cancel", danger }).then((yes) => {
    if (!yes) return;
    el.dataset.confirmed = "1";
    el.click();
    delete el.dataset.confirmed;
  });
}, true);

// Keep the current milestone in view in the (scrolling) milestone strip, and the current page's
// tab in the main nav and the project tabs, which scroll sideways on the phone.
document.addEventListener("DOMContentLoaded", () => {
  for (const on of document.querySelectorAll(".nav [aria-current=page], .tabs [aria-current=page]")) {
    const bar = on.parentElement;
    if (bar.scrollWidth > bar.clientWidth) bar.scrollLeft = on.offsetLeft - bar.offsetLeft - (bar.clientWidth - on.offsetWidth) / 2;
  }
  const now = document.querySelector(".ms-strip .ms.now");
  const strip = now && now.parentElement;
  if (strip) strip.scrollLeft = now.offsetLeft - strip.offsetLeft - 24;
});

// Build feedback: "2 of 3 marked" as the owner goes.
document.addEventListener("change", (e) => {
  const form = e.target.closest("form.fb");
  if (!form) return;
  const groups = new Set([...form.querySelectorAll(".verdicts input")].map((i) => i.name));
  const marked = [...groups].filter((n) => form.querySelector(`input[name="${n}"]:checked`)).length;
  const out = form.querySelector("[data-fb-count]");
  if (out) out.textContent = `${marked} of ${groups.size} marked`;
});

// A copy button (an icon in the field's corner) on every command or token block (pre.cmd). Uses
// the clipboard API where the page is secure (https), and falls back to selecting the text and
// copying it (http on the WireGuard address). A check mark shows it worked.
const COPY_ICON =
  '<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><rect x="9" y="9" width="11" height="11" rx="2"/><path d="M5 15V6a2 2 0 0 1 2-2h8"/></svg>';
const DONE_ICON = '<svg class="i" viewBox="0 0 24 24" aria-hidden="true"><path d="m5 12.5 4.5 4.5L19 7.5"/></svg>';

async function copyText(pre) {
  try {
    if (navigator.clipboard && window.isSecureContext) {
      await navigator.clipboard.writeText(pre.textContent.trim());
      return true;
    }
  } catch {
    // fall through to the selection
  }
  const range = document.createRange();
  range.selectNodeContents(pre);
  const sel = window.getSelection();
  sel.removeAllRanges();
  sel.addRange(range);
  let ok = false;
  try {
    ok = document.execCommand("copy");
  } catch {
    ok = false;
  }
  sel.removeAllRanges();
  return ok;
}

// Wired on load, and again for content that arrives later (a record in a dialog): `af:content`.
const wireCopy = (root) => {
  for (const pre of root.querySelectorAll("pre.cmd")) {
    if (pre.parentElement && pre.parentElement.classList.contains("copy-wrap")) continue;
    const wrap = document.createElement("div");
    wrap.className = "copy-wrap";
    pre.replaceWith(wrap);
    wrap.append(pre);
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "copy-btn";
    btn.title = "Copy";
    btn.setAttribute("aria-label", "Copy");
    btn.innerHTML = COPY_ICON;
    wrap.append(btn);
    btn.addEventListener("click", async () => {
      const ok = await copyText(pre);
      btn.innerHTML = ok ? DONE_ICON : COPY_ICON;
      btn.classList.toggle("done", ok);
      btn.setAttribute("aria-label", ok ? "Copied" : "Copy didn't work; select the text and copy it");
      btn.title = btn.getAttribute("aria-label");
      setTimeout(() => {
        btn.innerHTML = COPY_ICON;
        btn.classList.remove("done");
        btn.setAttribute("aria-label", "Copy");
        btn.title = "Copy";
      }, 2000);
    });
  }
};
document.addEventListener("DOMContentLoaded", () => wireCopy(document));
document.addEventListener("af:content", (e) => wireCopy(e.detail));

// Charts: the exact numbers under the pointer or finger (the owner, 2026-11-11: "allow hover to
// see exact datapoints"). Each chart's SVG carries its columns' numbers in data-tips; for bars the
// hovered column is lit, for lines and areas a crosshair marks the point. On a phone, a first tap
// on a bar that opens something shows its numbers, a second tap opens it. Small marks with a
// data-tip (the log's timeline, a project's token strip) get the same tooltip with their one line.
(() => {
  const HEX = /^#[0-9a-fA-F]{3,8}$/;
  let lastPointer = "mouse";
  document.addEventListener("pointerdown", (e) => { lastPointer = e.pointerType; }, true);

  function render(tip, col) {
    tip.replaceChildren();
    const h = document.createElement("strong");
    h.textContent = col.t;
    tip.append(h);
    for (const [color, name, value] of col.r) {
      const row = document.createElement("div");
      row.className = "tip-row";
      const sw = document.createElement("span");
      sw.className = "tip-sw";
      if (HEX.test(color)) sw.style.background = color;
      const n = document.createElement("span");
      n.textContent = name;
      const v = document.createElement("span");
      v.className = "tip-v";
      v.textContent = value;
      row.append(sw, n, v);
      tip.append(row);
    }
    if (!col.r.length) {
      const none = document.createElement("div");
      none.className = "tip-none";
      none.textContent = "nothing";
      tip.append(none);
    }
    if (col.s) {
      const row = document.createElement("div");
      row.className = "tip-row tip-total";
      const n = document.createElement("span");
      n.textContent = "Total";
      const v = document.createElement("span");
      v.className = "tip-v";
      v.textContent = col.s;
      row.append(document.createElement("span"), n, v);
      tip.append(row);
    }
  }

  function chart(svg) {
    let cols;
    try {
      cols = JSON.parse(svg.dataset.tips || "[]");
    } catch {
      return;
    }
    if (!cols.length) return;
    const box = svg.closest(".chart") || svg.parentElement;
    const points = svg.dataset.mode === "points";
    const centers = svg.dataset.mode === "centers";
    const mark = document.createElement("div");
    mark.className = points || centers ? "chart-cross" : "chart-band";
    mark.hidden = true;
    const tip = document.createElement("div");
    tip.className = "chart-tip";
    tip.hidden = true;
    tip.setAttribute("aria-hidden", "true");
    box.append(mark, tip);
    let shown = -1;

    const at = (clientX) => {
      const r = svg.getBoundingClientRect();
      const x = Math.min(Math.max(clientX - r.left, 0), r.width - 0.01);
      const n = cols.length;
      return points ? Math.round((x / r.width) * (n - 1)) : Math.floor((x / r.width) * n);
    };
    const show = (i, clientX) => {
      const r = svg.getBoundingClientRect();
      const b = box.getBoundingClientRect();
      const n = cols.length;
      const left = r.left - b.left;
      const top = r.top - b.top;
      if (points) {
        mark.style.left = `${left + (i / (n - 1)) * r.width}px`;
      } else if (centers) {
        mark.style.left = `${left + ((i + 0.5) / n) * r.width}px`;
      } else {
        mark.style.left = `${left + (i / n) * r.width}px`;
        mark.style.width = `${r.width / n}px`;
      }
      mark.style.top = `${top}px`;
      mark.style.height = `${r.height}px`;
      mark.hidden = false;
      if (i !== shown) render(tip, cols[i]);
      tip.hidden = false;
      // Beside the pointer, flipped to the left near the right edge; never outside the chart.
      const x = clientX - b.left;
      const w = tip.offsetWidth;
      let tx = x + 14;
      if (tx + w > b.width) tx = x - w - 14;
      tip.style.left = `${Math.max(0, Math.min(tx, b.width - w))}px`;
      tip.style.top = `${top + 4}px`;
      shown = i;
    };
    const hide = () => {
      mark.hidden = true;
      tip.hidden = true;
      shown = -1;
    };
    svg.addEventListener("pointermove", (e) => show(at(e.clientX), e.clientX));
    svg.addEventListener("pointerdown", (e) => show(at(e.clientX), e.clientX));
    svg.addEventListener("pointerleave", (e) => {
      if (e.pointerType === "mouse") hide();
    });
    // On a touch screen the first tap on a linked bar shows its numbers; the next one opens it.
    svg.addEventListener(
      "click",
      (e) => {
        if (lastPointer === "mouse" || !e.target.closest("a")) return;
        const i = at(e.clientX);
        if (svg.dataset.tapped !== String(i)) {
          e.preventDefault();
          svg.dataset.tapped = String(i);
          show(i, e.clientX);
        }
      },
      true,
    );
    document.addEventListener("pointerdown", (e) => {
      if (!box.contains(e.target)) {
        hide();
        delete svg.dataset.tapped;
      }
    });
  }

  // One floating tooltip for small marks with a data-tip.
  let smallWired = false;
  function small() {
    if (smallWired) return;
    smallWired = true;
    const tip = document.createElement("div");
    tip.className = "chart-tip floating";
    tip.hidden = true;
    tip.setAttribute("aria-hidden", "true");
    document.body.append(tip);
    const place = (el) => {
      tip.textContent = el.dataset.tip;
      tip.hidden = false;
      const r = el.getBoundingClientRect();
      const w = tip.offsetWidth;
      tip.style.left = `${Math.max(8, Math.min(r.left + r.width / 2 - w / 2, innerWidth - w - 8)) + scrollX}px`;
      tip.style.top = `${r.top + scrollY - tip.offsetHeight - 8}px`;
    };
    document.addEventListener("pointerover", (e) => {
      const el = e.target.closest && e.target.closest("[data-tip]");
      if (el) place(el);
    });
    document.addEventListener("pointerout", (e) => {
      const el = e.target.closest && e.target.closest("[data-tip]");
      if (el && e.pointerType === "mouse") tip.hidden = true;
    });
    document.addEventListener("focusin", (e) => {
      const el = e.target.closest && e.target.closest("[data-tip]");
      if (el) place(el);
      else tip.hidden = true;
    });
    document.addEventListener("pointerdown", (e) => {
      if (!(e.target.closest && e.target.closest("[data-tip]"))) tip.hidden = true;
    });
  }

  document.addEventListener("DOMContentLoaded", () => {
    for (const svg of document.querySelectorAll("svg[data-tips]")) {
      svg.dataset.wired = "1";
      chart(svg);
    }
    if (document.querySelector("[data-tip]")) small();
  });
  // The Machine page swaps its charts every minute: wire up the new ones.
  document.addEventListener("af:charts", () => {
    for (const svg of document.querySelectorAll("svg[data-tips]:not([data-wired])")) {
      svg.dataset.wired = "1";
      chart(svg);
    }
    if (document.querySelector("[data-tip]")) small();
  });
})();

// Answering a question: pick an option or write your own answer, never both, and nothing goes
// out until Send answer (the owner, 2026-11-11). Choosing an option empties the text; typing
// clears the option; Send is enabled only when exactly one of them is given. The server refuses
// both as well, so this holds without the script too.
const wireAnswers = (root) => {
  for (const form of root.querySelectorAll("form[data-answer]")) {
    const words = form.querySelector("[name=words]");
    const radios = [...form.querySelectorAll("input[name=option]")];
    const send = form.querySelector("button[type=submit]");
    const update = () => {
      const picked = radios.some((r) => r.checked);
      const typed = words && words.value.trim() !== "";
      send.disabled = !(picked !== typed);
    };
    for (const r of radios) {
      r.addEventListener("change", () => {
        if (words) words.value = "";
        update();
      });
    }
    if (words) {
      words.addEventListener("input", () => {
        if (words.value.trim() !== "") for (const r of radios) r.checked = false;
        update();
      });
    }
    update();
  }
};
document.addEventListener("DOMContentLoaded", () => wireAnswers(document));
document.addEventListener("af:content", (e) => wireAnswers(e.detail));

// To test: count what's ticked; Send needs at least one.
const wireToTest = (root) => {
  for (const form of root.querySelectorAll("form[data-tt]")) {
    const boxes = [...form.querySelectorAll("input[type=checkbox]")];
    const count = form.querySelector("[data-tt-count]");
    const send = form.querySelector("[data-tt-send]");
    if (!count || !send) continue;
    const update = () => {
      const n = boxes.filter((b) => b.checked).length;
      count.textContent = `${n} ticked`;
      send.disabled = n === 0;
      form.classList.toggle("tt-active", n > 0);
    };
    for (const b of boxes) b.addEventListener("change", update);
    update();
  }
};
document.addEventListener("DOMContentLoaded", () => wireToTest(document));
document.addEventListener("af:content", (e) => wireToTest(e.detail));

// Project overview: a milestone button swaps the board below it without reloading the page.
document.addEventListener("click", async (e) => {
  const a = e.target.closest("a[data-ms]");
  if (!a || e.defaultPrevented || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
  const board = document.getElementById("board");
  if (!board) return;
  e.preventDefault();
  board.classList.add("loading");
  try {
    const r = await fetch(a.href, { credentials: "same-origin" });
    if (!r.ok) throw new Error(r.status);
    const doc = new DOMParser().parseFromString(await r.text(), "text/html");
    const next = doc.getElementById("board");
    if (!next) throw new Error("no board");
    board.replaceWith(next);
    for (const m of document.querySelectorAll("a[data-ms]")) {
      const on = m === a;
      m.classList.toggle("sel", on);
      if (on) m.setAttribute("aria-current", "true"); else m.removeAttribute("aria-current");
    }
    history.replaceState(history.state, "", a.href.replace(/#.*$/, ""));
    document.dispatchEvent(new CustomEvent("af:content", { detail: next }));
  } catch {
    location.href = a.href;
  }
});

// Project tabs on the phone switch in place (the owner, 2026-11-12: "on mobile clicking a tab in a
// project page doesn't add navigation and instead just switches tabs"): the tab's page is fetched,
// its content swapped in, and the address replaced rather than pushed, so Back leaves the project
// instead of stepping through its tabs. Anything going wrong falls back to loading the tab, still
// without a history entry.
(() => {
  const phone = window.matchMedia("(max-width: 700px)");
  let pending = null;
  document.addEventListener("click", async (e) => {
    const a = e.target.closest && e.target.closest(".proj-tabs a[href]");
    if (!a || !phone.matches || e.defaultPrevented || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
    e.preventDefault();
    const url = a.href;
    const bar = a.parentElement;
    for (const t of bar.querySelectorAll("a")) {
      t.classList.toggle("on", t === a);
      if (t === a) t.setAttribute("aria-current", "page");
      else t.removeAttribute("aria-current");
    }
    if (pending) pending.abort();
    pending = new AbortController();
    try {
      const res = await fetch(url, { signal: pending.signal, credentials: "same-origin", headers: { Accept: "text/html" } });
      if (!res.ok || res.redirected) throw new Error(String(res.status));
      const doc = new DOMParser().parseFromString(await res.text(), "text/html");
      const next = doc.getElementById("main");
      const main = document.getElementById("main");
      if (!next || !main) throw new Error("no content");
      const left = bar.scrollLeft;
      main.replaceChildren(...Array.from(next.childNodes, (n) => document.importNode(n, true)));
      document.title = doc.title;
      history.replaceState(history.state, "", url);
      const tabs = main.querySelector(".proj-tabs");
      if (tabs) {
        tabs.scrollLeft = left;
        if (window.scrollY > tabs.offsetTop) window.scrollTo({ top: Math.max(0, tabs.offsetTop - 8) });
      }
      document.dispatchEvent(new CustomEvent("af:content", { detail: main }));
      document.dispatchEvent(new CustomEvent("af:charts"));
    } catch (err) {
      if (err.name !== "AbortError") location.replace(url);
    }
  });
})();

// Forms marked data-autosubmit save as soon as a switch in them changes (the owner, 2026-11-17:
// toggles save at once, no Save button; the button stays for pages without scripts).
document.addEventListener("change", (e) => {
  const form = e.target.closest("form[data-autosubmit]");
  if (form && e.target.matches("input[type=checkbox]")) form.requestSubmit();
});

// The bell (the owner, 2026-11-17: "like youtube, so when the user opens the notifications on the
// desktop or phone, they are all marked as read and the badge goes away"): a panel under it (a
// sheet on the phone). Opening it marks everything read and removes the badge; what was new keeps
// its dot until it closes. Without scripts the bell is a link to /notifications.
(() => {
  let panel = null;
  const close = () => {
    if (!panel) return;
    panel.classList.remove("open");
    const p = panel;
    panel = null;
    setTimeout(() => p.remove(), 160);
    document.removeEventListener("keydown", onKey, true);
    document.removeEventListener("click", outside, true);
    const bell = document.querySelector("[data-bell]");
    if (bell) bell.setAttribute("aria-expanded", "false");
  };
  const onKey = (e) => { if (e.key === "Escape") { e.preventDefault(); close(); } };
  const outside = (e) => {
    if (panel && !e.target.closest(".ntf-panel") && !e.target.closest("[data-bell]")) close();
  };
  const open = async (bell) => {
    const res = await fetch("/notifications/panel", { credentials: "same-origin" });
    if (!res.ok) { location.href = bell.href; return; }
    const html = await res.text();
    panel = document.createElement("div");
    panel.className = "ntf-back";
    panel.innerHTML = `<div class="ntf-panel" role="dialog" aria-modal="false" aria-label="Notifications">
      <div class="ntf-head"><strong>Notifications</strong><span class="grow"></span><a class="icon-only" href="/settings?s=notifications" aria-label="Notification settings" data-settings></a><button type="button" class="icon-only ntf-x" aria-label="Close">×</button></div>
      <div class="ntf-list"></div></div>`;
    panel.querySelector(".ntf-list").innerHTML = html;
    panel.querySelector('a.icon-only').innerHTML = document.querySelector('a[href="/settings"].icon-btn')?.innerHTML || "⚙";
    panel.querySelector(".ntf-x").addEventListener("click", close);
    panel.addEventListener("click", (e) => { if (e.target === panel) close(); });
    document.body.append(panel);
    requestAnimationFrame(() => panel && panel.classList.add("open"));
    bell.setAttribute("aria-expanded", "true");
    document.addEventListener("keydown", onKey, true);
    document.addEventListener("click", outside, true);
    // Read: the badge goes now (here and on the phone's bell).
    for (const b of document.querySelectorAll(".bell-badge")) b.remove();
    for (const b of document.querySelectorAll("[data-bell]")) b.setAttribute("aria-label", "Notifications");
    fetch("/notifications/read", { method: "POST", credentials: "same-origin", headers: { Accept: "application/json" } }).catch(() => {});
  };
  document.addEventListener("click", (e) => {
    const bell = e.target.closest("[data-bell]");
    if (!bell || e.metaKey || e.ctrlKey || e.shiftKey || e.button) return;
    e.preventDefault();
    if (panel) close(); else open(bell);
  });
})();

// Settings as a dialog over the page (the owner, 2026-11-17: "the settings panel should be a
// dialog, not its own window"): the cog (and every link marked data-settings) opens it; sections,
// switches and forms work inside it; ✕, Esc or a click beside it closes it, and the page stays as
// it was. It shows the /settings page's own content, so it never differs from it; without scripts
// the cog is a link to that page.
(() => {
  let back = null;
  let opener = null;
  const shellOf = (html) => {
    const doc = new DOMParser().parseFromString(html, "text/html");
    return doc.querySelector(".set-shell");
  };
  const show = (shell) => {
    const body = back.querySelector(".sdlg-body");
    body.replaceChildren(shell);
    shell.querySelector(".set-pane")?.scrollTo(0, 0);
    document.dispatchEvent(new CustomEvent("af:content", { detail: shell }));
  };
  const load = async (url, init, soft) => {
    back.classList.add("busy");
    try {
      const res = await fetch(url, { credentials: "same-origin", ...init });
      const shell = shellOf(await res.text());
      if (!shell) throw new Error("no settings in the answer");
      show(shell);
    } catch (_) {
      if (soft) close(); else location.href = typeof url === "string" ? url : "/settings";
    } finally {
      back && back.classList.remove("busy");
    }
  };
  const close = () => {
    if (!back) return;
    back.classList.remove("open");
    const b = back;
    back = null;
    document.removeEventListener("keydown", onKey, true);
    document.documentElement.classList.remove("no-scroll");
    setTimeout(() => b.remove(), 160);
    if (opener && opener.focus) opener.focus();
  };
  const onKey = (e) => {
    if (e.key === "Escape" && !document.querySelector(".cdlg-back")) { e.preventDefault(); close(); }
  };
  const open = (url, soft) => {
    opener = document.activeElement;
    back = document.createElement("div");
    back.className = "sdlg-back";
    back.innerHTML = `<div class="sdlg" role="dialog" aria-modal="true" aria-label="Settings">
      <div class="sdlg-head"><h2>Settings</h2><button type="button" class="icon-only sdlg-x" aria-label="Close">×</button></div>
      <div class="sdlg-body"><p class="sub sdlg-wait">Loading…</p></div></div>`;
    back.querySelector(".sdlg-x").addEventListener("click", close);
    back.addEventListener("mousedown", (e) => { if (e.target === back) close(); });
    // Inside: section links load in place; forms post and show the section they come back to.
    back.addEventListener("click", (e) => {
      const a = e.target.closest("a[href]");
      if (!a || e.defaultPrevented || e.metaKey || e.ctrlKey) return;
      const u = new URL(a.href, location.href);
      if (u.origin === location.origin && u.pathname === "/settings") { e.preventDefault(); load(u.pathname + u.search); }
      else if (u.origin === location.origin && !a.target) close();
    });
    back.addEventListener("submit", (e) => {
      const form = e.target;
      if (!form.matches("form[method=post], form[method=POST]")) return;
      e.preventDefault();
      load(form.action, { method: "POST", body: new URLSearchParams(new FormData(form, e.submitter)), headers: { "Content-Type": "application/x-www-form-urlencoded" } });
    });
    document.addEventListener("keydown", onKey, true);
    document.body.append(back);
    document.documentElement.classList.add("no-scroll");
    requestAnimationFrame(() => back && back.classList.add("open"));
    back.querySelector(".sdlg-x").focus();
    load(url, undefined, soft);
  };
  // A direct link to Settings: the server sent the dashboard; Settings opens over it.
  const o = document.body.dataset;
  if (o.openDlg === "settings" && o.openUrl && o.openFrom) {
    history.replaceState(null, "", o.openFrom);
    open(o.openUrl, true);
  }
  document.addEventListener("click", (e) => {
    const a = e.target.closest("a[data-settings]");
    if (!a || e.metaKey || e.ctrlKey || e.shiftKey || e.button) return;
    // Already on the settings page: it is the settings.
    if (location.pathname === "/settings") return;
    e.preventDefault();
    // From the bell's panel: that closes first.
    document.querySelector(".ntf-back .ntf-x")?.click();
    const u = new URL(a.href, location.href);
    if (back) load(u.pathname + u.search); else open(u.pathname + u.search);
  });
})();

// Pages that belong in a dialog (the owner, 2026-11-17: "make sure this is also a dialog"): a link
// or form marked data-dlg opens its page's [data-dlg-content] over the current page; forms inside
// it stay inside it, and an answer that isn't such a page (e.g. the new project's page) is opened
// as a page. Without scripts, they're pages as before.
(() => {
  let back = null;
  const close = () => {
    if (!back) return;
    // An editor with unsaved edits asks first (the project document).
    if (window.afDocGuard && back.querySelector("form[data-doc-editor]") && !window.afDocGuard()) return;
    const ed = back.querySelector("form[data-doc-editor] textarea[name=text]");
    if (ed) ed.defaultValue = ed.value;
    back.classList.remove("open");
    const b = back;
    back = null;
    document.documentElement.classList.remove("no-scroll");
    document.removeEventListener("keydown", onKey, true);
    setTimeout(() => b.remove(), 160);
  };
  const onKey = (e) => { if (e.key === "Escape" && !document.querySelector(".cdlg-back")) { e.preventDefault(); close(); } };
  const show = async (url, init, soft) => {
    let res;
    try {
      res = await fetch(url, { credentials: "same-origin", ...init });
    } catch (_) {
      if (!soft) location.href = url;
      return;
    }
    const doc = new DOMParser().parseFromString(await res.text(), "text/html");
    const content = doc.querySelector("[data-dlg-content]");
    if (!content) { if (!soft) location.href = res.url; return; }
    const h1 = content.querySelector(".head h1");
    const title = h1 ? h1.textContent : document.title;
    if (h1) h1.closest(".head").querySelector("h1").remove();
    if (!back) {
      back = document.createElement("div");
      back.className = "sdlg-back pdlg-back";
      back.innerHTML = `<div class="sdlg pdlg" role="dialog" aria-modal="true" aria-labelledby="pdlg-t">
        <div class="sdlg-head"><h2 id="pdlg-t"></h2><button type="button" class="icon-only sdlg-x" aria-label="Close">×</button></div>
        <div class="pdlg-body"></div></div>`;
      back.querySelector(".sdlg-x").addEventListener("click", close);
      back.addEventListener("mousedown", (e) => { if (e.target === back) close(); });
      back.addEventListener("submit", (e) => {
        const form = e.target;
        if ((form.getAttribute("method") || "").toLowerCase() !== "post") return;
        e.preventDefault();
        show(form.action, { method: "POST", body: new URLSearchParams(new FormData(form, e.submitter)), headers: { "Content-Type": "application/x-www-form-urlencoded" } });
      });
      document.body.append(back);
      document.documentElement.classList.add("no-scroll");
      document.addEventListener("keydown", onKey, true);
      requestAnimationFrame(() => back && back.classList.add("open"));
    }
    back.querySelector("#pdlg-t").textContent = title;
    const body = back.querySelector(".pdlg-body");
    body.replaceChildren(content);
    body.scrollTo(0, 0);
    document.dispatchEvent(new CustomEvent("af:content", { detail: content }));
    back.querySelector(".sdlg-x").focus();
  };
  document.addEventListener("click", (e) => {
    const a = e.target.closest("a[data-dlg]");
    if (!a || e.metaKey || e.ctrlKey || e.shiftKey || e.button) return;
    e.preventDefault();
    show(a.href);
  });
  document.addEventListener("submit", (e) => {
    const form = e.target.closest("form[data-dlg]");
    if (!form || back) return;
    e.preventDefault();
    show(form.action, { method: "POST", body: new URLSearchParams(new FormData(form, e.submitter)), headers: { "Content-Type": "application/x-www-form-urlencoded" } });
  });
  // A direct link to such a page (Add project): the server sent the page it opens over.
  const o = document.body.dataset;
  if (o.openDlg === "dlg" && o.openUrl && o.openFrom) {
    history.replaceState(null, "", o.openFrom);
    show(o.openUrl, undefined, true);
  }
})();

// The board on the phone: one column at a time, picked with the chips above it.
document.addEventListener("click", (e) => {
  const chip = e.target.closest(".bchips button[data-col]");
  if (!chip) return;
  const i = chip.dataset.col;
  for (const b of chip.parentElement.querySelectorAll("button")) {
    const on = b === chip;
    b.classList.toggle("on", on);
    b.setAttribute("aria-selected", on ? "true" : "false");
  }
  const board = chip.parentElement.nextElementSibling;
  if (board) [...board.children].forEach((c, n) => c.classList.toggle("on", String(n) === i));
});

// Settings lists: "Add a builder" / "Add a client" in the header opens the list's last row.
document.addEventListener("click", (e) => {
  const b = e.target.closest("[data-open]");
  if (!b) return;
  const d = document.getElementById(b.dataset.open);
  if (!d) return;
  d.open = true;
  d.scrollIntoView({ block: "nearest", behavior: "smooth" });
});

// Project overview (D-198): its side panels, Merge order and Release are open on the desktop and
// can't be folded there; on the phone they start folded, as rows that open.
(() => {
  const phone = window.matchMedia("(max-width: 700px)");
  const wire = (root) => {
    for (const d of root.querySelectorAll("details.ov-fold")) d.open = !phone.matches;
  };
  document.addEventListener("DOMContentLoaded", () => wire(document));
  document.addEventListener("af:content", (e) => e.detail && e.detail.querySelectorAll && wire(e.detail));
  document.addEventListener("click", (e) => {
    const s = e.target.closest && e.target.closest("details.ov-fold > summary");
    if (s && !phone.matches && !e.target.closest("a, button")) e.preventDefault();
  });
})();

// Filter menus (the Log, the Notes tab): one open at a time, closed by Esc or a click beside them;
// "Find a category" narrows a long list as you type.
(() => {
  const closeAll = (except) => {
    for (const d of document.querySelectorAll("details.dd[open]")) if (d !== except) d.open = false;
  };
  document.addEventListener("toggle", (e) => {
    const d = e.target;
    if (!(d instanceof HTMLDetailsElement) || !d.matches("details.dd") || !d.open) return;
    closeAll(d);
    d.querySelector("[data-find]")?.focus();
  }, true);
  document.addEventListener("click", (e) => { if (!e.target.closest("details.dd")) closeAll(null); });
  document.addEventListener("keydown", (e) => {
    if (e.key !== "Escape") return;
    const open = document.querySelector("details.dd[open]");
    if (!open) return;
    e.preventDefault();
    open.open = false;
    open.querySelector("summary")?.focus();
  });
  document.addEventListener("input", (e) => {
    const f = e.target.closest("[data-find]");
    if (!f) return;
    const q = f.value.trim().toLowerCase();
    for (const a of f.closest(".dd-menu").querySelectorAll("a[data-name]")) a.hidden = q !== "" && !a.dataset.name.toLowerCase().includes(q);
  });
  // Enter in the find box opens the first category left.
  document.addEventListener("keydown", (e) => {
    const f = e.target.closest && e.target.closest("[data-find]");
    if (!f || e.key !== "Enter") return;
    e.preventDefault();
    const first = [...f.closest(".dd-menu").querySelectorAll("a[data-name]")].find((a) => !a.hidden);
    if (first) location.href = first.href;
  });
})();

// The project document (the owner, 2026-11-18): contents that follow the reading, and the editor:
// the whole document as Markdown with a preview beside it (Markdown or Preview on the phone),
// Ctrl+S saves, closing asks before dropping edits, and a newer version saved meanwhile is said.
(() => {
  // Contents: a click scrolls inside the dialog; the section being read is marked.
  document.addEventListener("click", (e) => {
    const a = e.target.closest(".pd-toc a[href^='#']");
    if (!a) return;
    const t = document.getElementById(a.getAttribute("href").slice(1));
    if (!t) return;
    e.preventDefault();
    t.scrollIntoView({ behavior: "smooth", block: "start" });
  });
  document.addEventListener("scroll", (e) => {
    const box = e.target;
    if (!(box instanceof Element)) return;
    const toc = box.querySelector(".pd-toc");
    if (!toc) return;
    const top = box.getBoundingClientRect().top + 40;
    let cur = null;
    for (const s of box.querySelectorAll(".pd-doc section[id]")) if (s.getBoundingClientRect().top <= top) cur = s.id;
    for (const a of toc.querySelectorAll("a")) a.classList.toggle("on", a.getAttribute("href") === "#" + (cur || toc.querySelector("a")?.getAttribute("href").slice(1)));
  }, true);

  const dirty = (form) => {
    const ta = form && form.querySelector("textarea[name=text]");
    return !!ta && ta.value !== ta.defaultValue;
  };
  const ask = () => confirm("Drop your edits to the project document?");
  window.afDocGuard = () => {
    const f = document.querySelector("form[data-doc-editor]");
    return !dirty(f) || ask();
  };
  // Cancel and links out of the editor ask first.
  document.addEventListener("click", (e) => {
    const a = e.target.closest("a[data-cancel]");
    if (!a || !dirty(a.closest("form"))) return;
    if (!ask()) { e.preventDefault(); e.stopImmediatePropagation(); }
    else a.closest("form").querySelector("textarea[name=text]").defaultValue = a.closest("form").querySelector("textarea[name=text]").value;
  }, true);
  window.addEventListener("beforeunload", (e) => {
    if (dirty(document.querySelector("form[data-doc-editor]"))) { e.preventDefault(); e.returnValue = ""; }
  });

  // Section helpers for "Use its version": the text split at `## ` headings outside code.
  const split = (text) => {
    const out = [{ title: "Introduction", lines: [] }];
    let code = false;
    for (const line of text.split("\n")) {
      if (line.trimStart().startsWith("```")) code = !code;
      if (!code && line.startsWith("## ")) out.push({ title: line.slice(3).trim(), lines: [] });
      out[out.length - 1].lines.push(line);
    }
    return out.filter((s, i) => i > 0 || s.lines.join("").trim() !== "");
  };

  const wire = (root) => {
    const form = root.querySelector && root.querySelector("form[data-doc-editor]");
    if (!form || form.dataset.wired) return;
    form.dataset.wired = "1";
    const ta = form.querySelector("textarea[name=text]");
    const prev = form.querySelector("[data-preview]");
    const flag = form.closest("[data-dlg-content], main")?.querySelector("[data-dirty]");
    const slug = form.dataset.slug;
    let timer = null;
    let seq = 0;
    const render = async () => {
      const n = ++seq;
      try {
        const res = await fetch(`/p/${encodeURIComponent(slug)}/doc/preview`, {
          method: "POST", credentials: "same-origin",
          headers: { "Content-Type": "application/x-www-form-urlencoded" },
          body: new URLSearchParams({ text: ta.value }),
        });
        if (res.ok && n === seq) prev.innerHTML = await res.text(); // rendered and sanitised by the server
      } catch (_) { /* the preview waits for the next edit */ }
    };
    ta.addEventListener("input", () => {
      if (flag) flag.hidden = !dirty(form);
      clearTimeout(timer);
      timer = setTimeout(render, 250);
    });
    ta.addEventListener("keydown", (e) => {
      if ((e.ctrlKey || e.metaKey) && (e.key === "s" || e.key === "Enter")) { e.preventDefault(); form.requestSubmit(); }
    });
    form.addEventListener("submit", () => { ta.defaultValue = ta.value; });
    // Formatting buttons wrap the selection or start a line.
    form.addEventListener("click", (e) => {
      const b = e.target.closest("button[data-md]");
      if (!b) return;
      const [s, t] = [ta.selectionStart, ta.selectionEnd];
      const sel = ta.value.slice(s, t);
      const lineStart = ta.value.lastIndexOf("\n", s - 1) + 1;
      const wrap = (l, r, ph) => ta.setRangeText(l + (sel || ph) + r, s, t, sel ? "end" : "select");
      const lead = (p) => ta.setRangeText(p, lineStart, lineStart, "end");
      switch (b.dataset.md) {
        case "h": lead("## "); break;
        case "b": wrap("**", "**", "bold"); break;
        case "i": wrap("_", "_", "italic"); break;
        case "c": wrap("`", "`", "code"); break;
        case "l": lead("- "); break;
        case "a": wrap("[", "](https://)", "text"); break;
      }
      ta.focus();
      ta.dispatchEvent(new Event("input"));
    });
    // Phone: Markdown or Preview.
    form.addEventListener("click", (e) => {
      const b = e.target.closest(".pd-seg button[data-pane]");
      if (!b) return;
      form.querySelector(".pd-panes").dataset.show = b.dataset.pane;
      for (const x of b.parentElement.children) { x.classList.toggle("on", x === b); x.setAttribute("aria-selected", String(x === b)); }
      if (b.dataset.pane === "prev") render();
    });
    // Both changed a section: take the agent's version into the editor.
    form.addEventListener("click", (e) => {
      const b = e.target.closest("button[data-use-theirs]");
      if (!b) return;
      const title = b.dataset.useTheirs;
      const theirs = [...form.querySelectorAll("textarea[data-theirs]")].find((x) => x.dataset.theirs === title).value;
      const parts = split(ta.value);
      const i = parts.findIndex((p) => p.title.toLowerCase() === title.toLowerCase());
      const block = theirs.trimEnd().split("\n").concat([""]);
      if (i >= 0) parts[i].lines = block; else parts.push({ title, lines: block });
      ta.value = parts.map((p) => p.lines.join("\n").replace(/\n+$/, "")).join("\n\n") + "\n";
      ta.dispatchEvent(new Event("input"));
      b.disabled = true;
      b.textContent = "Its version is in the editor";
    });
    // A newer version saved while editing: say so (it's merged on Save).
    const warn = form.querySelector("[data-live-warn]");
    const base = Number(form.dataset.base);
    const poll = setInterval(async () => {
      if (!form.isConnected) { clearInterval(poll); return; }
      try {
        const res = await fetch(`/p/${encodeURIComponent(slug)}/doc/rev?since=${base}`, { credentials: "same-origin" });
        const d = await res.json();
        if (d.rev > base && warn) {
          warn.querySelector("[data-live-text]").textContent = `${d.author} saved rev ${d.rev} while you were editing; it changed ${d.changed || "the document"}. Saving keeps your edits and takes its changes to the sections you didn't touch; where you both changed a section, you pick.`;
          warn.hidden = false;
        }
      } catch (_) { /* try again next time */ }
    }, 20000);
    if (!form.querySelector(".pd-warn:not([data-live-warn])")) ta.focus();
  };
  document.addEventListener("DOMContentLoaded", () => wire(document));
  document.addEventListener("af:content", (e) => e.detail && wire(e.detail));
})();
