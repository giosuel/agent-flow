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


// One dialog (the UI library, D-245; canvas board 5): every dialog is a .dlg (head, body, foot) in
// one of its sizes over one scrim, and the bell's panel is a .pop; on the phone both are sheets
// from below that a pull on the handle or head closes. One controller keeps the open ones as a
// stack (N-18 #18, N-19 #38): Esc and a click on the scrim close the top one only (a confirm over
// a record closes just the confirm), focus moves into it, stays there and goes back to what opened
// it, and the page behind doesn't scroll.
// afDlg.open({ size, title, label, cls, pop, anchor, opener, role, dismiss, closed }) gives { el, head,
// title, tools, body, close, dismiss }. `dismiss` is what Esc, the close button, the scrim and a
// pull do: by default close(); a dialog can ask first or go Back instead. Either way, what the owner
// typed or picked in it and hasn't sent is never dropped without asking (afDlg.leave).
window.afDlg = (() => {
  const stack = [];
  const FOCUS = "a[href], button:not([disabled]), input:not([disabled]):not([type=hidden]), select:not([disabled]), textarea:not([disabled]), summary, [tabindex]:not([tabindex='-1'])";
  const phone = matchMedia("(max-width: 700px)");
  let n = 0;
  const top = () => stack[stack.length - 1];
  // A field of a form that posts, changed from what the page came with: a written answer, a pasted
  // token, a project's settings. Unnamed fields are the page's own (filters, finds) and aren't sent.
  const edited = (x) => {
    if (!x.name || x.disabled) return false;
    if (x.type === "checkbox" || x.type === "radio") return x.checked !== x.defaultChecked;
    if (x.tagName === "SELECT") {
      const o = [...x.options];
      if (x.multiple) return o.some((y) => y.selected !== y.defaultSelected);
      return x.selectedIndex !== Math.max(0, o.map((y) => y.defaultSelected).lastIndexOf(true));
    }
    if (x.tagName === "TEXTAREA" || (x.tagName === "INPUT" && !/^(hidden|submit|button|reset|image)$/.test(x.type))) return x.value !== x.defaultValue;
    return false;
  };
  const changed = (root) => [...root.querySelectorAll("form")]
    .some((f) => (f.getAttribute("method") || "").toLowerCase() === "post" && [...f.elements].some(edited));
  // Before a dialog closes or shows something else: true when nothing would be lost, else a promise
  // of the owner's choice in the app's own dialog (the project document asks in its own words).
  const leave = (d) => {
    if (!changed(d.body)) return true;
    if (d.el.querySelector("form[data-md-editor]") && window.afDocGuard) return window.afDocGuard();
    return window.afDialog({ title: "Drop what you entered?", text: "It isn't saved or sent yet.", ok: "Drop it", cancel: "Keep editing", danger: true });
  };
  const lock = () => document.documentElement.classList.toggle("dlg-on", stack.some((d) => !d.pop));
  document.addEventListener("keydown", (e) => {
    const d = top();
    if (!d) return;
    if (e.key === "Escape") {
      // A menu open inside it closes first (the menus' own Esc).
      if (d.el.querySelector("details.dd[open], details[data-menu][open]")) return;
      e.preventDefault();
      e.stopImmediatePropagation();
      d.dismiss();
    } else if (e.key === "Tab" && !d.pop) {
      const f = [...d.el.querySelectorAll(FOCUS)].filter((x) => x.getClientRects().length);
      if (!f.length) return;
      const i = f.indexOf(document.activeElement);
      if (e.shiftKey ? i <= 0 : i < 0 || i === f.length - 1) {
        e.preventDefault();
        f[e.shiftKey ? f.length - 1 : 0].focus();
      }
    }
  }, true);
  // A popover closes on a click anywhere else (its own button toggles it).
  document.addEventListener("click", (e) => {
    const d = top();
    if (d && d.pop && !d.el.contains(e.target) && !(d.anchor && d.anchor.contains(e.target))) d.dismiss();
  }, true);
  const open = ({ size = "md", title = "", label = "", cls = "", pop = false, anchor = null, opener = null, role = "dialog", dismiss = null, closed = null } = {}) => {
    const id = "dlg-t" + ++n;
    const back = document.createElement("div");
    back.className = pop ? "pop-back" : "dlg-back";
    back.innerHTML = `<div class="${pop ? "pop" : "dlg " + size}${cls ? " " + cls : ""}" role="${role}" aria-modal="${!pop}" aria-labelledby="${id}">
      <div class="dlg-grab" aria-hidden="true"></div>
      <div class="dlg-head"><div class="dlg-t" id="${id}" role="heading" aria-level="2"></div><span class="dlg-tools"></span><button type="button" class="ibtn${pop ? " flat" : ""} dlg-x" aria-label="Close">${icon("close")}</button></div>
      <div class="dlg-body"></div></div>`;
    const el = back.firstElementChild;
    const d = {
      back, el, pop, anchor,
      head: el.querySelector(".dlg-head"), title: el.querySelector(".dlg-t"),
      tools: el.querySelector(".dlg-tools"), body: el.querySelector(".dlg-body"),
      opener: opener || document.activeElement,
    };
    d.title.textContent = title;
    if (label) { el.setAttribute("aria-label", label); el.removeAttribute("aria-labelledby"); }
    d.close = () => {
      const i = stack.indexOf(d);
      if (i < 0) return;
      stack.splice(i, 1);
      lock();
      // Focus goes back to what opened it, unless it already went somewhere else on the page.
      const a = document.activeElement;
      if ((!a || a === document.body || el.contains(a)) && d.opener && document.contains(d.opener)) d.opener.focus({ preventScroll: true });
      back.classList.remove("open");
      setTimeout(() => back.remove(), 260);
      if (closed) closed(d);
    };
    // Asked once at a time: a second Esc or tap while it asks is the question's own.
    let asking = false;
    d.dismiss = () => {
      const go = () => { if (stack.includes(d)) (dismiss ? dismiss(d) : d.close()); };
      const ok = leave(d);
      if (ok === true) return go();
      if (asking) return;
      asking = true;
      ok.then((yes) => { asking = false; if (yes) go(); });
    };
    el.querySelector(".dlg-x").addEventListener("click", () => d.dismiss());
    // The scrim: a press that starts and ends on it (not a text selection that ends there).
    let down = false;
    back.addEventListener("mousedown", (e) => { down = e.target === back; });
    back.addEventListener("click", (e) => { if (down && e.target === back && !pop) d.dismiss(); });
    // The phone's sheet: a pull down on its handle or head.
    let y0 = null;
    el.addEventListener("touchstart", (e) => {
      if (phone.matches && e.target.closest(".dlg-grab, .dlg-head") && !e.target.closest("button, a, input")) y0 = e.touches[0].clientY;
    }, { passive: true });
    el.addEventListener("touchmove", (e) => {
      if (y0 !== null) el.style.transform = `translateY(${Math.max(0, e.touches[0].clientY - y0)}px)`;
    }, { passive: true });
    el.addEventListener("touchend", (e) => {
      if (y0 === null) return;
      const dy = e.changedTouches[0].clientY - y0;
      y0 = null;
      el.style.transform = "";
      if (dy > 110) d.dismiss();
    });
    // A popover sits under its button on the desktop.
    if (pop && anchor && !phone.matches) {
      const r = anchor.getBoundingClientRect();
      el.style.top = `${Math.round(r.bottom + 8)}px`;
      el.style.right = `${Math.max(16, Math.round(innerWidth - r.right))}px`;
    }
    back.style.zIndex = String(60 + stack.length * 2);
    stack.push(d);
    document.body.append(back);
    lock();
    requestAnimationFrame(() => back.classList.add("open"));
    if (!pop) d.el.querySelector(".dlg-x").focus({ preventScroll: true });
    return d;
  };
  return { open, top, leave: async (d) => leave(d), closePop: () => { const d = top(); if (d && d.pop) d.close(); } };
})();

