// AgentFlow's small helpers. The pages work without JavaScript wherever they can; htmx does the
// partial updates.
"use strict";

// Buttons with data-confirm ask before they submit (no inline handlers: the CSP forbids them).
document.addEventListener("click", (e) => {
  const el = e.target.closest("[data-confirm]");
  if (el && !window.confirm(el.dataset.confirm)) {
    e.preventDefault();
    e.stopImmediatePropagation();
  }
}, true);
