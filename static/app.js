// AgentFlow's small helpers. The pages are plain forms and links and work without JavaScript
// wherever they can.
"use strict";

// Buttons with data-confirm ask before they submit (no inline handlers: the CSP forbids them).
document.addEventListener("click", (e) => {
  const el = e.target.closest("[data-confirm]");
  if (el && !window.confirm(el.dataset.confirm)) {
    e.preventDefault();
    e.stopImmediatePropagation();
  }
}, true);
