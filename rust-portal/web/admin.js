"use strict";
// Management page: users, computers and live sessions. Uses $, show and api from app.js.

const admin = { users: [], computers: [], tab: "users", timer: 0, editing: null };

function el(tag, attrs, ...children) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (k === "class") e.className = v;
    else if (k.startsWith("on")) e.addEventListener(k.slice(2), v);
    else e.setAttribute(k, v);
  }
  for (const c of children) e.append(c);
  return e;
}

const tag = (text, kind) => el("span", { class: "tag" + (kind ? " " + kind : "") }, text);

function adminError(r) {
  $("admin-error").textContent = r.ok ? "" : (r.data.error || `Request failed (${r.status}).`);
  return r.ok;
}

$("manage").addEventListener("click", () => openAdmin());
$("admin-back").addEventListener("click", () => {
  clearInterval(admin.timer);
  api("/api/me").then((r) => (r.ok ? home(r.data) : show("login")));
});
for (const b of $("admin-tabs").querySelectorAll("button")) {
  b.addEventListener("click", () => selectTab(b.dataset.tab));
}

async function openAdmin() {
  show("admin");
  $("admin-error").textContent = "";
  await loadAll();
  selectTab(admin.tab);
}

function selectTab(name) {
  admin.tab = name;
  for (const b of $("admin-tabs").querySelectorAll("button")) b.classList.toggle("active", b.dataset.tab === name);
  for (const t of ["users", "computers", "sessions", "traffic"]) $("tab-" + t).classList.toggle("hidden", t !== name);
  clearInterval(admin.timer);
  const poll = (load, ms) => {
    load();
    admin.timer = setInterval(() => {
      if ($("admin").classList.contains("hidden")) clearInterval(admin.timer);
      else load();
    }, ms);
  };
  if (name === "sessions") poll(loadSessions, 5000);
  if (name === "traffic") poll(loadTraffic, 60000);
}

async function loadAll() {
  const [u, c] = await Promise.all([api("/api/admin/users"), api("/api/admin/connections")]);
  if (u.status === 401) return show("login");
  if (!adminError(u) || !adminError(c)) return;
  admin.users = u.data;
  admin.computers = c.data;
  renderUsers();
  renderComputers();
}

// ---------- users ----------

$("user-filter").addEventListener("input", renderUsers);

function renderUsers() {
  const q = $("user-filter").value.trim().toLowerCase();
  const body = $("users-body");
  body.replaceChildren();
  for (const u of admin.users) {
    if (q && !u.username.toLowerCase().includes(q)) continue;
    const twoFactor = u.totp_disabled ? tag("off", "warn") : u.totp_enrolled ? tag("on", "on") : tag("set up at next sign-in");
    const computers = u.is_admin ? "all" : String(u.connections.length);
    body.append(el("tr", {},
      el("td", {}, u.username),
      el("td", {}, u.is_admin ? tag("admin", "on") : tag("user")),
      el("td", {}, twoFactor),
      el("td", {}, u.disabled ? tag("disabled", "bad") : tag("active")),
      el("td", {}, computers),
      el("td", { class: "actions" }, el("button", { class: "btn secondary", onclick: () => editUser(u) }, "Edit")),
    ));
  }
}

$("add-user").addEventListener("click", () => editUser(null));
$("u-cancel").addEventListener("click", () => $("user-dialog").close());

function editUser(u) {
  admin.editing = u;
  const isNew = !u;
  $("user-dialog-title").textContent = isNew ? "Add user" : `Edit ${u.username}`;
  $("u-name").value = isNew ? "" : u.username;
  $("u-name").disabled = !isNew;
  $("u-pass").value = "";
  $("u-pass").required = isNew;
  $("u-pass").placeholder = isNew ? "At least 8 characters" : "Leave empty to keep the current password";
  $("u-pass-label").textContent = isNew ? "Password" : "New password";
  $("u-admin").checked = !isNew && u.is_admin;
  $("u-totp-off").checked = !isNew && u.totp_disabled;
  $("u-disabled").checked = !isNew && u.disabled;
  $("u-disabled-row").classList.toggle("hidden", isNew);
  $("u-delete").classList.toggle("hidden", isNew);
  $("u-reset-totp").classList.toggle("hidden", isNew || !u.totp_enrolled);
  $("u-totp-state").textContent = isNew
    ? "The user sets up two-factor authentication at first sign-in."
    : u.totp_enrolled ? "Two-factor authentication is set up." : "Two-factor authentication is not set up yet.";
  $("user-error").textContent = "";

  const assigned = new Set(isNew ? [] : u.connections);
  const list = $("u-computers");
  list.replaceChildren();
  for (const c of admin.computers) {
    list.append(el("label", { class: "check" },
      el("input", Object.assign({ type: "checkbox", value: String(c.id) }, assigned.has(c.id) ? { checked: "" } : {})),
      c.name));
  }
  if (!admin.computers.length) list.append(el("p", { class: "note" }, "No computers yet."));
  $("user-dialog").showModal();
  (isNew ? $("u-name") : $("u-pass")).focus();
}

