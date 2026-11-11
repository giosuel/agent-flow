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
