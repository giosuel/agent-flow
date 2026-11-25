// Passkey sign-in and enrolment. The server (webauthn-rs) sends and expects base64url strings
// where the browser API uses ArrayBuffers.
"use strict";

const b64u = {
  decode(s) {
    const pad = "=".repeat((4 - (s.length % 4)) % 4);
    const bin = atob(s.replace(/-/g, "+").replace(/_/g, "/") + pad);
    return Uint8Array.from(bin, (c) => c.charCodeAt(0)).buffer;
  },
  encode(buf) {
    let bin = "";
    for (const b of new Uint8Array(buf)) bin += String.fromCharCode(b);
    return btoa(bin).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
  },
};

async function postJson(url, body) {
  const res = await fetch(url, {
    method: "POST",
    credentials: "same-origin",
    headers: { "Content-Type": "application/json", Accept: "application/json" },
    body: JSON.stringify(body || {}),
  });
  const data = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error(data.error || `${res.status} ${res.statusText}`);
  return data;
}

// A notice in the library's look: its tone's icon (from the page's sprite) and the text.
function say(el, text, kind) {
  if (!el) return;
  const name = kind === "ok" ? "check" : kind === "warn" || kind === "bad" ? "alert" : "info";
  el.innerHTML = `<svg class="i" aria-hidden="true"><use href="#i-${name}"/></svg><span></span>`;
  el.querySelector("span").textContent = text;
  el.className = "notice" + (kind ? " " + kind : "");
  el.hidden = !text;
}

async function signIn(button, status) {
  button.disabled = true;
  say(status, "Waiting for your passkey…");
  try {
    const { id, options } = await postJson("/login/passkey/start");
    const pk = options.publicKey;
    pk.challenge = b64u.decode(pk.challenge);
    for (const c of pk.allowCredentials || []) c.id = b64u.decode(c.id);
    const cred = await navigator.credentials.get({ publicKey: pk });
    const r = cred.response;
    const credential = {
      id: cred.id,
      rawId: b64u.encode(cred.rawId),
      type: cred.type,
      response: {
        authenticatorData: b64u.encode(r.authenticatorData),
        clientDataJSON: b64u.encode(r.clientDataJSON),
        signature: b64u.encode(r.signature),
        userHandle: r.userHandle ? b64u.encode(r.userHandle) : null,
      },
      extensions: cred.getClientExtensionResults(),
    };
    await postJson("/login/passkey/finish", { id, credential });
    // Back where the owner was going (an approval link), if it's a page of this site.
    const next = new URLSearchParams(window.location.search).get("next") || "/";
    window.location.assign(/^\/[A-Za-z0-9\/_.-]*$/.test(next) && !next.startsWith("//") ? next : "/");
  } catch (e) {
    if (e.name === "NotAllowedError") say(status, "Cancelled. Try again when you're ready.");
    else say(status, "Couldn't sign in: " + e.message, "bad");
    button.disabled = false;
  }
}

// A name for a passkey made from a link's page: the kind of device it's on.
function deviceName() {
  const ua = navigator.userAgent;
  if (/iPhone/.test(ua)) return "iPhone";
  if (/iPad/.test(ua)) return "iPad";
  if (/Android/.test(ua)) return "Android";
  if (/Mac OS X/.test(ua)) return "Mac";
  if (/Windows/.test(ua)) return "Windows";
  if (/Linux/.test(ua)) return "Linux";
  return "Passkey";
}

async function enrol(button, nameInput, status) {
  button.disabled = true;
  say(status, "Follow your device's prompt…");
  try {
    // Settings enrols for the person signed in; a setup, invite or device link's page for its
    // person (its button names its own addresses).
    const start = button.dataset.start || "/settings/passkeys/start";
    const finish = button.dataset.finish || "/settings/passkeys/finish";
    const { id, options } = await postJson(start);
    const pk = options.publicKey;
    pk.challenge = b64u.decode(pk.challenge);
    pk.user.id = b64u.decode(pk.user.id);
    for (const c of pk.excludeCredentials || []) c.id = b64u.decode(c.id);
    const cred = await navigator.credentials.create({ publicKey: pk });
    const r = cred.response;
    const credential = {
      id: cred.id,
      rawId: b64u.encode(cred.rawId),
      type: cred.type,
      response: {
        attestationObject: b64u.encode(r.attestationObject),
        clientDataJSON: b64u.encode(r.clientDataJSON),
        transports: r.getTransports ? r.getTransports() : undefined,
      },
      extensions: cred.getClientExtensionResults(),
    };
    const done = await postJson(finish, { id, name: nameInput ? nameInput.value : deviceName(), credential });
    if (done.next && /^\/[A-Za-z0-9\/_.?=&#-]*$/.test(done.next) && !done.next.startsWith("//")) window.location.assign(done.next);
    else window.location.reload();
  } catch (e) {
    if (e.name === "NotAllowedError") say(status, "Cancelled.");
    else if (e.name === "InvalidStateError") say(status, "This device already has a passkey here.", "warn");
    else say(status, "Couldn't add the passkey: " + e.message, "bad");
    button.disabled = false;
  }
}

// Delegated, so the buttons work in the Settings dialog too (its content arrives later).
document.addEventListener("click", (e) => {
  const status = document.getElementById("passkey-status");
  const signin = e.target.closest("#passkey-signin");
  const add = e.target.closest("#passkey-add");
  if (!signin && !add) return;
  if (!window.PublicKeyCredential) {
    say(status, "This browser doesn't support passkeys.", "warn");
    return;
  }
  if (signin) signIn(signin, status);
  else enrol(add, document.getElementById("passkey-name"), status);
});