$("user-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const u = admin.editing;
  const connections = [...$("u-computers").querySelectorAll("input:checked")].map((i) => Number(i.value));
  const body = {
    is_admin: $("u-admin").checked,
    totp_disabled: $("u-totp-off").checked,
    connections,
  };
  const password = $("u-pass").value;
  let r;
  if (!u) {
    r = await api("/api/admin/users", { ...body, username: $("u-name").value.trim(), password });
  } else {
    body.disabled = $("u-disabled").checked;
    if (password) body.password = password;
    r = await api(`/api/admin/users/${u.id}`, body);
  }
  if (!r.ok) {
    $("user-error").textContent = r.data.error || `Request failed (${r.status}).`;
    return;
  }
  $("user-dialog").close();
  await loadAll();
});

$("u-reset-totp").addEventListener("click", async () => {
  const u = admin.editing;
  if (!confirm(`Reset two-factor for ${u.username}? Their authenticator stops working, and they set up a new one at next sign-in.`)) return;
  const r = await api(`/api/admin/users/${u.id}`, { reset_totp: true });
  if (!r.ok) return void ($("user-error").textContent = r.data.error || "Failed.");
  $("user-dialog").close();
  await loadAll();
});

$("u-delete").addEventListener("click", async () => {
  const u = admin.editing;
  if (!confirm(`Delete user ${u.username}? This cannot be undone.`)) return;
  const r = await api(`/api/admin/users/${u.id}`, undefined, "DELETE");
  if (!r.ok) return void ($("user-error").textContent = r.data.error || "Failed.");
  $("user-dialog").close();
  await loadAll();
});

// ---------- computers ----------

function renderComputers() {
  const body = $("computers-body");
  body.replaceChildren();
  const users = new Map();
  for (const u of admin.users) for (const id of u.connections) users.set(id, (users.get(id) || 0) + 1);
  for (const c of admin.computers) {
    const user = (c.domain ? c.domain + "\\" : "") + (c.username || "—");
    body.append(el("tr", {},
      el("td", {}, c.name),
      el("td", {}, `${c.hostname}:${c.port}`),
      el("td", {}, user, c.has_password ? "" : el("span", {}, " ", tag("no password", "warn"))),
      el("td", {}, String(users.get(c.id) || 0)),
      el("td", { class: "actions" }, el("button", { class: "btn secondary", onclick: () => editComputer(c) }, "Edit")),
    ));
  }
}

$("add-computer").addEventListener("click", () => editComputer(null));
$("c-cancel").addEventListener("click", () => $("computer-dialog").close());

function editComputer(c) {
  admin.editing = c;
  const isNew = !c;
  $("computer-dialog-title").textContent = isNew ? "Add computer" : `Edit ${c.name}`;
  $("c-name").value = isNew ? "" : c.name;
  $("c-host").value = isNew ? "" : c.hostname;
  $("c-port").value = isNew ? 3389 : c.port;
  $("c-user").value = isNew ? "" : c.username;
  $("c-domain").value = isNew ? "" : c.domain || "";
  $("c-pass").value = "";
  $("c-pass").placeholder = !isNew && c.has_password ? "Leave empty to keep the current password" : "";
  $("c-delete").classList.toggle("hidden", isNew);
  $("computer-error").textContent = "";
  $("computer-dialog").showModal();
  $("c-name").focus();
}

$("computer-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const c = admin.editing;
  const body = {
    name: $("c-name").value,
    hostname: $("c-host").value,
    port: Number($("c-port").value),
    username: $("c-user").value,
    domain: $("c-domain").value,
    password: $("c-pass").value,
  };
  const r = await api(c ? `/api/admin/connections/${c.id}` : "/api/admin/connections", body);
  if (!r.ok) return void ($("computer-error").textContent = r.data.error || `Request failed (${r.status}).`);
  $("computer-dialog").close();
  await loadAll();
});