// Confirms and messages in that dialog, never the browser's (the owner, 2026-11-17: "remove all
// the simple alerts and replace them with actual styled dialogs that fit into the design").
// window.afDialog({title, text, body, size, ok, cancel, danger}) resolves true for OK (body: nodes
// shown instead of the text, such as a button's confirmation, D-263); without cancel it's a
// message with one button. The safe choice has focus, so Enter never removes something by accident.
window.afDialog = ({ title, text = "", body = null, size = "sm", ok = "OK", cancel = null, danger = false }) =>
  new Promise((resolve) => {
    let yes = false;
    const d = window.afDlg.open({ size, title, cls: "cdlg", role: cancel ? "alertdialog" : "dialog", closed: () => resolve(yes) });
    const p = body ? null : document.createElement("p");
    if (p) { p.textContent = text; d.body.append(p); } else d.body.append(body);
    const lead = p || d.body.querySelector("p");
    d.body.hidden = !(body || text);
    if (lead && (body || text)) { lead.id = d.title.id + "-x"; d.el.setAttribute("aria-describedby", lead.id); }
    const foot = document.createElement("div");
    foot.className = "dlg-foot";
    foot.innerHTML = `${cancel ? '<button type="button" class="btn quiet" data-v="0"></button>' : ""}<button type="button" class="btn${danger ? " danger-solid" : ""}" data-v="1"></button>`;
    foot.querySelector('[data-v="1"]').textContent = ok;
    if (cancel) foot.querySelector('[data-v="0"]').textContent = cancel;
    foot.addEventListener("click", (e) => {
      const b = e.target.closest("button[data-v]");
      if (!b) return;
      yes = b.dataset.v === "1";
      d.close();
    });
    d.el.append(foot);
    foot.querySelector(cancel ? '[data-v="0"]' : '[data-v="1"]').focus();
  });

// A form posted from a dialog that didn't come back as the page it shows (refused, a server error,
// AgentFlow restarting or out of reach): the dialog and what was typed in it stay, and the reason
// from the error page is said, never a blank page instead (04-6, 04-7).
window.afRefused = (html, status) => {
  const doc = html ? new DOMParser().parseFromString(html, "text/html") : null;
  const why = doc && doc.querySelector(".bare-in h1 + .sub");
  const text = why && why.textContent.trim()
    ? why.textContent.trim()
    : status ? `AgentFlow answered ${status}. What you entered is still here; try again.` : "AgentFlow didn't answer. What you entered is still here; try again.";
  return window.afDialog({ title: "Couldn't save", text });
};

