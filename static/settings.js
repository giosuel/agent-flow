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
  const say = (t, kind) => { status.textContent = t; status.className = "notice" + (kind ? " " + kind : ""); status.hidden = !t; };
  if (!("serviceWorker" in navigator) || !("PushManager" in window)) {
    say(/iPhone|iPad/.test(navigator.userAgent)
      ? "On iPhone, add AgentFlow to your home screen first (Share, then Add to Home Screen), then open it from there."
      : "This browser can't receive push notifications.", "warn");
    return;
  }
  button.disabled = true;
  try {
    const permission = await Notification.requestPermission();
    if (permission !== "granted") throw new Error("notifications are blocked for this site");
    const reg = await navigator.serviceWorker.register("/sw.js");
    await navigator.serviceWorker.ready;
    let sub = await reg.pushManager.getSubscription();
    if (!sub) {
      sub = await reg.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: urlB64ToBytes(button.dataset.key) });
    }
    await post("/settings/push/subscribe", { subscription: sub.toJSON(), device: deviceName() });
    window.location.reload();
  } catch (e) {
    say("Couldn't turn notifications on: " + e.message, "warn");
    button.disabled = false;
  }
}

document.addEventListener("DOMContentLoaded", () => {
  const button = document.getElementById("push-on");
  const status = document.getElementById("push-status");
  if (button) button.addEventListener("click", () => turnOn(button, status));
  // Register the worker on every visit so the app keeps receiving pushes after updates.
  if ("serviceWorker" in navigator) navigator.serviceWorker.register("/sw.js").catch(() => {});
});