$("c-delete").addEventListener("click", async () => {
  const c = admin.editing;
  if (!confirm(`Delete computer ${c.name}? Users lose access to it. This cannot be undone.`)) return;
  const r = await api(`/api/admin/connections/${c.id}`, undefined, "DELETE");
  if (!r.ok) return void ($("computer-error").textContent = r.data.error || "Failed.");
  $("computer-dialog").close();
  await loadAll();
});

// ---------- sessions ----------

async function loadSessions() {
  const r = await api("/api/admin/sessions");
  if (!adminError(r)) return;
  const body = $("sessions-body");
  body.replaceChildren();
  if (!r.data.length) {
    body.append(el("tr", {}, el("td", { colspan: "4", class: "note" }, "No one is connected.")));
    return;
  }
  for (const s of r.data) {
    const since = new Date(s.started * 1000);
    body.append(el("tr", {},
      el("td", {}, s.username),
      el("td", {}, s.connection),
      el("td", {}, since.toLocaleString()),
      el("td", { class: "actions" }, el("button", {
        class: "btn danger",
        onclick: async () => {
          if (!confirm(`End ${s.username}'s session on ${s.connection}?`)) return;
          adminError(await api(`/api/admin/sessions/${s.id}/end`, {}));
          loadSessions();
        },
      }, "End")),
    ));
  }
}

// ---------- traffic ----------

const fmtBytes = (n) => {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  while (n >= 1000 && i < units.length - 1) { n /= 1000; i++; }
  return `${n < 10 && i ? n.toFixed(1) : Math.round(n)} ${units[i]}`;
};
const fmtDuration = (s) => {
  const h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60);
  return h ? `${h} h ${m} min` : `${m} min`;
};
const fmtRate = (bytes, s) => (s > 0 ? `${Math.round((bytes * 8) / s / 1000)} kbps` : "–");

async function loadTraffic() {
  const r = await api("/api/admin/traffic");
  if (!adminError(r)) return;
  const body = $("traffic-body");
  body.replaceChildren();
  const rows = r.data.rows;
  if (!rows.length) {
    body.append(el("tr", {}, el("td", { colspan: "7", class: "note" }, "No traffic in the last 24 hours.")));
    return;
  }
  // One line per user (busiest first), then one per computer when a user used several.
  const users = new Map();
  for (const row of rows) {
    const u = users.get(row.username) || { username: row.username, out: 0, in: 0, s: 0, parts: [] };
    u.out += row.bytes_out;
    u.in += row.bytes_in;
    u.s += row.seconds;
    u.parts.push(row);
    users.set(row.username, u);
  }
  const sorted = [...users.values()].sort((a, b) => b.out + b.in - (a.out + a.in));
  const line = (name, computer, out, inn, s, sub) => el("tr", sub ? { class: "sub" } : {},
    el("td", {}, name),
    el("td", {}, computer),
    el("td", {}, fmtBytes(out)),
    el("td", {}, fmtBytes(inn)),
    el("td", {}, sub ? fmtBytes(out + inn) : el("strong", {}, fmtBytes(out + inn))),
    el("td", {}, fmtDuration(s)),
    el("td", {}, fmtRate(out + inn, s)),
  );
  const total = { out: 0, in: 0, s: 0 };
  for (const u of sorted) {
    const computer = u.parts.length === 1 ? (u.parts[0].connection || "(removed)") : `${u.parts.length} computers`;
    body.append(line(u.username, computer, u.out, u.in, u.s, false));
    if (u.parts.length > 1) {
      for (const p of u.parts) body.append(line("", p.connection || "(removed)", p.bytes_out, p.bytes_in, p.seconds, true));
    }
    total.out += u.out; total.in += u.in; total.s += u.s;
  }
  body.append(el("tr", { class: "total" },
    el("td", {}, el("strong", {}, "All users")), el("td", {}, ""),
    el("td", {}, fmtBytes(total.out)), el("td", {}, fmtBytes(total.in)),
    el("td", {}, el("strong", {}, fmtBytes(total.out + total.in))),
    el("td", {}, fmtDuration(total.s)), el("td", {}, "")));
}
