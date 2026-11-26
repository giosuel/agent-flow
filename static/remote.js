// A build job's page follows its output while it runs: new lines every second, numbered as in a
// note's log (the library's code view, D-275), the state and the time with them. Stops when the
// job is over (and while the tab is hidden).
"use strict";
(() => {
  const log = document.getElementById("job-log");
  if (!log || log.dataset.done) return;
  const id = log.dataset.job;
  let next = Number(log.dataset.next || 0);
  let n = Number(log.dataset.n || 0);
  let busy = false;
  // As remote::line_severity on the server, in the code view's tones.
  const cls = (line) => {
    if (line.startsWith("· ")) return "cv-dim";
    const l = line.trimStart();
    if (l.startsWith("error") || l.startsWith("FAILED") || l.includes("panicked at") || l.startsWith("test result: FAILED")) return "lv-error";
    if (l.startsWith("warning")) return "lv-warn";
    return "";
  };
  async function tick() {
    if (document.hidden || busy) return;
    busy = true;
    try {
      const r = await fetch(`/machines/jobs/${id}/output?after=${next}`, { headers: { accept: "application/json" } });
      if (!r.ok) return;
      const j = await r.json();
      const atBottom = log.scrollTop + log.clientHeight >= log.scrollHeight - 30;
      const lines = j.text.split("\n");
      if (j.text.endsWith("\n")) lines.pop();
      for (const line of j.text ? lines : []) {
        const num = document.createElement("span");
        num.className = "cv-n";
        num.textContent = String(++n);
        const text = document.createElement("span");
        text.className = `cv-t ${cls(line)}`.trim();
        text.textContent = line;
        log.append(num, text);
        document.getElementById("job-empty")?.remove();
      }
      const count = document.getElementById("job-lines");
      if (count) count.textContent = `${j.finished ? "" : "live · "}${j.lines} lines`;
      next = j.next;
      if (atBottom) log.scrollTop = log.scrollHeight;
      document.getElementById("job-time").textContent = j.time;
      const chip = document.getElementById("job-state");
      chip.textContent = j.label;
      chip.className = `chip ${j.chip}`.trim();
      if (j.finished) {
        clearInterval(timer);
        document.getElementById("job-cancel")?.remove();
      }
    } catch {
      // Try again on the next tick.
    } finally {
      busy = false;
    }
  }
  log.scrollTop = log.scrollHeight;
  const timer = setInterval(tick, 1000);
})();
