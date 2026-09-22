function esc(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) => ({
    "&": "&amp;",
    "<": "&lt;",
    ">": "&gt;",
    '"': "&quot;",
    "'": "&#39;",
  }[c]));
}

function b64urlToBuf(b64) {
  const pad = "===".slice((b64.length + 3) % 4);
  const bin = atob(String(b64).replace(/-/g, "+").replace(/_/g, "/") + pad);
  const buf = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) buf[i] = bin.charCodeAt(i);
  return buf.buffer;
}

function bufToB64url(buf) {
  const bytes = new Uint8Array(buf);
  let s = "";
  bytes.forEach((b) => (s += String.fromCharCode(b)));
  return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

function revivePublicKey(opts) {
  const pk = JSON.parse(JSON.stringify(opts));
  pk.challenge = b64urlToBuf(pk.challenge);
  if (pk.user && pk.user.id) pk.user.id = b64urlToBuf(pk.user.id);
  (pk.excludeCredentials || []).forEach((c) => {
    c.id = b64urlToBuf(c.id);
  });
  (pk.allowCredentials || []).forEach((c) => {
    c.id = b64urlToBuf(c.id);
  });
  return pk;
}

function credentialToJson(cred) {
  const json = {
    id: cred.id,
    rawId: bufToB64url(cred.rawId),
    type: cred.type,
    response: {},
    clientExtensionResults: cred.getClientExtensionResults ? cred.getClientExtensionResults() : {},
  };
  const r = cred.response;
  if (r.attestationObject) json.response.attestationObject = bufToB64url(r.attestationObject);
  if (r.clientDataJSON) json.response.clientDataJSON = bufToB64url(r.clientDataJSON);
  if (r.authenticatorData) json.response.authenticatorData = bufToB64url(r.authenticatorData);
  if (r.signature) json.response.signature = bufToB64url(r.signature);
  if (r.userHandle) json.response.userHandle = bufToB64url(r.userHandle);
  if (r.getTransports) json.response.transports = r.getTransports();
  if (cred.authenticatorAttachment) json.authenticatorAttachment = cred.authenticatorAttachment;
  return json;
}

async function authJson(url, opt = {}) {
  const init = Object.assign({ credentials: "same-origin" }, opt);
  if (init.body && typeof init.body !== "string") {
    init.headers = Object.assign({ "content-type": "application/json" }, init.headers || {});
    init.body = JSON.stringify(init.body);
  }
  const r = await fetch(url, init);
  const text = await r.text();
  let data = null;
  try {
    data = text ? JSON.parse(text) : null;
  } catch {
    data = null;
  }
  if (!r.ok) throw new Error((data && data.error) || `${url} ${r.status}`);
  return data;
}

function inviteFromHash() {
  const m = location.hash.match(/invite=([^&]+)/);
  return m ? decodeURIComponent(m[1]) : "";
}

function showGate(name) {
  document.getElementById("app").hidden = true;
  document.getElementById("gate").hidden = false;
  ["setup", "login", "register"].forEach((id) => {
    document.getElementById("card-" + id).hidden = id !== name;
  });
}

function showApp() {
  document.getElementById("gate").hidden = true;
  document.getElementById("app").hidden = false;
}

function renderAccount(st) {
  const u = st.user || {};
  document.getElementById("account-btn").textContent = u.username || "Account";
  document.getElementById("account-name").textContent = u.isAdmin
    ? u.username + " · admin"
    : u.username || "";
  document.getElementById("invite-btn").hidden = !u.isAdmin || st.passwordBackend === "pam";
  const keys = st.passkeys || [];
  document.getElementById("passkey-list").innerHTML =
    keys
      .map(
        (k) =>
          `<div class="row"><span>Passkey ${esc(k.id)}</span><button type="button" class="ghost" data-passkey="${esc(k.id)}">Remove</button></div>`
      )
      .join("") || "<p>No passkeys yet</p>";
}

async function authStatus() {
  const r = await fetch("/api/auth/status", { credentials: "same-origin" });
  return r.json();
}

async function waRegister(username, invite) {
  const ccr = await authJson("/api/auth/webauthn/register/start", {
    method: "POST",
    body: { username, invite },
  });
  const cred = await navigator.credentials.create({ publicKey: revivePublicKey(ccr.publicKey) });
  await authJson("/api/auth/webauthn/register/finish", {
    method: "POST",
    body: { credential: credentialToJson(cred), username, invite },
  });
}

async function waLogin(username) {
  const rcr = await authJson("/api/auth/webauthn/login/start", { method: "POST", body: { username } });
  const cred = await navigator.credentials.get({ publicKey: revivePublicKey(rcr.publicKey) });
  await authJson("/api/auth/webauthn/login/finish", {
    method: "POST",
    body: { credential: credentialToJson(cred), username },
  });
}

async function routeFromStatus(st) {
  if (st.passwordBackend === "pam") {
    document.getElementById("login-lead").textContent = st.setupRequired
      ? "Sign in with your Linux account. The first user becomes admin; later users must be in group teslamate-rs."
      : "Sign in with your Linux username and password.";
  }
  if (st.setupRequired && st.passwordBackend !== "pam") {
    if (st.setupAllowed === false) {
      showGate("login");
      const lead = document.getElementById("login-lead");
      if (lead) {
        lead.textContent =
          "First-admin setup is locked on this bind. Use loopback or set TESLAMATE_RS_ALLOW_SETUP=1.";
      }
      return false;
    }
    showGate("setup");
    return false;
  }
  if (!st.authenticated) {
    showGate(inviteFromHash() && st.registerEnabled !== false ? "register" : "login");
    return false;
  }
  renderAccount(st);
  showApp();
  return true;
}

window.tmAuth = {
  status: authStatus,
  route: routeFromStatus,
};

document.getElementById("card-setup").onsubmit = async (e) => {
  e.preventDefault();
  const err = document.getElementById("setup-err");
  err.textContent = "";
  try {
    await authJson("/api/auth/setup", {
      method: "POST",
      body: {
        username: document.getElementById("setup-user").value,
        password: document.getElementById("setup-pass").value,
      },
    });
    await boot();
  } catch (ex) {
    err.textContent = ex.message || ex;
  }
};
document.getElementById("setup-passkey").onclick = async () => {
  const err = document.getElementById("setup-err");
  err.textContent = "";
  try {
    await waRegister(document.getElementById("setup-user").value);
    await boot();
  } catch (ex) {
    err.textContent = ex.message || ex;
  }
};
document.getElementById("card-login").onsubmit = async (e) => {
  e.preventDefault();
  const err = document.getElementById("login-err");
  err.textContent = "";
  try {
    await authJson("/api/auth/login", {
      method: "POST",
      body: {
        username: document.getElementById("login-user").value,
        password: document.getElementById("login-pass").value,
      },
    });
    await boot();
  } catch (ex) {
    err.textContent = ex.message || ex;
  }
};
document.getElementById("login-passkey").onclick = async () => {
  const err = document.getElementById("login-err");
  err.textContent = "";
  try {
    await waLogin(document.getElementById("login-user").value);
    await boot();
  } catch (ex) {
    err.textContent = ex.message || ex;
  }
};
document.getElementById("card-register").onsubmit = async (e) => {
  e.preventDefault();
  const err = document.getElementById("reg-err");
  err.textContent = "";
  try {
    await authJson("/api/auth/register", {
      method: "POST",
      body: {
        username: document.getElementById("reg-user").value,
        password: document.getElementById("reg-pass").value,
        invite: inviteFromHash(),
      },
    });
    await boot();
  } catch (ex) {
    err.textContent = ex.message || ex;
  }
};
document.getElementById("reg-passkey").onclick = async () => {
  const err = document.getElementById("reg-err");
  err.textContent = "";
  try {
    await waRegister(document.getElementById("reg-user").value, inviteFromHash());
    await boot();
  } catch (ex) {
    err.textContent = ex.message || ex;
  }
};
document.getElementById("account-btn").onclick = () => {
  document.getElementById("account").classList.toggle("open");
};
document.getElementById("logout-btn").onclick = async () => {
  await authJson("/api/auth/logout", { method: "POST", body: {} });
  await boot();
};
document.getElementById("logout-all-btn").onclick = async () => {
  await authJson("/api/auth/logout-all", { method: "POST", body: {} });
  await boot();
};
document.getElementById("add-passkey").onclick = async () => {
  try {
    await waRegister();
    renderAccount(await authStatus());
  } catch (ex) {
    document.getElementById("invite-url").textContent = ex.message || ex;
  }
};
document.getElementById("passkey-list").onclick = async (e) => {
  const id = e.target.dataset.passkey;
  if (!id) return;
  try {
    await authJson("/api/auth/passkeys/" + id, { method: "DELETE" });
    renderAccount(await authStatus());
  } catch (ex) {
    document.getElementById("invite-url").textContent = ex.message || ex;
  }
};
document.getElementById("invite-btn").onclick = async () => {
  try {
    const data = await authJson("/api/admin/invites", { method: "POST", body: {} });
    const url = location.origin + data.url;
    document.getElementById("invite-url").textContent = url;
    try {
      await navigator.clipboard.writeText(url);
    } catch {
      /* ignore */
    }
  } catch (ex) {
    document.getElementById("invite-url").textContent = ex.message || ex;
  }
};
