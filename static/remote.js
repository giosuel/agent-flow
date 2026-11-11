// A build job's page follows its output while it runs: new lines every second, the state and the
// time with them. Stops when the job is over (and while the tab is hidden).
"use strict";
(() => {
  const log = document.getElementById("job-log");
  if (!log || log.dataset.done) return;
  const id = log.dataset.job;
  let next = Number(log.dataset.next || 0);
  let busy = false;
  const cls = (line) => {
    if (line.startsWith("· ")) return "d";
    const l = line.trimStart();
    if (l.startsWith("error") || l.startsWith("FAILED") || l.includes("panicked at") || l.startsWith("test result: FAILED")) return "e";
    if (l.startsWith("warning")) return "w";
    return "";
  };
  async function tick() {
    if (document.hidden || busy) return;
    busy = true;
    try {
      const r = await fetch(`/machine/jobs/${id}/output?after=${next}`, { headers: { accept: "application/json" } });
      if (!r.ok) return;
      const j = await r.json();
      const atBottom = log.scrollTop + log.clientHeight >= log.scrollHeight - 30;
      for (const line of j.text.split("\n")) {
        if (line === "" && j.text.endsWith("\n")) continue;
        const div = document.createElement("div");
        div.className = cls(line);
        div.textContent = line;
        log.appendChild(div);
      }
      next = j.next;
      if (atBottom) log.scrollTop = log.scrollHeight;
      document.getElementById("job-time").textContent = j.time;
      const chip = document.getElementById("job-state");
      chip.textContent = j.finished ? (j.state === "done" ? "done" : `${j.state}${j.reason ? " · " + j.reason : ""}`) : `${j.state}${j.reason ? " · " + j.reason : ""}`;
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
