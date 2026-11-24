// Settings: turn push notifications on for this device.
"use strict";

function urlB64ToBytes(s) {
  const pad = "=".repeat((4 - (s.length % 4)) % 4);
  const bin = atob(s.replace(/-/g, "+").replace(/_/g, "/") + pad);
  return Uint8Array.from(bin, (c) => c.charCodeAt(0));
}

function deviceName() {
  const ua = navigator.userAgent;
  if (/iPhone/.test(ua)) return "iPhone";
  if (/iPad/.test(ua)) return "iPad";
  if (/Android/.test(ua)) return "Android";
  const browser = /Firefox\//.test(ua) ? "Firefox" : /Edg\//.test(ua) ? "Edge" : /Chrome\//.test(ua) ? "Chrome" : /Safari\//.test(ua) ? "Safari" : "Browser";
  const os = /Mac OS X/.test(ua) ? "macOS" : /Windows/.test(ua) ? "Windows" : /Linux/.test(ua) ? "Linux" : "";
  return os ? `${browser} on ${os}` : browser;
}

async function post(url, body) {
  const res = await fetch(url, {
    method: "POST",
    credentials: "same-origin",
    headers: { "Content-Type": "application/json", Accept: "application/json" },
    body: JSON.stringify(body),
  });
  const data = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error(data.error || res.statusText);
  return data;
}

async function turnOn(button, status) {
  // A notice in the library's look: its tone's icon and the text.
  const say = (t, kind) => {
    const name = kind === "ok" ? "check" : kind === "warn" || kind === "bad" ? "alert" : "info";
    status.innerHTML = `<svg class="i" aria-hidden="true"><use href="#i-${name}"/></svg><span></span>`;
    status.querySelector("span").textContent = t;
    status.className = "notice" + (kind ? " " + kind : "");
    status.hidden = !t;
  };
  if (!("serviceWorker" in navigator) || !("PushManager" in window)) {
    window.afDialog({
      title: "Notifications aren't available here",
      text: /iPhone|iPad/.test(navigator.userAgent)
        ? "On iPhone, add AgentFlow to your Home Screen first (Share, then Add to Home Screen), open it from there and turn them on again."
        : "This browser can't receive push notifications.",
    });
    return;
  }
  button.disabled = true;
  try {
    const permission = await Notification.requestPermission();
    if (permission !== "granted") {
      button.disabled = false;
      await window.afDialog({
        title: "Notifications are blocked",
        text: "This browser doesn't allow them for AgentFlow. Allow notifications for this site in its settings, then turn them on again.",
      });
      return;
    }
    const reg = await navigator.serviceWorker.register("/sw.js");
    await navigator.serviceWorker.ready;
    let sub = await reg.pushManager.getSubscription();
    if (!sub) {
      sub = await reg.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: urlB64ToBytes(button.dataset.key) });
    }
    await post("/settings/push/subscribe", { subscription: sub.toJSON(), device: deviceName() });
    window.location.reload();
  } catch (e) {
    say("Couldn't turn notifications on: " + e.message, "bad");
    button.disabled = false;
  }
}

// Delegated, so the button works in the Settings dialog too.
document.addEventListener("click", (e) => {
  const button = e.target.closest("#push-on");
  if (button) turnOn(button, document.getElementById("push-status"));
});
document.addEventListener("DOMContentLoaded", () => {
  // Register the worker on every visit so the app keeps receiving pushes after updates.
  if ("serviceWorker" in navigator) navigator.serviceWorker.register("/sw.js").catch(() => {});
});