// Buttons with data-confirm ask first (no inline handlers: the CSP forbids them). The question's
// first sentence is the title, the rest the text; the button's own label confirms, red when the
// button is (.danger) or says so (data-danger), never guessed from its words (N-19 #34). With
// data-confirm-body, the question is the title and the <template> right after the button is the
// text (a button's command, D-263).
document.addEventListener("click", (e) => {
  const el = e.target.closest("[data-confirm]");
  if (!el || el.dataset.confirmed) return;
  e.preventDefault();
  e.stopImmediatePropagation();
  const q = el.dataset.confirm;
  const tpl = el.hasAttribute("data-confirm-body") && el.nextElementSibling;
  const body = tpl && tpl.matches("template.confirm-body") ? tpl.content.cloneNode(true) : null;
  const cut = body ? -1 : q.search(/[?.]\s/);
  const title = cut < 0 ? q : q.slice(0, cut + 1);
  const text = cut < 0 ? "" : q.slice(cut + 2);
  const label = (el.dataset.confirmOk || el.textContent || el.getAttribute("aria-label") || "OK").trim() || "OK";
  const danger = el.classList.contains("danger") || el.dataset.danger !== undefined;
  window.afDialog({ title, text, body, size: body ? "md" : "sm", ok: label, cancel: "Cancel", danger }).then((yes) => {
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
// Icons come from icons.rs through the page's sprite (base.html), never drawn here.
const icon = (name) => `<svg class="i" aria-hidden="true"><use href="#i-${name}"/></svg>`;
const COPY_ICON = icon("copy");
const DONE_ICON = icon("check");

// The one copy routine (T-113): an element's text or a string. Without the clipboard API (http),
// the text is selected and copied; a string goes through a hidden field for that.
async function copyText(what) {
  const text = typeof what === "string" ? what : what.textContent.trim();
  try {
    if (navigator.clipboard && window.isSecureContext) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch {
    // fall through to the selection
  }
  let el = what;
  let temp = null;
  if (typeof what === "string") {
    temp = document.createElement("textarea");
    temp.value = text;
    temp.setAttribute("readonly", "");
    temp.style.cssText = "position: fixed; top: 0; left: 0; opacity: 0";
    document.body.append(temp);
    el = temp;
  }
  const sel = window.getSelection();
  if (temp) temp.select();
  else {
    const range = document.createRange();
    range.selectNodeContents(el);
    sel.removeAllRanges();
    sel.addRange(range);
  }
  let ok = false;
  try {
    ok = document.execCommand("copy");
  } catch {
    ok = false;
  }
  sel.removeAllRanges();
  if (temp) temp.remove();
  return ok;
}
window.afCopy = copyText;

// "Copy the text" on a note's page: the text as it is (its Markdown, a log's lines), from the
// hidden field the page keeps it in.
document.addEventListener("click", async (e) => {
  const btn = e.target.closest("[data-copy-from]");
  const src = btn && document.getElementById(btn.dataset.copyFrom);
  if (!src) return;
  // A field holds the text; a code view (a job's output) has it in its lines.
  const text = "value" in src ? src.value : [...src.querySelectorAll(".cv-t")].map((t) => t.textContent).join("\n");
  const ok = await copyText(text);
  btn.innerHTML = ok ? DONE_ICON : COPY_ICON;
  btn.classList.toggle("done", ok);
  btn.setAttribute("aria-label", ok ? "Copied" : "Copy didn't work; download it instead");
  setTimeout(() => {
    btn.innerHTML = COPY_ICON;
    btn.classList.remove("done");
    btn.setAttribute("aria-label", "Copy the text");
  }, 1600);
});

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
    btn.className = "ibtn flat";
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
      sw.className = "swatch";
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
    // On the overview the link stays and is marked; on Plan the whole area was swapped and the new
    // one marks itself.
    if (document.contains(a)) {
      for (const m of document.querySelectorAll("a[data-ms]")) {
        const on = m === a;
        m.classList.toggle("sel", on);
        if (on) m.setAttribute("aria-current", "true"); else m.removeAttribute("aria-current");
      }
    }
    history.replaceState(history.state, "", a.href.replace(/#.*$/, ""));
    document.dispatchEvent(new CustomEvent("af:content", { detail: next }));
  } catch {
    location.href = a.href;
  }
});

// Plan (the owner, 2026-11-18, D-219; 2026-11-19, D-226): the milestone list scrolls to the picked
// milestone, the one before it just above; on the phone there's no scroll box, the done ones and
// the step rail start folded (unless the picked one is among them).
// A fade on each side of the list's box with more to scroll to.
const fadeList = (box) => {
  box.classList.toggle("more-up", box.scrollTop > 2);
  box.classList.toggle("more-down", box.scrollTop + box.clientHeight < box.scrollHeight - 2);
};
const foldRail = (root) => {
  for (const box of root.querySelectorAll(".ml-plan [data-ml]")) {
    if (!box.dataset.fade) {
      box.dataset.fade = "1";
      box.addEventListener("scroll", () => fadeList(box), { passive: true });
    }
    const picked = box.querySelector(".ml-row.picked");
    if (picked && box.scrollHeight > box.clientHeight) {
      // The row before it, among the rows shown (the done ones sit in their own fold).
      const rows = [...box.querySelectorAll(".ml-row")].filter((r) => r.offsetHeight > 0);
      const before = rows[rows.indexOf(picked) - 1];
      box.scrollTop = Math.max(0, picked.offsetTop - (before ? before.offsetHeight : 0) - 4);
    }
    fadeList(box);
  }
  if (!window.matchMedia("(max-width: 700px)").matches) return;
  for (const d of root.querySelectorAll("details[data-srail]")) d.open = false;
  for (const d of root.querySelectorAll("details[data-ml-done]")) {
    if (!d.querySelector(".picked")) d.open = false;
  }
};
document.addEventListener("DOMContentLoaded", () => foldRail(document));
// Again once the styles are in: before that the list's box has no height to scroll in.
window.addEventListener("load", () => foldRail(document));
document.addEventListener("af:content", (e) => foldRail(e.detail));

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
// its dot until it closes. /notifications opened directly is the dashboard with the panel open.
(() => {
  let panel = null;
  const open = async (bell) => {
    const res = await fetch("/notifications/panel", { credentials: "same-origin" });
    if (!res.ok) { location.href = bell.href; return; }
    const html = await res.text();
    panel = window.afDlg.open({
      pop: true, anchor: bell, cls: "ntf-panel", title: "Notifications",
      closed: () => {
        panel = null;
        for (const b of document.querySelectorAll("[data-bell]")) b.setAttribute("aria-expanded", "false");
      },
    });
    panel.tools.innerHTML = `<a class="ibtn flat" href="/settings?s=notifications" aria-label="Notification settings" data-settings>${icon("gear")}</a>`;
    panel.body.innerHTML = html;
    bell.setAttribute("aria-expanded", "true");
    // Read: the badge goes now (here and on the phone's bell).
    for (const b of document.querySelectorAll(".bell-badge")) b.remove();
    for (const b of document.querySelectorAll("[data-bell]")) b.setAttribute("aria-label", "Notifications");
    fetch("/notifications/read", { method: "POST", credentials: "same-origin", headers: { Accept: "application/json" } }).catch(() => {});
  };
  // A direct link to /notifications: the server sent the dashboard; the panel opens over it.
  const o = document.body.dataset;
  if (o.openDlg === "ntf" && o.openFrom) {
    history.replaceState(null, "", o.openFrom);
    const bell = document.querySelector("[data-bell]");
    if (bell) open(bell);
  }
  document.addEventListener("click", (e) => {
    const bell = e.target.closest("[data-bell]");
    if (!bell || e.metaKey || e.ctrlKey || e.shiftKey || e.button) return;
    e.preventDefault();
    if (panel) panel.close(); else open(bell);
  });
})();

// Settings as a dialog over the page (the owner, 2026-11-17: "the settings panel should be a
// dialog, not its own window"): the cog (and every link marked data-settings) opens it; sections,
// switches and forms work inside it; ✕, Esc or a click beside it closes it, and the page stays as
// it was. It shows the /settings page's own content, so it never differs from it; without scripts
// the cog is a link to that page.
(() => {
  let d = null;
  const shellOf = (html) => {
    const doc = new DOMParser().parseFromString(html, "text/html");
    return doc.querySelector(".set-shell");
  };
  const show = (shell) => {
    d.body.replaceChildren(shell);
    shell.querySelector(".set-pane")?.scrollTo(0, 0);
    if (!d.el.contains(document.activeElement)) d.el.querySelector(".dlg-x").focus({ preventScroll: true });
    document.dispatchEvent(new CustomEvent("af:content", { detail: shell }));
  };
  // A form that comes back without the settings (a token GitHub refused, an error, no answer)
  // keeps the dialog and its fields and says why: its address only takes a POST, so going there
  // would be a blank 405 page.
  const load = async (url, init, soft) => {
    const box = d;
    const post = !!init && init.method === "POST";
    box.el.classList.add("busy");
    try {
      let res;
      try {
        res = await fetch(url, { credentials: "same-origin", ...init });
      } catch (e) {
        if (post) { window.afRefused(); return; }
        throw e;
      }
      const html = await res.text();
      const shell = shellOf(html);
      if (shell) { if (d === box) show(shell); return; }
      if (post) { window.afRefused(html, res.status); return; }
      throw new Error("no settings in the answer");
    } catch (_) {
      if (soft) box.close(); else location.href = typeof url === "string" ? url : "/settings";
    } finally {
      box.el.classList.remove("busy");
    }
  };
  const open = (url, soft) => {
    d = window.afDlg.open({ size: "lg", cls: "sdlg", title: "Settings", closed: () => { d = null; } });
    d.body.innerHTML = '<p class="sub sdlg-wait">Loading…</p>';
    // Inside: section links load in place; forms post and show the section they come back to.
    d.el.addEventListener("click", (e) => {
      const a = e.target.closest("a[href]");
      if (!a || e.defaultPrevented || e.metaKey || e.ctrlKey) return;
      const u = new URL(a.href, location.href);
      if (u.origin === location.origin && u.pathname === "/settings") {
        e.preventDefault();
        window.afDlg.leave(d).then((yes) => { if (yes) load(u.pathname + u.search); });
      }
      else if (u.origin === location.origin && !a.target) d.close();
    });
    d.el.addEventListener("submit", (e) => {
      const form = e.target;
      if (!form.matches("form[method=post], form[method=POST]")) return;
      e.preventDefault();
      load(form.action, { method: "POST", body: new URLSearchParams(new FormData(form, e.submitter)), headers: { "Content-Type": "application/x-www-form-urlencoded" } });
    });
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
    window.afDlg.closePop();
    const u = new URL(a.href, location.href);
    if (d) load(u.pathname + u.search); else open(u.pathname + u.search);
  });
})();

// Pages that belong in a dialog (the owner, 2026-11-17: "make sure this is also a dialog"): a link
// or form marked data-dlg opens its page's [data-dlg-content] over the current page; forms inside
// it stay inside it, and an answer that isn't such a page (e.g. the new project's page) is opened
// as a page. Without scripts, they're pages as before.
(() => {
  let d = null;
  const SIZES = ["sm", "md", "lg", "xl"];
  // Closing: afDlg has asked already when something typed in it isn't saved (the project document
  // in its own words); the editor's edits are then dropped, so leaving the page doesn't ask again.
  const saved = (box) => {
    for (const ed of box ? box.el.querySelectorAll("form[data-md-editor] textarea[name=text]") : []) ed.defaultValue = ed.value;
  };
  const dismiss = (box) => {
    saved(box);
    box.close();
  };
  // A form whose answer is neither such a page nor a success (refused, an error, no answer) stays
  // where it is with what was typed in it, and the reason is said (04-7: the document's edits were
  // lost on a save that failed).
  const show = async (url, init, soft) => {
    const post = !!init && init.method === "POST";
    let res, html;
    try {
      res = await fetch(url, { credentials: "same-origin", ...init });
      html = await res.text();
    } catch (_) {
      if (post) { window.afRefused(); return; }
      if (!soft) location.href = url;
      return;
    }
    const doc = new DOMParser().parseFromString(html, "text/html");
    const content = doc.querySelector("[data-dlg-content]");
    if (!content) {
      if (post && !res.ok) { window.afRefused(html, res.status); return; }
      if (post) saved(d);
      if (!soft) location.href = res.url;
      return;
    }
    const h1 = content.querySelector(".head h1");
    const title = h1 ? h1.textContent : document.title;
    if (h1) h1.remove();
    // Its size comes with the page: data-dlg-size (md when it doesn't say).
    const size = SIZES.includes(content.dataset.dlgSize) ? content.dataset.dlgSize : "md";
    if (!d) {
      d = window.afDlg.open({ size, cls: "pdlg", dismiss, closed: () => { d = null; } });
      // Before the form's own listeners (capture), so the editor sees the submit is the dialog's
      // and keeps its edits marked unsaved until the answer comes back.
      d.el.addEventListener("submit", (e) => {
        const form = e.target;
        if ((form.getAttribute("method") || "").toLowerCase() !== "post") return;
        e.preventDefault();
        show(form.action, { method: "POST", body: new URLSearchParams(new FormData(form, e.submitter)), headers: { "Content-Type": "application/x-www-form-urlencoded" } });
      }, true);
    } else {
      d.el.classList.remove(...SIZES);
      d.el.classList.add(size);
    }
    d.title.textContent = title;
    d.body.replaceChildren(content);
    d.body.scrollTo(0, 0);
    if (!d.el.contains(document.activeElement)) d.el.querySelector(".dlg-x").focus({ preventScroll: true });
    document.dispatchEvent(new CustomEvent("af:content", { detail: content }));
  };
  document.addEventListener("click", (e) => {
    const a = e.target.closest("a[data-dlg]");
    if (!a || e.metaKey || e.ctrlKey || e.shiftKey || e.button) return;
    e.preventDefault();
    show(a.href);
  });
  document.addEventListener("submit", (e) => {
    const form = e.target.closest("form[data-dlg]");
    if (!form || d) return;
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

// Menus, the UI library's one dropdown: the filters (details.dd) and the bar's project switcher and
// More (details[data-menu]). One open at a time, closed by Esc or a click beside it; "Find a
// category" narrows a long list as you type. (Two scripts did this before, N-19 #9.)
(() => {
  const MENU = "details.dd, details[data-menu]";
  const closeAll = (except) => {
    for (const d of document.querySelectorAll("details.dd[open], details[data-menu][open]")) if (d !== except) d.open = false;
  };
  document.addEventListener("toggle", (e) => {
    const d = e.target;
    if (!(d instanceof HTMLDetailsElement) || !d.matches(MENU) || !d.open) return;
    closeAll(d);
    d.querySelector("[data-find]")?.focus();
  }, true);
  document.addEventListener("click", (e) => { if (!e.target.closest(MENU)) closeAll(null); });
  document.addEventListener("keydown", (e) => {
    if (e.key !== "Escape") return;
    const open = document.querySelector("details.dd[open], details[data-menu][open]");
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
    // A dialog's body scrolls, or the page itself (the Handbook).
    const page = e.target === document;
    const box = page ? document.documentElement : e.target;
    if (!(box instanceof Element)) return;
    const toc = box.querySelector(".pd-toc");
    if (!toc) return;
    const top = page ? 90 : box.getBoundingClientRect().top + 40;
    let cur = null;
    for (const s of box.querySelectorAll(".pd-doc section[id]")) if (s.getBoundingClientRect().top <= top) cur = s.id;
    for (const a of toc.querySelectorAll("a")) a.classList.toggle("on", a.getAttribute("href") === "#" + (cur || toc.querySelector("a")?.getAttribute("href").slice(1)));
  }, true);

  const dirty = (form) => {
    const ta = form && form.querySelector("textarea[name=text]");
    return !!ta && ta.value !== ta.defaultValue;
  };
  // The app's dialog, never the browser's own (N-19 #38, T-114).
  const ask = () => window.afDialog({ title: "Drop your edits?", text: "What you changed in the document isn't saved.", ok: "Drop them", cancel: "Keep editing", danger: true });
  window.afDocGuard = async () => {
    const f = document.querySelector("form[data-md-editor]");
    return !dirty(f) || ask();
  };
  // Cancel asks first; once it's answered, the same click goes on.
  document.addEventListener("click", (e) => {
    const a = e.target.closest("a[data-cancel]");
    if (!a || a.dataset.confirmed || !dirty(a.closest("form"))) return;
    e.preventDefault();
    e.stopImmediatePropagation();
    ask().then((yes) => {
      if (!yes) return;
      const ta = a.closest("form").querySelector("textarea[name=text]");
      ta.defaultValue = ta.value;
      a.dataset.confirmed = "1";
      a.click();
      delete a.dataset.confirmed;
    });
  }, true);
  window.addEventListener("beforeunload", (e) => {
    if (dirty(document.querySelector("form[data-md-editor]"))) { e.preventDefault(); e.returnValue = ""; }
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
    const form = root.querySelector && root.querySelector("form[data-md-editor]");
    if (!form || form.dataset.wired) return;
    form.dataset.wired = "1";
    const ta = form.querySelector("textarea[name=text]");
    const prev = form.querySelector("[data-preview]");
    const flag = form.closest("[data-dlg-content], main")?.querySelector("[data-dirty]");
    const previewUrl = form.dataset.preview;
    const revUrl = form.dataset.rev;
    let timer = null;
    let seq = 0;
    const render = async () => {
      const n = ++seq;
      try {
        const res = await fetch(previewUrl, {
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
    // Saved once the form posts as a page (leaving it doesn't ask); a dialog that posts it in the
    // script (prevented here) keeps the edits unsaved until its answer comes back.
    form.addEventListener("submit", (e) => { if (!e.defaultPrevented) ta.defaultValue = ta.value; });
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
    const poll = revUrl && setInterval(async () => {
      if (!form.isConnected) { clearInterval(poll); return; }
      try {
        const res = await fetch(`${revUrl}${revUrl.includes("?") ? "&" : "?"}since=${base}`, { credentials: "same-origin" });
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

// Conversations: ← → step between conversations (or runs); on the phone, a swipe does.
(() => {
  const go = (sel) => { const a = document.querySelector(sel); if (a) location.href = a.href; };
  document.addEventListener("keydown", (e) => {
    if (!document.querySelector(".cv-pager") || e.altKey || e.ctrlKey || e.metaKey) return;
    // A dialog over the page (a record, Settings, the bell) has the keys: ← → step its records.
    if (window.afDlg.top()) return;
    if (e.target.closest && e.target.closest("input, textarea, select, [contenteditable]")) return;
    if (e.key === "ArrowLeft" || e.key === "ArrowUp") { e.preventDefault(); go("a[data-newer]"); }
    if (e.key === "ArrowRight" || e.key === "ArrowDown") { e.preventDefault(); go("a[data-older]"); }
  });
  // The middle list: the selected conversation in view, and "Find in these conversations".
  const sel = document.querySelector(".cv-item.on");
  if (sel) { const box = sel.closest(".cv-days"); box.scrollTop = sel.offsetTop - box.clientHeight / 2; }
  document.addEventListener("input", (e) => {
    const f = e.target.closest && e.target.closest("[data-cv-find]");
    if (!f) return;
    const q = f.value.trim().toLowerCase();
    const days = document.querySelector("[data-cv-days]");
    let any = false;
    for (const day of days.querySelectorAll(".cv-day")) {
      let n = 0;
      for (const a of day.querySelectorAll(".cv-item")) {
        const show = q === "" || a.dataset.find.includes(q);
        a.hidden = !show;
        if (show) n++;
      }
      day.hidden = n === 0;
      if (n) any = true;
    }
    days.querySelector("[data-cv-none]").hidden = any;
  });
  let x0 = null, y0 = null;
  document.addEventListener("touchstart", (e) => {
    if (!e.target.closest || !e.target.closest("[data-swipe]") || e.touches.length !== 1) { x0 = null; return; }
    x0 = e.touches[0].clientX; y0 = e.touches[0].clientY;
  }, { passive: true });
  document.addEventListener("touchend", (e) => {
    if (x0 === null) return;
    const dx = e.changedTouches[0].clientX - x0, dy = e.changedTouches[0].clientY - y0;
    x0 = null;
    if (Math.abs(dx) < 80 || Math.abs(dx) < Math.abs(dy) * 2) return;
    go(dx < 0 ? "a[data-older]" : "a[data-newer]");
  }, { passive: true });
})();

// The desktop bar (the owner, 2026-11-19, D-232). Its menus close like every menu (above). The
// project's tabs that don't fit fold into More, from the last one; the current tab folded marks More.
(() => {
  const fold = () => {
    const bar = document.querySelector(".btabs");
    const more = document.querySelector(".btabs-more");
    if (!bar || !more) return;
    const tabs = [...bar.children];
    const items = [...more.querySelectorAll(".bmi")];
    for (const t of tabs) t.hidden = false;
    more.hidden = true;
    if (bar.offsetParent === null || bar.scrollWidth <= bar.clientWidth) {
      for (const i of items) i.hidden = true;
      more.classList.remove("on");
      return;
    }
    more.hidden = false;
    let n = tabs.length;
    while (n > 1 && bar.scrollWidth > bar.clientWidth) tabs[--n].hidden = true;
    items.forEach((i, k) => (i.hidden = k < n));
    more.classList.toggle("on", tabs.slice(n).some((t) => t.classList.contains("on")));
  };
  let raf = 0;
  window.addEventListener("resize", () => {
    cancelAnimationFrame(raf);
    raf = requestAnimationFrame(fold);
  });
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", fold);
  else fold();
  document.fonts?.ready.then(fold);
})();

// The overview's questions, a page at a time (the owner, 2026-11-23, D-234): on the desktop one
// row (as many as the grid has columns), on the phone the first question whole and five rows a
// page. The page is kept in ?q=, and an answer comes back to it. On the Decisions tab
// (data-qpage="cards") every question is a whole card, one a page on the phone.
(() => {
  const sec = document.querySelector('[data-qpage]');
  if (!sec) return;
  const grid = sec.querySelector('.qgrid');
  const top = sec.querySelector('.pager'), bottom = sec.querySelector('.qpager-p');
  const phone = matchMedia('(max-width: 700px)');
  const cardsOnly = sec.dataset.qpage === 'cards';
  let page = Math.max(1, parseInt(new URLSearchParams(location.search).get('q'), 10) || 1);
  const withPage = (href) => {
    const u = new URL(href, location.href);
    if (page > 1) u.searchParams.set('q', page); else u.searchParams.delete('q');
    return u.pathname + u.search + u.hash;
  };
  function render() {
    const cards = [...grid.querySelectorAll('article.qc')], rows = [...grid.querySelectorAll('.qmini')];
    const mini = phone.matches && !cardsOnly;
    const items = mini ? rows : cards;
    (mini ? cards : rows).forEach((e) => { e.hidden = false; });
    grid.style.gridTemplateColumns = '';
    const per = phone.matches ? (cardsOnly ? 1 : 5) : Math.max(1, getComputedStyle(grid).gridTemplateColumns.split(' ').filter(Boolean).length);
    if (items.length <= per) {
      items.forEach((e) => { e.hidden = false; });
      top.hidden = bottom.hidden = true;
      delete sec.dataset.paged;
      page = 1;
    } else {
      const pages = Math.ceil(items.length / per);
      page = Math.min(Math.max(page, 1), pages);
      items.forEach((e, i) => { e.hidden = Math.floor(i / per) + 1 !== page; });
      // A last page with fewer questions keeps the columns' width.
      if (!phone.matches) grid.style.gridTemplateColumns = `repeat(${per}, minmax(0, 1fr))`;
      sec.dataset.paged = '';
      const first = (page - 1) * per + 1, last = Math.min(page * per, items.length);
      top.querySelector('.pg-range').textContent = `${first}–${last} of ${items.length} · page ${page} of ${pages}`;
      bottom.querySelector('.qp-pos').textContent = `${page} / ${pages}`;
      sec.querySelectorAll('.qp-prev, .pg-prev').forEach((b) => { b.disabled = page === 1; });
      sec.querySelectorAll('.qp-next, .pg-next').forEach((b) => { b.disabled = page === pages; });
      top.hidden = phone.matches;
      bottom.hidden = !phone.matches;
    }
    sec.querySelectorAll('input[name="next"]').forEach((i) => { i.value = withPage(i.value); });
    history.replaceState(history.state, '', withPage(location.href));
  }
  sec.addEventListener('click', (e) => {
    const b = e.target.closest('.qp-prev, .qp-next, .pg-prev, .pg-next');
    if (!b || b.disabled) return;
    page += b.matches('.qp-next, .pg-next') ? 1 : -1;
    render();
  });
  let raf = 0;
  addEventListener('resize', () => { cancelAnimationFrame(raf); raf = requestAnimationFrame(render); });
  render();
})();

// A long table a page at a time (data-pager="10" on its card; the pager component, ui.html pager):
// Earlier builds. Sorting starts again at page 1 of the new order.
(() => {
  document.querySelectorAll('[data-pager]').forEach((box) => {
    const per = Math.max(1, parseInt(box.dataset.pager, 10) || 10);
    const body = box.querySelector('tbody');
    const pager = box.querySelector('.pager');
    if (!body || !pager) return;
    let page = 1;
    const render = () => {
      const rows = [...body.children];
      const pages = Math.max(1, Math.ceil(rows.length / per));
      page = Math.min(Math.max(page, 1), pages);
      rows.forEach((r, i) => { r.hidden = Math.floor(i / per) + 1 !== page; });
      pager.hidden = pages === 1;
      const first = (page - 1) * per + 1, last = Math.min(page * per, rows.length);
      pager.querySelector('.pg-range').textContent = `${first}–${last} of ${rows.length} · page ${page} of ${pages}`;
      pager.querySelector('.pg-prev').disabled = page === 1;
      pager.querySelector('.pg-next').disabled = page === pages;
    };
    box.addEventListener('click', (e) => {
      const b = e.target.closest('.pg-prev, .pg-next');
      if (!b || b.disabled || !pager.contains(b)) return;
      page += b.classList.contains('pg-next') ? 1 : -1;
      render();
    });
    box.addEventListener('af:sorted', () => { page = 1; render(); });
    render();
  });
})();

// Number fields' own steppers (.numf): one step up or down, within min and max, as the browser's
// arrows would; the ends are greyed out.
(() => {
  const ends = (input) => {
    const box = input.closest(".numf");
    if (!box) return;
    const v = input.valueAsNumber;
    const [up, down] = box.querySelectorAll("[data-step]");
    up.disabled = input.max !== "" && !Number.isNaN(v) && v >= Number(input.max);
    down.disabled = input.min !== "" && !Number.isNaN(v) && v <= Number(input.min);
  };
  const all = (root) => root.querySelectorAll(".numf input").forEach(ends);
  document.addEventListener("click", (e) => {
    const b = e.target.closest && e.target.closest(".numf [data-step]");
    if (!b) return;
    const input = b.closest(".numf").querySelector("input");
    if (b.dataset.step === "1") input.stepUp();
    else input.stepDown();
    input.dispatchEvent(new Event("input", { bubbles: true }));
    input.dispatchEvent(new Event("change", { bubbles: true }));
    ends(input);
  });
  document.addEventListener("input", (e) => {
    if (e.target.matches && e.target.matches(".numf input")) ends(e.target);
  });
  document.addEventListener("DOMContentLoaded", () => all(document));
  document.addEventListener("af:content", (e) => all(e.detail));
})();

// The table component (templates/table.html): a click on a header sorts by its column; the second
// click turns it around, the third goes back to the order the page came in (the default). Works
// for <table>s and for the Notes tab's grid of rows (role=row), and for tables in dialogs.
(() => {
  const rowsOf = (box) => box.querySelector(":scope tbody, :scope [role=rowgroup]");
  const headsOf = (box) => {
    const b = box.querySelector(".th-sort");
    const th = b && b.closest("th, [role=columnheader]");
    return th ? [...th.parentElement.children] : [];
  };
  const value = (row, col, kind) => {
    const cell = row.children[col];
    const raw = cell ? (cell.dataset.v ?? cell.textContent.trim()) : "";
    if (kind === "text") return raw.toLowerCase();
    const n = parseFloat(raw);
    return Number.isNaN(n) ? -Infinity : n;
  };
  const other = (d) => (d === "ascending" ? "descending" : "ascending");
  // Sorts `box` by column `col` in `dir` ("" for the order it came in).
  const sort = (box, col, dir) => {
    const body = rowsOf(box);
    const heads = headsOf(box);
    const th = heads[col];
    if (!body || (dir && !th)) return;
    const kind = (th && th.querySelector(".th-sort")?.dataset.sort) || "text";
    const rows = [...body.children];
    rows.forEach((r, i) => { if (r.dataset.i === undefined) r.dataset.i = String(i); });
    if (dir) {
      box.dataset.sortCol = String(col);
      box.dataset.sortDir = dir;
    } else {
      delete box.dataset.sortCol;
      delete box.dataset.sortDir;
    }
    for (const h of heads) {
      const d = dir ? (h === th ? dir : "") : (h.dataset.default || "");
      if (d) h.setAttribute("aria-sort", d); else h.removeAttribute("aria-sort");
    }
    const sign = dir === "ascending" ? 1 : -1;
    rows.sort((a, z) => {
      if (dir) {
        const x = value(a, col, kind), y = value(z, col, kind);
        if (x < y) return -sign;
        if (x > y) return sign;
      }
      return +a.dataset.i - +z.dataset.i;
    });
    body.append(...rows);
    box.dispatchEvent(new CustomEvent("af:sorted", { bubbles: true }));
  };
  document.addEventListener("click", (e) => {
    const b = e.target.closest && e.target.closest(".th-sort");
    const box = b && b.closest("[data-sortable]");
    const th = b && b.closest("th, [role=columnheader]");
    if (!box || !th) return;
    const col = [...th.parentElement.children].indexOf(th);
    // Where this column is now: sorted by a click, or the default order (which may be by it).
    const mine = box.dataset.sortCol === String(col) ? box.dataset.sortDir : "";
    const first = b.dataset.sort === "text" ? "ascending" : "descending";
    let next;
    if (th.dataset.default) next = mine ? "" : other(th.dataset.default);
    else next = !mine ? first : mine === first ? other(first) : "";
    sort(box, col, next);
  });
  // A part of the page fetched again (the Build servers section every 5 s) keeps its tables' sort.
  window.afKeepSort = (from, to) => {
    const was = [...from.querySelectorAll("[data-sortable]")];
    const now = [...to.querySelectorAll("[data-sortable]")];
    was.forEach((b, i) => { if (b.dataset.sortDir && now[i]) sort(now[i], +b.dataset.sortCol, b.dataset.sortDir); });
  };
})();

// A path folds its first parts into "…" only when it doesn't fit its line, and "…" shows them all
// (the owner, 2026-11-25, D-282: "why is this always ".../" ? can we expand this?"). Measured with
// its parts unshrunk, again when its width changes and when a dialog brings one.
(() => {
  const fit = (nav) => {
    const ol = nav.querySelector("ol");
    if (!ol || nav.classList.contains("unfolded")) return;
    for (const li of ol.querySelectorAll("li.pfold")) li.remove();
    const items = [...ol.children];
    for (const li of items) li.hidden = false;
    nav.classList.remove("folded");
    nav.classList.add("pmeasure");
    const fits = () => ol.scrollWidth <= ol.clientWidth + 1;
    if (items.length > 1 && !fits()) {
      const more = document.createElement("li");
      more.className = "pfold";
      const b = document.createElement("button");
      b.type = "button";
      b.className = "pfold-btn";
      b.textContent = "…";
      b.title = "Show the whole path";
      b.setAttribute("aria-label", "Show the whole path");
      more.append(b);
      ol.insertBefore(more, items[0]);
      let i = 0;
      while (i < items.length - 1 && !fits()) items[i++].hidden = true;
      nav.classList.add("folded");
    }
    nav.classList.remove("pmeasure");
  };
  const seen = new WeakSet();
  const ro = "ResizeObserver" in window ? new ResizeObserver((es) => { for (const e of es) fit(e.target); }) : null;
  const all = () => {
    for (const nav of document.querySelectorAll("nav.path")) {
      if (!seen.has(nav)) { seen.add(nav); if (ro) ro.observe(nav); }
      fit(nav);
    }
  };
  document.addEventListener("click", (e) => {
    const b = e.target.closest && e.target.closest(".pfold-btn");
    if (!b) return;
    const nav = b.closest("nav.path");
    nav.classList.add("unfolded");
    nav.classList.remove("folded");
    b.parentElement.remove();
    for (const li of nav.querySelectorAll("li[hidden]")) li.hidden = false;
  });
  document.addEventListener("af:content", () => requestAnimationFrame(all));
  all();
})();

// The first sign-in and Settings › Profile: a preview of how you show up, as you type and pick
// (D-270).
document.addEventListener("input", (e) => {
  const form = e.target.closest && e.target.closest("[data-profile]");
  if (!form) return;
  if (e.target.matches("[data-pv-name]")) {
    const who = form.querySelector("[data-pv-who]");
    if (who) who.textContent = e.target.value.trim().toLowerCase() || "you";
  }
});
document.addEventListener("change", (e) => {
  const form = e.target.closest && e.target.closest("[data-profile]");
  if (!form || e.target.name !== "icon") return;
  for (const img of form.querySelectorAll("[data-pv-icon]")) img.src = "/static/avatars/" + e.target.value + ".svg";
});
