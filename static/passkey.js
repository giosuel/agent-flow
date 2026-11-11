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

function say(el, text, kind) {
  if (!el) return;
  el.textContent = text;
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
    window.location.assign("/");
  } catch (e) {
    say(status, e.name === "NotAllowedError" ? "Cancelled. Try again when you're ready." : "Couldn't sign in: " + e.message, "warn");
    button.disabled = false;
  }
}

async function enrol(button, nameInput, status) {
  button.disabled = true;
  say(status, "Follow your device's prompt…");
  try {
    const { id, options } = await postJson("/settings/passkeys/start");
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
    await postJson("/settings/passkeys/finish", { id, name: nameInput ? nameInput.value : "", credential });
    window.location.reload();
  } catch (e) {
    say(status, e.name === "NotAllowedError" ? "Cancelled." : e.name === "InvalidStateError" ? "This device already has a passkey here." : "Couldn't add the passkey: " + e.message, "warn");
    button.disabled = false;
  }
}

document.addEventListener("DOMContentLoaded", () => {
  const status = document.getElementById("passkey-status");
  const signin = document.getElementById("passkey-signin");
  if (signin) signin.addEventListener("click", () => signIn(signin, status));
  const add = document.getElementById("passkey-add");
  if (add) add.addEventListener("click", () => enrol(add, document.getElementById("passkey-name"), status));
  if ((signin || add) && !window.PublicKeyCredential) {
    say(status, "This browser doesn't support passkeys.", "warn");
    if (signin) signin.disabled = true;
    if (add) add.disabled = true;
  }
});
