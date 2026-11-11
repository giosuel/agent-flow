// The Log page's live stream: new entries appear at the top while "Stream" is on.
"use strict";
document.addEventListener("DOMContentLoaded", () => {
  const button = document.getElementById("stream");
  const list = document.getElementById("entries");
  if (!button || !list) return;
  let source = null;
  const add = (e) => {
    const li = document.createElement("li");
    const details = document.createElement("details");
    details.className = "entry fresh sev-" + e.severity;
    const summary = document.createElement("summary");
    const parts = [["span", "mono sub", e.time], ["strong", "", e.actor], ["span", "chip", e.action], ["span", "esum", e.summary]];
    for (const [tag, cls, text] of parts) {
      const el = document.createElement(tag);
      if (cls) el.className = cls;
      el.textContent = text; // never HTML: entries come from agents
      summary.appendChild(el);
    }
    details.appendChild(summary);
    li.appendChild(details);
    list.prepend(li);
  };
  button.addEventListener("click", () => {
    if (source) {
      source.close();
      source = null;
      button.setAttribute("aria-pressed", "false");
      button.textContent = "Stream";
      return;
    }
    const url = "/log/stream?q=" + encodeURIComponent(button.dataset.q || "") + "&after=" + encodeURIComponent(button.dataset.after || "0");
    source = new EventSource(url);
    source.onmessage = (m) => {
      try {
        const e = JSON.parse(m.data);
        button.dataset.after = e.seq;
        add(e);
      } catch (_) { /* ignore a broken line */ }
    };
    button.setAttribute("aria-pressed", "true");
    button.textContent = "Stop";
  });
});
