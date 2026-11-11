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

// Buttons with data-confirm ask before they submit (no inline handlers: the CSP forbids them).
document.addEventListener("click", (e) => {
  const el = e.target.closest("[data-confirm]");
  if (el && !window.confirm(el.dataset.confirm)) {
    e.preventDefault();
    e.stopImmediatePropagation();
  }
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

document.addEventListener("DOMContentLoaded", () => {
  for (const pre of document.querySelectorAll("pre.cmd")) {
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
});

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
  function small() {
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
  });
})();

// Answering a question: pick an option or write your own answer, never both, and nothing goes
// out until Send answer (the owner, 2026-11-11). Choosing an option empties the text; typing
// clears the option; Send is enabled only when exactly one of them is given. The server refuses
// both as well, so this holds without the script too.
document.addEventListener("DOMContentLoaded", () => {
  for (const form of document.querySelectorAll("form[data-answer]")) {
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
});

// To test: count what's ticked; Send needs at least one.
document.addEventListener("DOMContentLoaded", () => {
  for (const form of document.querySelectorAll("form[data-tt]")) {
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
});
