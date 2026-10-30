// Tinlink desktop UI. All network access and media run in the Rust core;
// this page renders state and sends commands. No framework, no build step.
"use strict";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);

function el(tag, props = {}, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(props)) {
    if (key === "class") node.className = value;
    else if (key === "dataset") Object.assign(node.dataset, value);
    else if (key.startsWith("on")) node.addEventListener(key.slice(2), value);
    else if (value !== undefined && value !== null && value !== false) node[key] = value;
  }
  node.append(...children.filter((c) => c !== null && c !== undefined));
  return node;
}

function setStatus(text, isError = false) {
  $("status").textContent = text;
  $("status").classList.toggle("error", isError);
  if (!isError && text) {
    clearTimeout(setStatus.timer);
    setStatus.timer = setTimeout(() => {
      if ($("status").textContent === text) $("status").textContent = "";
    }, 5000);
  }
}

/** Calls a command and reports failures in the status line. */
async function run(fn) {
  try {
    return await fn();
  } catch (e) {
    setStatus(String(e), true);
    return undefined;
  }
}

const api = (method, path, body) => invoke("api", { method, path, body });

// Remembered settings. Storage may be unavailable; the UI works without it.
const settings = {
  load() {
    try {
      return JSON.parse(localStorage.getItem("tinlink.settings")) || {};
    } catch {
      return {};
    }
  },
  save(patch) {
    try {
      localStorage.setItem("tinlink.settings", JSON.stringify({ ...settings.load(), ...patch }));
    } catch {
      // ignore
    }
  },
};

/** Permission sets arrive as "VIEW_CHANNEL | SEND_MESSAGES". */
function can(perms, flag) {
  if (!perms) return false;
  const set = perms.split("|").map((p) => p.trim());
  return set.includes(flag) || set.includes("ADMINISTRATOR");
}

const state = {
  account: null,
  groups: [],
  group: null, // detail incl. permissions
  members: new Map(), // user id -> member
  categories: [],
  channels: [],
  voice: [], // { channel_id, user_id }
  channel: null,
  messages: [],
  loadingOlder: false,
  noOlder: false,
  voiceChannel: null, // channel we are connected to (or a dev room name)
  snapshot: null, // last room-state from the core
  pendingFiles: [],
  view: "group", // "group" or "dms"
  dms: [], // direct message channels, most recent first
};

// ---------------------------------------------------------------- dialog

/**
 * Shows a small form. `fields`: [{ id, label, type, value, placeholder }].
 * `submit(values)` may return an error string to keep the dialog open.
 */
function ask({ title, fields = [], text, ok = "OK", cancel = true, submit }) {
  return new Promise((resolve) => {
    const dialog = $("dialog");
    $("dialog-title").textContent = title;
    $("dialog-error").textContent = "";
    $("dialog-ok").textContent = ok;
    $("dialog-cancel").hidden = !cancel;
    const body = $("dialog-body");
    body.replaceChildren();
    if (text) body.append(text instanceof Node ? text : el("p", { textContent: text }));
    const inputs = {};
    for (const f of fields) {
      let input;
      if (f.options) {
        input = el("select", {}, ...f.options.map(([value, label]) => el("option", { value, textContent: label })));
        if (f.value) input.value = f.value;
      } else if (f.type === "checkbox") {
        input = el("input", { type: "checkbox", checked: !!f.value });
      } else {
        input = el("input", { type: f.type || "text", value: f.value || "", placeholder: f.placeholder || "" });
      }
      inputs[f.id] = input;
      body.append(f.type === "checkbox"
        ? el("label", { class: "inline" }, input, ` ${f.label}`)
        : el("label", {}, f.label, input));
    }
    const finish = (result) => {
      dialog.close();
      $("dialog-form").onsubmit = null;
      $("dialog-cancel").onclick = null;
      resolve(result);
    };
    $("dialog-form").onsubmit = async (ev) => {
      ev.preventDefault();
      const values = {};
      for (const [id, input] of Object.entries(inputs)) {
        values[id] = input.type === "checkbox"
          ? input.checked
          : input.type === "password" ? input.value : input.value.trim();
      }
      if (submit) {
        $("dialog-ok").disabled = true;
        const error = await submit(values);
        $("dialog-ok").disabled = false;
        if (error) {
          $("dialog-error").textContent = error;
          return;
        }
      }
      finish(values);
    };
    $("dialog-cancel").onclick = () => finish(null);
    dialog.showModal();
    Object.values(inputs)[0]?.focus();
  });
}

/** Runs `fn` and turns a failure into the dialog's error text. */
async function tryText(fn) {
  try {
    await fn();
    return null;
  } catch (e) {
    return String(e);
  }
}

// ----------------------------------------------------------------- login

let registering = false;

function showLogin(error = "") {
  $("main-view").hidden = true;
  $("login-view").hidden = false;
  $("login-error").textContent = error;
  const saved = settings.load();
  if (saved.server) $("login-server").value = saved.server;
  if (saved.userName) $("login-name").value = saved.userName;
  $("login-password").value = "";
}

$("login-toggle").addEventListener("click", () => {
  registering = !registering;
  $("display-name-row").hidden = !registering;
  $("login-submit").textContent = registering ? "Create account" : "Log in";
  $("login-toggle").textContent = registering ? "I already have an account" : "Create an account";
  $("login-password").autocomplete = registering ? "new-password" : "current-password";
});

$("login-form").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  $("login-error").textContent = "";
  $("login-submit").disabled = true;
  const server = $("login-server").value.trim();
  const name = $("login-name").value.trim();
  const password = $("login-password").value;
  try {
    const account = registering
      ? await invoke("register", { server, name, password, displayName: $("login-display").value.trim() || null })
      : await invoke("login", { server, name, password });
    settings.save({ server, userName: name });
    await showMain(account);
  } catch (e) {
    $("login-error").textContent = String(e);
  } finally {
    $("login-submit").disabled = false;
  }
});

$("dev-form").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  const room = $("dev-room").value.trim();
  const ok = await run(() => invoke("dev_join", {
    server: $("dev-server").value.trim(),
    name: $("dev-name").value.trim(),
    room,
  }));
  if (ok) {
    unlockAudio();
    await showMain(null);
    voiceJoined({ id: room, name: room, dev: true });
  }
});

$("logout").addEventListener("click", async () => {
  cancelRejoin();
  settings.save({ lastVoice: null });
  await run(() => invoke("logout"));
  voiceLeft();
  state.account = null;
  state.keyring = null;
  showLogin();
});

// ------------------------------------------------------------- encryption
// Same keys and flows as the web client, with its code (ui/shared is
// copied from apps/web at build time); the Rust core only gets media keys.

const e2ee = await import("./shared/e2ee.js");
const { createKeyring } = await import("./shared/keyring.js");

/** The core reports API errors as text; the keyring needs to see conflicts. */
async function keyringApi(method, path, body) {
  try {
    return await api(method, path, body);
  } catch (e) {
    const error = new Error(String(e));
    if (/already exists|does not follow the newest epoch/.test(error.message)) error.status = 409;
    throw error;
  }
}

/** Sets up this device's keys after login; asks for what is missing. */
async function setupEncryption() {
  const keyring = createKeyring({
    api: keyringApi,
    userId: myId(),
    onWarning: (userId) => setStatus(
      `The security key of ${displayNameOf(userId, "a member")} changed. Their devices get no keys until you check this with them.`, true),
  });
  state.keyring = keyring;
  try {
    let status = await keyring.setup("Tinlink desktop on Windows");
    if (status === "revoked") status = await keyring.setup("Tinlink desktop on Windows");
    if (state.keyring !== keyring) return;
    if (status === "new_identity") await showRecoveryKey(await keyring.createIdentity());
    else if (status === "pending") await approveThisDevice();
    if (keyring.approved) {
      await approvePendingDevices();
      shareKeysSoon();
    }
  } catch (e) {
    console.warn("encryption setup failed", e);
    setStatus(`Encryption is not available: ${e.message || e}`, true);
  }
}

async function showRecoveryKey(text) {
  await ask({
    title: "Your recovery key",
    text: el("div", {},
      el("p", { textContent: "Tinlink encrypts voice and messages end to end. This key restores your access on a new device or browser. Keep it in your password manager; nobody else, not even the server, can recover it for you." }),
      el("p", {}, el("code", { class: "code", textContent: text }))),
    fields: [{ id: "saved", label: "I have saved my recovery key", type: "checkbox" }],
    cancel: false,
    ok: "Done",
    submit: (v) => (v.saved ? null : "Please save the key first."),
  });
}

/** Asks for the recovery key, or for approval on another device. */
async function approveThisDevice() {
  const keyring = state.keyring;
  const code = await keyring.code();
  await ask({
    title: "Approve this device",
    text: el("div", {},
      el("p", { textContent: "Your account is protected with end-to-end encryption. Enter your recovery key, or approve this device in Tinlink on a device or browser that is already approved. It shows this code:" }),
      el("p", {}, el("code", { class: "code", textContent: code }))),
    fields: [{ id: "recovery", label: "Recovery key", placeholder: "XXXX XXXX …" }],
    ok: "Approve",
    submit: (v) => {
      if (keyring.approved) return null;
      if (!v.recovery) return "Enter the recovery key, or approve on another device.";
      return tryText(() => keyring.useRecoveryKey(v.recovery));
    },
  });
  if (keyring.approved) {
    setStatus("This device is approved for encryption");
    shareKeysSoon();
  }
}

/** Offers to approve own devices that wait for it. */
async function approvePendingDevices({ explicit = false } = {}) {
  const keyring = state.keyring;
  if (!keyring?.canApprove) {
    if (explicit) setStatus("Only a device that was set up with the recovery key can approve others.", true);
    return;
  }
  const pending = await keyring.pendingDevices();
  if (!pending.length && explicit) setStatus("No device is waiting for approval.");
  for (const d of pending) {
    const yes = await ask({
      title: "Approve a new device?",
      text: el("div", {},
        el("p", { textContent: `"${d.name}" wants to read your encrypted channels. Approve it only if it shows this code:` }),
        el("p", {}, el("code", { class: "code", textContent: d.code }))),
      ok: "Approve",
    });
    if (yes) await run(() => keyring.approveDevice(d));
  }
}

async function onKeysChanged(userId) {
  const keyring = state.keyring;
  if (!keyring) return;
  if (userId === myId()) {
    const wasApproved = keyring.approved;
    const status = await keyring.refresh().catch(() => null);
    if (!wasApproved && status === "ready") {
      // Approved from another device while the dialog is open.
      if ($("dialog").open && $("dialog-title").textContent === "Approve this device") $("dialog-cancel").click();
      setStatus("This device is approved for encryption");
    }
    if (status === "revoked") setupEncryption();
    else if (keyring.approved) approvePendingDevices();
  }
  shareKeysSoon();
}

async function onChannelKeysAdded(channelId) {
  if (voiceEncrypted && state.voiceChannel?.id === channelId) {
    await run(async () => {
      await state.keyring.load(channelId);
      await invoke("set_media_keys", { keys: await mediaKeysOf(channelId) });
    });
  }
  shareKeysSoon();
}

/**
 * Shares channel keys with new members and devices. Every approved device
 * does this; the server ignores keys someone else delivered first.
 */
function shareKeysSoon() {
  clearTimeout(shareKeysSoon.timer);
  shareKeysSoon.timer = setTimeout(async () => {
    const keyring = state.keyring;
    if (!keyring?.approved) return;
    const channels = [...state.channels.map((c) => c.id), ...state.dms.map((d) => d.id)];
    for (const id of channels) {
      await keyring.shareMissing(id).catch((e) => console.warn("sharing keys failed", id, e));
    }
  }, 1500 + Math.random() * 1500);
}

// ------------------------------------------------------------------ main

async function showMain(account) {
  state.account = account;
  $("login-view").hidden = true;
  $("main-view").hidden = false;
  renderMe();
  $("groups").hidden = !account;
  $("profile").hidden = !account;
  $("logout").textContent = account ? "Log out" : "Back";
  await loadScreens();
  await loadCameras();
  $("dm-button").parentElement.hidden = !account;
  if (!account) return;
  setupEncryption();
  await loadGroups();
  await loadDms();
  const saved = settings.load();
  const groupId = state.groups.some((g) => g.id === saved.groupId) ? saved.groupId : state.groups[0]?.id;
  if (groupId) await selectGroup(groupId);
  else renderEmptyGroup();
  await rejoinLastVoice();
}

function renderEmptyGroup() {
  state.group = null;
  $("group-name").textContent = "No group yet";
  $("group-invite").hidden = true;
  $("group-settings").hidden = true;
  closeSettings();
  $("channel-tree").replaceChildren(
    el("p", { class: "muted", style: "padding: 0 12px", textContent: "Create a group or join one with an invite code (+ on the left)." }),
  );
  openChannel(null);
}

async function loadGroups() {
  state.groups = (await run(() => api("GET", "/groups"))) || [];
  renderGroups();
}

function renderGroups() {
  $("group-list").replaceChildren(...state.groups.map((g) => el("li", {},
    g.unread && g.id !== state.group?.id ? el("span", { class: "unread-dot" }) : null,
    el("button", {
      type: "button",
      class: g.id === state.group?.id && state.view === "group" ? "active" : "",
      title: g.name,
      onclick: () => selectGroup(g.id),
    }, g.icon_id ? imageEl(g.icon_id) : initials(g.name)),
    g.mention_count ? el("span", { class: "mention-badge", textContent: String(g.mention_count) }) : null)));
}

// ------------------------------------------------------- read state

const myId = () => state.account?.user.id;
const channelById = (id) => state.channels.find((c) => c.id === id);
/** UUIDv7 ids are time ordered, so string comparison orders them. */
const isUnread = (ch) => !!ch.last_message_id && (!ch.last_read_id || ch.last_message_id > ch.last_read_id);

/** Derives the current group's unread state from its channels. */
function updateGroupBadges() {
  const g = state.groups.find((x) => x.id === state.group?.id);
  if (g) {
    g.unread = state.channels.some(isUnread);
    g.mention_count = state.channels.reduce((n, c) => n + (c.mention_count || 0), 0);
  }
  renderGroups();
}

let readTimer = null;
/** Marks the open channel read shortly after it is seen with focus. */
function scheduleMarkRead() {
  clearTimeout(readTimer);
  readTimer = setTimeout(markRead, 600);
}

async function markRead() {
  const ch = state.channel;
  const last = state.messages[state.messages.length - 1];
  if (!ch || !last || !document.hasFocus()) return;
  const entry = channelById(ch.id);
  if (entry && !isUnread(entry) && !entry.mention_count && entry.last_read_id >= last.id) return;
  const known = dmById(ch.id);
  if (known && !known.unread_count && known.last_read_id >= last.id) return;
  try {
    await api("PUT", `/channels/${ch.id}/read`, { message_id: last.id });
  } catch {
    return;
  }
  if (entry) {
    if (!entry.last_message_id || entry.last_message_id < last.id) entry.last_message_id = last.id;
    entry.last_read_id = last.id;
    entry.mention_count = 0;
  }
  const dm = dmById(ch.id);
  if (dm) {
    dm.last_read_id = last.id;
    dm.unread_count = 0;
    renderDmBadge();
  }
  renderTree();
  updateGroupBadges();
}

window.addEventListener("focus", scheduleMarkRead);

/** A new message somewhere: unread state, badges and mention notification. */
function trackNewMessage(e) {
  const m = e.data;
  if (typingUsers.delete(m.author_id)) renderTyping();
  if (!e.group_id) return trackDmMessage(m);
  const mine = m.author_id === myId();
  const mentioned = (m.mentions || []).includes(myId());
  const seen = m.channel_id === state.channel?.id && document.hasFocus();
  if (state.group && e.group_id === state.group.id) {
    const ch = channelById(m.channel_id);
    if (ch) {
      if (!ch.last_message_id || ch.last_message_id < m.id) ch.last_message_id = m.id;
      if (mine) ch.last_read_id = m.id;
      else if (mentioned && !seen) ch.mention_count = (ch.mention_count || 0) + 1;
      renderTree();
    }
    updateGroupBadges();
  } else if (!mine) {
    const g = state.groups.find((x) => x.id === e.group_id);
    if (g) {
      g.unread = true;
      if (mentioned) g.mention_count = (g.mention_count || 0) + 1;
      renderGroups();
    }
  }
  if (mentioned && !mine && !seen) {
    const ch = channelById(m.channel_id);
    const where = ch ? `#${ch.name}` : state.groups.find((x) => x.id === e.group_id)?.name || "Tinlink";
    const body = m.content || (m.attachments?.length ? "sent a file" : "");
    invoke("notify", { title: `${m.author_name || "Someone"} mentioned you in ${where}`, body: body.slice(0, 200) });
  }
}

function initials(name) {
  return name.split(/\s+/).filter(Boolean).slice(0, 2).map((w) => w[0].toUpperCase()).join("") || "?";
}

// ------------------------------------------------- avatars and icons

/** Edge length of uploaded icons; the server accepts up to 256 KB. */
const ICON_SIZE = 128;
/** Image id -> promise of a data URL. Ids never change their content. */
const imageCache = new Map();

/** An <img> for a server image; the CSP only allows data URLs, so the
 * backend fetches it. */
function imageEl(id, props = {}) {
  if (!imageCache.has(id)) {
    imageCache.set(id, invoke("image_data", { url: `/api/images/${id}` }).catch((e) => {
      imageCache.delete(id);
      throw e;
    }));
  }
  const img = el("img", { alt: "", ...props });
  imageCache.get(id).then((src) => { img.src = src; }).catch(() => {});
  return img;
}

function hue(text) {
  let h = 0;
  for (const c of text) h = (h * 31 + c.codePointAt(0)) % 360;
  return h;
}

/** A round avatar: the picture, or the first letter on a colored circle.
 * `size` is "small", "medium" or "large". */
function avatar(user, size = "small") {
  const name = user?.display_name || user?.name || "?";
  if (user?.avatar_id) return el("span", { class: `avatar ${size}` }, imageEl(user.avatar_id));
  const node = el("span", { class: `avatar ${size}`, textContent: initials(name).slice(0, 1) });
  node.style.background = `hsl(${hue(user?.name || name)} 45% 45%)`;
  return node;
}

/** The channel's icon image, emoji or a plain `#`. */
function channelIcon(ch) {
  if (ch.icon_id) return imageEl(ch.icon_id, { class: "channel-icon" });
  return el("span", { class: "channel-icon", textContent: ch.emoji || "#" });
}

/** What we know about a user: name, display name and avatar. */
function userInfo(id) {
  if (!id) return null;
  if (id === myId()) return state.account.user;
  return state.members.get(id) || state.dms.find((d) => d.user.id === id)?.user || null;
}

/** Lets the user pick an image, crops it to a centered square and scales
 * it to a data URL; null when cancelled. */
function pickImage() {
  return new Promise((resolve) => {
    const input = el("input", { type: "file", accept: "image/png,image/jpeg,image/gif,image/webp" });
    input.addEventListener("change", () => resolve(input.files[0] || null));
    input.addEventListener("cancel", () => resolve(null));
    input.click();
  });
}

async function scaleImage(file) {
  const bitmap = await createImageBitmap(file).catch(() => {
    throw new Error("This file is not an image that can be read.");
  });
  const side = Math.min(bitmap.width, bitmap.height);
  const canvas = el("canvas");
  canvas.width = canvas.height = Math.min(ICON_SIZE, side);
  canvas.getContext("2d").drawImage(bitmap,
    (bitmap.width - side) / 2, (bitmap.height - side) / 2, side, side,
    0, 0, canvas.width, canvas.height);
  bitmap.close();
  const webp = canvas.toDataURL("image/webp", 0.9);
  return webp.startsWith("data:image/webp") ? webp : canvas.toDataURL("image/png");
}

/** Picks, scales and uploads an image with `upload(dataUrl)`. */
async function chooseAndUpload(upload) {
  const file = await pickImage();
  if (file) await upload(await scaleImage(file));
}

/** Preview plus "Choose…" and "Remove" buttons for an icon at `path`. */
function iconPicker(iconId, fallback, path, label) {
  const set = async (image) => {
    if (await run(() => api("PUT", path, { image }))) settingsChanged(image ? `${label} saved` : `${label} removed`);
  };
  return el("span", { class: "icon-picker" },
    iconId ? imageEl(iconId, { class: "icon-preview" }) : el("span", { class: "icon-preview", textContent: fallback }),
    el("button", { type: "button", class: "small", textContent: "Choose…", onclick: () => run(() => chooseAndUpload(set)) }),
    iconId ? el("button", { type: "button", class: "small", textContent: "Remove", onclick: () => set(null) }) : null);
}

function renderMe() {
  const account = state.account;
  $("me").replaceChildren(...(account
    ? [avatar(account.user), el("span", { class: "ellipsis", textContent: `${account.user.display_name || account.user.name} @ ${new URL(account.server).host}` })]
    : ["Developer mode"]));
}

/** A user changed their name or avatar (possibly we, in another session). */
function onUserUpdated(user) {
  const profile = { display_name: user.display_name, avatar_id: user.avatar_id };
  if (user.id === myId()) {
    Object.assign(state.account.user, profile);
    renderMe();
  }
  const member = state.members.get(user.id);
  if (member) Object.assign(member, profile);
  for (const dm of state.dms) if (dm.user.id === user.id) Object.assign(dm.user, profile);
  renderTree();
  for (const m of state.messages) {
    if (m.author_id === user.id) {
      m.author_name = user.display_name;
      replaceMessage(m);
    }
  }
}

$("group-add").addEventListener("click", async () => {
  const values = await ask({
    title: "Create or join a group",
    fields: [
      { id: "name", label: "New group name", placeholder: "e.g. Friday gaming" },
      { id: "code", label: "…or invite code", placeholder: "code from an invite" },
    ],
    submit: (v) => {
      if (!v.name && !v.code) return "Enter a name or an invite code.";
      return tryText(async () => {
        const group = v.code
          ? await api("POST", `/invites/${encodeURIComponent(v.code)}/join`)
          : await api("POST", "/groups", { name: v.name });
        await loadGroups();
        await selectGroup(group.id);
      });
    },
  });
  if (values) setStatus(values.code ? "Joined the group" : "Group created");
});

$("group-invite").addEventListener("click", async () => {
  const invite = await run(() => api("POST", `/groups/${state.group.id}/invites`, {}));
  if (!invite) return;
  await ask({
    title: "Invite code",
    text: el("div", {},
      el("p", { textContent: "Share this code; others enter it under + on the left." }),
      el("div", { class: "code", textContent: invite.code })),
    cancel: false,
  });
});

async function selectGroup(id) {
  const [group, members, channels] = await Promise.all([
    run(() => api("GET", `/groups/${id}`)),
    run(() => api("GET", `/groups/${id}/members`)),
    run(() => api("GET", `/groups/${id}/channels`)),
  ]);
  if (!group) return;
  closeSettings();
  state.view = "group";
  state.group = group;
  renderDmBadge();
  settings.save({ groupId: id });
  setMembers(members || []);
  setChannels(channels);
  renderGroups();
  $("group-name").textContent = group.name;
  $("group-invite").hidden = !can(group.permissions, "MANAGE_GROUP");
  updateSettingsButton();
  const saved = settings.load().channels || {};
  const channel = state.channels.find((c) => c.id === saved[id]) || state.channels[0] || null;
  openChannel(channel);
}

function setMembers(members) {
  state.members = new Map(members.map((m) => [m.user_id, m]));
}

function setChannels(list) {
  state.categories = (list?.categories || []).sort((a, b) => a.position - b.position);
  state.channels = (list?.channels || []).sort((a, b) => a.position - b.position || a.name.localeCompare(b.name));
  state.voice = list?.voice || [];
  renderTree();
  updateGroupBadges();
}

/** Reloads channels and members of the current group (debounced). */
function refreshGroup() {
  clearTimeout(refreshGroup.timer);
  refreshGroup.timer = setTimeout(async () => {
    if (!state.group) return;
    const id = state.group.id;
    const [group, members, channels] = await Promise.all([
      run(() => api("GET", `/groups/${id}`)),
      run(() => api("GET", `/groups/${id}/members`)),
      run(() => api("GET", `/groups/${id}/channels`)),
    ]);
    if (state.group?.id !== id || !group) return;
    state.group = group;
    if (state.view === "group") $("group-name").textContent = group.name;
    updateSettingsButton();
    if (settingsOpen()) renderSettings();
    setMembers(members || []);
    setChannels(channels);
    if (state.channel && !state.channel.dm && !state.channels.some((c) => c.id === state.channel.id)) {
      openChannel(state.channels[0] || null);
    }
  }, 150);
}

// --------------------------------------------------------------- channels

function memberName(userId) {
  const m = state.members.get(userId);
  return m ? m.display_name || m.name : "unknown";
}

function renderTree() {
  if (state.view === "dms") return renderDmList();
  const tree = $("channel-tree");
  const groupPerms = state.group?.permissions;
  const sections = [{ id: null, name: "Channels", allow_user_channels: false }, ...state.categories];
  const nodes = [];
  for (const cat of sections) {
    const channels = state.channels.filter((c) => (c.category_id ?? null) === cat.id);
    const canCreate = can(groupPerms, "MANAGE_CHANNELS")
      || (cat.allow_user_channels && can(groupPerms, "CREATE_OWN_CHANNELS"));
    if (!channels.length && !canCreate && cat.id === null) continue;
    nodes.push(el("div", { class: "category" },
      el("span", { textContent: cat.name }),
      canCreate ? el("button", {
        type: "button", class: "small", title: "Create channel", textContent: "+",
        onclick: () => createChannel(cat),
      }) : null));
    for (const ch of channels) nodes.push(...channelNodes(ch));
  }
  nodes.push(...memberNodes());
  tree.replaceChildren(...nodes);
  updateSpeaking();
}

/** Group members, online first, with a "Message" action. */
function memberNodes() {
  const members = [...state.members.values()].sort((a, b) =>
    (b.online === true) - (a.online === true)
      || (a.display_name || a.name).localeCompare(b.display_name || b.name));
  if (!members.length) return [];
  const online = members.filter((m) => m.online).length;
  return [
    el("div", { class: "category" }, el("span", { textContent: `Members — ${online} online` })),
    ...members.map((m) => el("div", { class: `member${m.online ? "" : " offline"}`, title: `@${m.name}` },
      el("span", { class: `dot${m.online ? " online" : ""}` }),
      avatar(m),
      el("span", { class: "who-name", textContent: m.user_id === myId() ? `${m.display_name || m.name} (you)` : m.display_name || m.name }),
      m.user_id === myId() ? null : el("button", {
        type: "button", class: "small", textContent: "Message",
        onclick: () => openDmWith(m.user_id),
      }))),
  ];
}

// ------------------------------------------------------- direct messages

const dmById = (id) => state.dms.find((d) => d.id === id);
const dmName = (dm) => dm.user.display_name || dm.user.name;

async function loadDms() {
  state.dms = (await api("GET", "/dms").catch(() => null)) || [];
  renderDmBadge();
  if (state.view === "dms") renderDmList();
}

function renderDmBadge() {
  const unread = state.dms.reduce((n, d) => n + (d.unread_count || 0), 0);
  $("dm-badge").hidden = !unread;
  $("dm-badge").textContent = String(unread);
  $("dm-button").classList.toggle("active", state.view === "dms");
}

function renderDmList() {
  const rows = state.dms.map((dm) => el("div", {
    class: ["dm", dm.id === state.channel?.id && "active", dm.unread_count && "unread"].filter(Boolean).join(" "),
    title: `@${dm.user.name}`,
    onclick: () => openDm(dm),
  },
  el("span", { class: `dot${dm.user.online ? " online" : ""}` }),
  avatar(dm.user),
  el("span", { class: "who-name", textContent: dmName(dm) }),
  dm.unread_count ? el("span", { class: "mention-badge", textContent: String(dm.unread_count) }) : null));
  $("channel-tree").replaceChildren(...(rows.length ? rows : [
    el("p", { class: "muted dm-empty", textContent: "No conversations yet. Use \"Message\" next to a group member." }),
  ]));
  renderDmBadge();
}

/** Switches the sidebar to the direct message list. */
async function showDms(openId) {
  state.view = "dms";
  updateSettingsButton();
  $("group-name").textContent = "Direct messages";
  $("group-invite").hidden = true;
  renderGroups();
  await loadDms();
  renderDmList();
  const dm = dmById(openId || settings.load().dmId) || state.dms[0] || null;
  if (dm) await openDm(dm);
  else openChannel(null);
}

function openDm(dm) {
  settings.save({ dmId: dm.id });
  return openChannel({ id: dm.id, name: dmName(dm), dm: true, permissions: "SEND_MESSAGES | ATTACH_FILES" });
}

/** Opens (or creates) the conversation with a group member. */
async function openDmWith(userId) {
  const dm = await run(() => api("POST", "/dms", { user_id: userId }));
  if (dm) await showDms(dm.id);
}

$("dm-button").addEventListener("click", () => showDms());

/** A message in a direct conversation: unread count, order, notification. */
async function trackDmMessage(m) {
  let dm = dmById(m.channel_id);
  // A new conversation someone else started: the fresh list already counts it.
  const fresh = !dm;
  if (fresh) {
    await loadDms();
    dm = dmById(m.channel_id);
  }
  const mine = m.author_id === myId();
  const seen = m.channel_id === state.channel?.id && document.hasFocus();
  if (dm && !fresh) {
    if (!dm.last_message_id || dm.last_message_id < m.id) dm.last_message_id = m.id;
    if (mine) dm.last_read_id = m.id;
    else if (!seen && !(dm.last_read_id && dm.last_read_id >= m.id)) dm.unread_count = (dm.unread_count || 0) + 1;
    state.dms.sort((a, b) => (b.last_message_id || "").localeCompare(a.last_message_id || ""));
  }
  renderDmBadge();
  if (state.view === "dms") renderDmList();
  if (!mine && !seen) {
    const body = m.content || (m.attachments?.length ? "sent a file" : "");
    invoke("notify", { title: `Message from ${m.author_name || "someone"}`, body: body.slice(0, 200) });
  }
}

// ------------------------------------------------------------- search

const search = { query: "", hits: [] };

/** Lowercased words of a query (letters and digits). */
function searchTerms(query) {
  return query.toLowerCase().split(/[^\p{L}\p{N}]+/u).filter(Boolean);
}

/**
 * Ranges [start, end) in `text` where a word starts with one of the terms,
 * sorted and without overlaps (the server matches every word as a prefix).
 */
function highlightRanges(text, terms) {
  const ranges = [];
  const lower = text.toLowerCase();
  for (const term of terms) {
    let i = lower.indexOf(term);
    while (i >= 0) {
      if (i === 0 || !/[\p{L}\p{N}]/u.test(lower[i - 1])) ranges.push([i, i + term.length]);
      i = lower.indexOf(term, i + 1);
    }
  }
  ranges.sort((a, b) => a[0] - b[0] || b[1] - a[1]);
  const merged = [];
  for (const r of ranges) {
    const last = merged[merged.length - 1];
    if (last && r[0] <= last[1]) last[1] = Math.max(last[1], r[1]);
    else merged.push([...r]);
  }
  return merged;
}

function closeSearch() {
  $("search-results").hidden = true;
  $("messages").hidden = false;
}

async function runSearch(query, more = false) {
  const before = more ? search.hits[search.hits.length - 1]?.id : null;
  const params = new URLSearchParams({ q: query, limit: "50" });
  if (before) params.set("before", before);
  const page = await run(() => api("GET", `/search?${params}`));
  if (!page) return;
  search.query = query;
  search.hits = more ? [...search.hits, ...page] : page;
  const terms = searchTerms(query);
  const hits = page.map((h) => renderHit(h, terms));
  if (more) $("search-hits").append(...hits);
  else $("search-hits").replaceChildren(...hits);
  $("search-summary").textContent = search.hits.length
    ? `${search.hits.length}${page.length === 50 ? "+" : ""} result${search.hits.length === 1 && page.length < 50 ? "" : "s"} for “${query}”`
    : `No results for “${query}”`;
  $("search-more").hidden = page.length < 50;
  $("messages").hidden = true;
  $("search-results").hidden = false;
}

/** Text with the matched word starts wrapped in <mark>, built from DOM nodes. */
function highlighted(text, terms) {
  const nodes = [];
  let pos = 0;
  for (const [start, end] of highlightRanges(text, terms)) {
    if (start > pos) nodes.push(text.slice(pos, start));
    nodes.push(el("mark", { textContent: text.slice(start, end) }));
    pos = end;
  }
  nodes.push(text.slice(pos));
  return nodes;
}

function renderHit(h, terms) {
  const group = h.group_id ? state.groups.find((g) => g.id === h.group_id) : null;
  const where = h.group_id ? `# ${h.channel_name}${group ? ` · ${group.name}` : ""}` : `@ ${h.channel_name}`;
  return el("button", { type: "button", class: "hit", onclick: () => openHit(h) },
    el("div", { class: "meta" },
      el("span", { class: "where", textContent: where }),
      el("strong", { textContent: h.author_name || "deleted user" }),
      el("span", { textContent: ` ${formatTime(h.created_at)}` })),
    el("div", { class: "text" }, ...highlighted(h.content || "(attachment)", terms)));
}

/** Opens the channel of a search hit and scrolls to the message. */
async function openHit(h) {
  if (!h.group_id) {
    if (!dmById(h.channel_id)) await loadDms();
    if (!dmById(h.channel_id)) return;
    await showDms(h.channel_id);
  } else {
    if (state.view !== "group" || state.group?.id !== h.group_id) await selectGroup(h.group_id);
    const ch = channelById(h.channel_id);
    if (!ch) return;
    if (state.channel?.id !== ch.id || !state.messages.length) await openChannel(ch);
    else closeSearch();
  }
  await jumpTo(h.id);
}

async function jumpTo(messageId) {
  // Page back through the history until the message is loaded.
  for (let i = 0; i < 20 && !state.messages.some((m) => m.id === messageId); i++) {
    if (state.noOlder || !state.messages.length) break;
    await loadMessages(state.messages[0].id);
  }
  const node = $("messages").querySelector(`[data-id="${messageId}"]`);
  if (!node) {
    setStatus("That message is too far back to show.");
    return;
  }
  node.scrollIntoView({ block: "center" });
  node.classList.add("highlight");
  setTimeout(() => node.classList.remove("highlight"), 2000);
}

$("search-input").addEventListener("keydown", (ev) => {
  if (ev.key === "Enter") {
    const query = $("search-input").value.trim();
    if (searchTerms(query).length) runSearch(query);
  } else if (ev.key === "Escape") {
    $("search-input").value = "";
    closeSearch();
  }
});
$("search-close").addEventListener("click", closeSearch);
$("search-more").addEventListener("click", () => runSearch(search.query, true));

// ------------------------------------------------------------- typing

const typingUsers = new Map(); // user id -> { name, until }
let typingSentAt = 0;

$("message-input").addEventListener("input", () => {
  const ch = state.channel;
  if (!ch || !$("message-input").value.trim() || Date.now() - typingSentAt < 3000) return;
  typingSentAt = Date.now();
  invoke("send_typing", { channelId: ch.id }).catch(() => {});
});

function displayNameOf(userId, fallback) {
  const m = state.members.get(userId);
  if (m) return m.display_name || m.name;
  const dm = state.dms.find((d) => d.user.id === userId);
  return dm ? dmName(dm) : fallback;
}

function renderTyping() {
  const now = Date.now();
  for (const [id, t] of typingUsers) if (t.until <= now) typingUsers.delete(id);
  const names = [...typingUsers.values()].map((t) => t.name);
  $("typing").textContent = names.length === 0 ? ""
    : names.length === 1 ? `${names[0]} is typing…`
      : `${names.slice(0, -1).join(", ")} and ${names[names.length - 1]} are typing…`;
}

function clearTyping() {
  typingUsers.clear();
  renderTyping();
}

setInterval(renderTyping, 1000);

function channelNodes(ch) {
  const canJoin = ch.voice_enabled && can(ch.permissions, "CONNECT");
  const classes = ["channel", ch.id === state.channel?.id && "active", isUnread(ch) && "unread"];
  const row = el("div", { class: classes.filter(Boolean).join(" ") },
    el("button", {
      type: "button", class: "name", title: ch.name,
      onclick: () => openChannel(ch),
    }, channelIcon(ch), el("span", { class: "ellipsis", textContent: `${ch.name}${ch.temporary ? " ⏱" : ""}` })),
    ch.mention_count ? el("span", { class: "mention-badge", textContent: String(ch.mention_count) }) : null,
    canJoin ? el("button", {
      type: "button", class: "join", title: "Join voice", textContent: "🔊",
      onclick: () => joinVoice(ch),
    }) : null);
  const users = state.voice.filter((v) => v.channel_id === ch.id);
  const list = users.length ? el("ul", { class: "voice-users" }, ...users.map((v) => {
    const m = state.members.get(v.user_id);
    const moderate = canModerate() && v.user_id !== state.account?.user.id;
    return el("li", {
      dataset: { user: m?.name || v.user_id },
      class: moderate ? "moderatable" : "",
      title: moderate ? "Click to moderate" : "",
      onclick: moderate ? () => moderateVoice(ch, v.user_id) : null,
    },
    el("span", { class: "dot" }),
    avatar(m),
    el("span", { textContent: memberName(v.user_id) }));
  })) : null;
  return [row, list].filter(Boolean);
}

function canModerate() {
  const perms = state.group?.permissions;
  return can(perms, "MUTE_MEMBERS") || can(perms, "MOVE_MEMBERS");
}

/** Server mute, disconnect or move a member in voice (the server checks roles). */
async function moderateVoice(ch, userId) {
  const perms = state.group.permissions;
  const actions = [];
  if (can(perms, "MUTE_MEMBERS")) actions.push(["mute", "Server mute"], ["unmute", "Remove server mute"]);
  if (can(perms, "MOVE_MEMBERS")) actions.push(["disconnect", "Disconnect from voice"], ["move", "Move to channel…"]);
  const targets = state.channels.filter((c) => c.voice_enabled && c.id !== ch.id).map((c) => [c.id, `# ${c.name}`]);
  const fields = [{ id: "action", label: "Action", options: actions }];
  if (targets.length && can(perms, "MOVE_MEMBERS")) fields.push({ id: "to", label: "Move to", options: targets });
  const groupId = state.group.id;
  await ask({
    title: `Moderate ${memberName(userId)}`,
    fields,
    submit: (v) => tryText(async () => {
      if (v.action === "mute" || v.action === "unmute") {
        await api("PUT", `/groups/${groupId}/members/${userId}/voice-mute`, { muted: v.action === "mute" });
      } else if (v.action === "disconnect") {
        await api("POST", `/channels/${ch.id}/voice/${userId}/disconnect`);
      } else if (v.action === "move") {
        await api("POST", `/channels/${ch.id}/voice/${userId}/move`, { to: v.to });
      }
      setStatus("Done");
    }),
  });
}

async function createChannel(category) {
  await ask({
    title: category.id ? `New channel in ${category.name}` : "New channel",
    fields: [
      { id: "name", label: "Name" },
      { id: "temporary", label: "Delete when everyone has left", type: "checkbox", value: !!category.allow_user_channels },
    ],
    submit: (v) => {
      if (!v.name) return "Enter a name.";
      return tryText(async () => {
        const ch = await api("POST", `/groups/${state.group.id}/channels`, {
          name: v.name,
          category_id: category.id,
          temporary: v.temporary,
        });
        refreshGroup();
        if (ch?.voice_enabled && v.temporary) await joinVoice(ch);
      });
    },
  });
}

// ------------------------------------------------------------------- chat

function openChannel(ch) {
  state.channel = ch;
  clearTyping();
  closeSettings();
  closeSearch();
  state.messages = [];
  state.noOlder = false;
  state.pendingFiles = [];
  renderPending();
  $("messages").replaceChildren();
  for (const node of document.querySelectorAll(".channel")) node.classList.remove("active");
  if (!ch) {
    $("channel-name").textContent = state.group ? "No channel" : "Select a channel";
    $("composer").hidden = true;
    return;
  }
  renderTree();
  if (state.group && !ch.dm) {
    const channels = settings.load().channels || {};
    settings.save({ channels: { ...channels, [state.group.id]: ch.id } });
  }
  $("channel-name").textContent = ch.dm ? `@ ${ch.name}` : `# ${ch.name}`;
  $("composer").hidden = !can(ch.permissions, "SEND_MESSAGES");
  $("attach").hidden = !can(ch.permissions, "ATTACH_FILES");
  return loadMessages();
}

async function loadMessages(before) {
  const ch = state.channel;
  if (!ch) return;
  const query = before ? `?limit=50&before=${before}` : "?limit=50";
  const page = await run(() => api("GET", `/channels/${ch.id}/messages${query}`));
  if (!page || state.channel !== ch) return;
  if (page.length < 50) state.noOlder = true;
  page.reverse();
  const box = $("messages");
  if (before) {
    const older = page.filter((m) => !state.messages.some((x) => x.id === m.id));
    const fromBottom = box.scrollHeight - box.scrollTop;
    state.messages = [...older, ...state.messages];
    box.prepend(...older.map(renderMessage));
    box.scrollTop = box.scrollHeight - fromBottom;
  } else {
    // Full reload (channel opened or resync): the page replaces everything.
    state.messages = page;
    box.replaceChildren(...page.map(renderMessage));
    box.scrollTop = box.scrollHeight;
    scheduleMarkRead();
  }
}

$("messages").addEventListener("scroll", async () => {
  const box = $("messages");
  if (box.scrollTop > 40 || state.loadingOlder || state.noOlder || !state.messages.length) return;
  state.loadingOlder = true;
  await loadMessages(state.messages[0].id);
  state.loadingOlder = false;
});

function formatTime(iso) {
  const d = new Date(iso);
  const sameDay = d.toDateString() === new Date().toDateString();
  return sameDay ? d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" }) : d.toLocaleString();
}

function formatSize(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(0)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

function renderMessage(m) {
  const mine = m.author_id === myId();
  const mentionsMe = (m.mentions || []).includes(myId());
  const canDelete = mine || can(state.channel?.permissions, "MANAGE_MESSAGES");
  const author = userInfo(m.author_id) || { name: m.author_name || "?" };
  const node = el("div", { class: `message${mentionsMe ? " mentions-me" : ""}`, dataset: { id: m.id } },
    avatar(author, "medium"),
    el("div", { class: "body" },
      el("div", { class: "meta" },
        el("strong", { textContent: author.display_name || m.author_name || "deleted user" }),
        el("span", { textContent: formatTime(m.created_at) + (m.edited_at ? " (edited)" : "") })),
      m.content ? el("div", { class: "text", textContent: m.content }) : null,
      ...(m.attachments || []).map(renderAttachment)));
  if (mine || canDelete) {
    node.append(el("span", { class: "actions" },
      mine ? el("button", { type: "button", class: "small", textContent: "Edit", onclick: () => editMessage(m, node) }) : null,
      canDelete ? el("button", { type: "button", class: "small", textContent: "Delete", onclick: () => deleteMessage(m) }) : null));
  }
  return node;
}

function editMessage(m, node) {
  if (node.querySelector(".edit-box")) return;
  const box = el("textarea", { class: "edit-box", rows: 2, value: m.content, maxLength: 4000 });
  const save = async () => {
    const content = box.value.trim();
    if (!content || content === m.content) return replaceMessage(m);
    const updated = await run(() => api("PATCH", `/channels/${m.channel_id}/messages/${m.id}`, { content }));
    if (updated) replaceMessage(updated);
  };
  box.addEventListener("keydown", (ev) => {
    if (ev.key === "Enter" && !ev.shiftKey) {
      ev.preventDefault();
      save();
    } else if (ev.key === "Escape") {
      replaceMessage(m);
    }
  });
  const form = el("div", {}, box,
    el("div", { class: "row" },
      el("button", { type: "button", class: "small primary", textContent: "Save", onclick: save }),
      el("button", { type: "button", class: "small", textContent: "Cancel", onclick: () => replaceMessage(m) }),
      el("span", { class: "muted", textContent: "Enter to save, Escape to cancel" })));
  const text = node.querySelector(".text");
  if (text) text.replaceWith(form);
  else node.querySelector(".meta").after(form);
  box.focus();
}

async function deleteMessage(m) {
  const confirmed = await ask({
    title: "Delete message?",
    text: m.content ? `"${m.content.slice(0, 120)}"` : "This message and its files will be deleted.",
    ok: "Delete",
  });
  if (!confirmed) return;
  if ((await run(() => api("DELETE", `/channels/${m.channel_id}/messages/${m.id}`))) !== undefined) removeMessage(m.id);
}

function renderAttachment(a) {
  const download = () => run(async () => {
    const path = await invoke("download", { url: a.url, filename: a.filename });
    setStatus(`Saved to ${path}`);
  });
  if (a.content_type.startsWith("image/")) {
    const img = el("img", { alt: a.filename, title: `${a.filename} – click to save`, onclick: download });
    invoke("image_data", { url: a.url }).then((src) => { img.src = src; }).catch(() => {
      img.replaceWith(fileButton(a, download));
    });
    return el("div", { class: "attachment" }, img);
  }
  return el("div", { class: "attachment" }, fileButton(a, download));
}

function fileButton(a, onclick) {
  return el("button", {
    type: "button", class: "small", onclick,
    textContent: `📎 ${a.filename} (${formatSize(a.size_bytes)})`,
  });
}

function addMessage(m) {
  if (m.channel_id !== state.channel?.id || state.messages.some((x) => x.id === m.id)) return;
  const box = $("messages");
  const atBottom = box.scrollHeight - box.scrollTop - box.clientHeight < 60;
  state.messages.push(m);
  box.append(renderMessage(m));
  if (atBottom) box.scrollTop = box.scrollHeight;
  scheduleMarkRead();
}

function replaceMessage(m) {
  const i = state.messages.findIndex((x) => x.id === m.id);
  if (i < 0) return;
  state.messages[i] = m;
  $("messages").querySelector(`[data-id="${m.id}"]`)?.replaceWith(renderMessage(m));
}

function removeMessage(id) {
  state.messages = state.messages.filter((x) => x.id !== id);
  $("messages").querySelector(`[data-id="${id}"]`)?.remove();
}

$("attach").addEventListener("click", () => $("file-input").click());
$("file-input").addEventListener("change", () => {
  state.pendingFiles.push(...$("file-input").files);
  $("file-input").value = "";
  renderPending();
});

function renderPending() {
  $("pending-files").textContent = state.pendingFiles.length
    ? `To send: ${state.pendingFiles.map((f) => f.name).join(", ")}`
    : "";
}

// @-autocomplete for member names in the message input.
const mentionPicker = { items: [], index: 0, start: -1 };

function updateMentionPicker() {
  const input = $("message-input");
  const caret = input.selectionStart;
  const match = /(^|[^A-Za-z0-9_.-])@([A-Za-z0-9_.-]{0,32})$/.exec(input.value.slice(0, caret));
  if (!match) return hideMentionPicker();
  const query = match[2].toLowerCase();
  mentionPicker.start = caret - query.length - 1;
  mentionPicker.items = [...state.members.values()]
    .filter((m) => m.user_id !== myId())
    .filter((m) => m.name.toLowerCase().startsWith(query) || (m.display_name || "").toLowerCase().startsWith(query))
    .slice(0, 6);
  mentionPicker.index = 0;
  if (!mentionPicker.items.length) return hideMentionPicker();
  renderMentionPicker();
}

function renderMentionPicker() {
  $("autocomplete").replaceChildren(...mentionPicker.items.map((m, i) => el("li", {
    class: i === mentionPicker.index ? "selected" : "",
    onmousedown: (ev) => {
      ev.preventDefault();
      chooseMention(m);
    },
  }, m.display_name || m.name, el("span", { class: "handle", textContent: `@${m.name}` }))));
  $("autocomplete").hidden = false;
}

function hideMentionPicker() {
  $("autocomplete").hidden = true;
  mentionPicker.items = [];
}

function chooseMention(m) {
  const input = $("message-input");
  const caret = input.selectionStart;
  const inserted = `@${m.name} `;
  input.value = input.value.slice(0, mentionPicker.start) + inserted + input.value.slice(caret);
  const pos = mentionPicker.start + inserted.length;
  input.setSelectionRange(pos, pos);
  hideMentionPicker();
  input.focus();
}

$("message-input").addEventListener("input", updateMentionPicker);
$("message-input").addEventListener("blur", () => setTimeout(hideMentionPicker, 100));
$("message-input").addEventListener("keydown", (ev) => {
  if (mentionPicker.items.length) {
    if (ev.key === "ArrowDown" || ev.key === "ArrowUp") {
      ev.preventDefault();
      const n = mentionPicker.items.length;
      mentionPicker.index = (mentionPicker.index + (ev.key === "ArrowDown" ? 1 : n - 1)) % n;
      renderMentionPicker();
      return;
    }
    if (ev.key === "Enter" || ev.key === "Tab") {
      ev.preventDefault();
      chooseMention(mentionPicker.items[mentionPicker.index]);
      return;
    }
    if (ev.key === "Escape") {
      hideMentionPicker();
      return;
    }
  }
  if (ev.key === "Enter" && !ev.shiftKey) {
    ev.preventDefault();
    $("composer").requestSubmit();
  }
});

$("profile").addEventListener("click", async () => {
  const user = state.account.user;
  const preview = el("span", {}, avatar(user, "large"));
  const removeButton = el("button", { type: "button", class: "small", textContent: "Remove", hidden: !user.avatar_id });
  const setAvatar = async (image) => {
    const updated = await api("PUT", "/users/me/avatar", { image });
    onUserUpdated(updated);
    preview.replaceChildren(avatar(updated, "large"));
    removeButton.hidden = !updated.avatar_id;
  };
  removeButton.onclick = () => run(() => setAvatar(null));
  let wantsPassword = false;
  let wantsEncryption = false;
  const done = await ask({
    title: "Your profile",
    text: el("div", {},
      el("div", { class: "row profile-avatar" },
        preview,
        el("button", { type: "button", class: "small", textContent: "Choose picture…", onclick: () => run(() => chooseAndUpload(setAvatar)) }),
        removeButton),
      el("p", { class: "muted", textContent: `User name: @${user.name} (cannot be changed)` }),
      el("p", { class: "muted" },
        state.keyring?.approved ? "Encryption: this device is approved. " : "Encryption: this device is not approved yet. ",
        el("button", {
          type: "button", class: "small", textContent: state.keyring?.approved ? "Approve another device…" : "Approve…",
          onclick: () => {
            wantsEncryption = true;
            $("dialog-cancel").click();
          },
        })),
      el("button", {
        type: "button", class: "small", textContent: "Change password…",
        onclick: () => {
          wantsPassword = true;
          $("dialog-cancel").click();
        },
      })),
    fields: [{ id: "display_name", label: "Display name", value: user.display_name }],
    ok: "Save",
    submit: (v) => {
      if (!v.display_name) return "Enter a display name.";
      if (v.display_name === state.account.user.display_name) return null;
      return tryText(async () => onUserUpdated(await api("PATCH", "/users/me", { display_name: v.display_name })));
    },
  });
  if (done) setStatus("Profile saved");
  if (wantsPassword) changePassword();
  if (wantsEncryption) {
    if (state.keyring?.approved) approvePendingDevices({ explicit: true });
    else approveThisDevice();
  }
});

async function changePassword() {
  const done = await ask({
    title: "Change password",
    fields: [
      { id: "current", label: "Current password", type: "password" },
      { id: "next", label: "New password (at least 8 characters)", type: "password" },
      { id: "repeat", label: "Repeat new password", type: "password" },
    ],
    ok: "Change",
    submit: (v) => {
      if (v.next.length < 8) return "The new password needs at least 8 characters.";
      if (v.next !== v.repeat) return "The new passwords do not match.";
      return tryText(() => api("PUT", "/users/me/password", { current_password: v.current, new_password: v.next }));
    },
  });
  if (done) setStatus("Password changed; your other sessions were signed out");
}

$("composer").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  const ch = state.channel;
  const content = $("message-input").value.trim();
  if (!ch || (!content && !state.pendingFiles.length)) return;
  $("composer").querySelector("button[type=submit]").disabled = true;
  try {
    const ids = [];
    for (const file of state.pendingFiles) {
      setStatus(`Uploading ${file.name}…`);
      const data = new Uint8Array(await file.arrayBuffer());
      const info = await invoke("upload", data, {
        headers: {
          "x-channel-id": ch.id,
          "x-filename": encodeURIComponent(file.name),
          "x-content-type": file.type || "application/octet-stream",
        },
      });
      ids.push(info.id);
    }
    const message = await api("POST", `/channels/${ch.id}/messages`, { content, attachment_ids: ids });
    state.pendingFiles = [];
    renderPending();
    $("message-input").value = "";
    setStatus("");
    addMessage(message);
    $("messages").scrollTop = $("messages").scrollHeight;
  } catch (e) {
    setStatus(String(e), true);
  } finally {
    $("composer").querySelector("button[type=submit]").disabled = false;
  }
});

// --------------------------------------------------------- group settings
// Admin UI for the group, roles, members, channels and invites. It mirrors
// the server's rules (role hierarchy, granting only what you have); the
// server enforces them anyway and its error text shows in the status line.

const PERMISSIONS = [
  ["VIEW_CHANNEL", "View channels"],
  ["SEND_MESSAGES", "Send messages"],
  ["ATTACH_FILES", "Attach files"],
  ["CONNECT", "Join voice"],
  ["SPEAK", "Speak"],
  ["SHARE_SCREEN", "Share screen and camera"],
  ["CREATE_OWN_CHANNELS", "Create own rooms"],
  ["MANAGE_CHANNELS", "Manage channels"],
  ["MANAGE_MESSAGES", "Delete others' messages"],
  ["MUTE_MEMBERS", "Mute members in voice"],
  ["MOVE_MEMBERS", "Move and disconnect in voice"],
  ["KICK_MEMBERS", "Kick members"],
  ["MANAGE_ROLES", "Manage roles"],
  ["MANAGE_GROUP", "Manage group and invites"],
  ["ADMINISTRATOR", "Administrator (all permissions)"],
];
/** Permissions that can be allowed or denied per channel. */
const CHANNEL_PERMISSIONS = PERMISSIONS.slice(0, 11).filter(([flag]) => flag !== "CREATE_OWN_CHANNELS");

const flagSet = (perms) => new Set((perms || "").split("|").map((p) => p.trim()).filter(Boolean));
const joinFlags = (set) => [...set].join(" | ");

const admin = { tab: "overview", roles: [], roleId: null, overwriteChannel: null, overwriteTarget: null };

function canAdmin(flag) {
  return can(state.group?.permissions, flag);
}

function isOwner() {
  return state.group?.owner_id === myId();
}

/** Whether I may hand out `flag` (in a role or a channel overwrite). */
function canGrant(flag, perms = state.group?.permissions) {
  return isOwner() || can(perms, flag);
}

/** Roles strictly below my highest role can be managed and assigned. */
const myPosition = () => state.group?.my_position ?? 0;
const belowMe = (role) => role.position < myPosition();

const colorToHex = (c) => `#${((c ?? 0x99aab5) >>> 0).toString(16).padStart(6, "0").slice(-6)}`;
const hexToColor = (hex) => parseInt(hex.slice(1), 16);

/** Shows the settings button when the user may change anything in the group. */
function updateSettingsButton() {
  const any = ["MANAGE_GROUP", "MANAGE_ROLES", "MANAGE_CHANNELS", "KICK_MEMBERS"].some(canAdmin);
  $("group-settings").hidden = state.view !== "group" || !state.group || !any;
  if ($("group-settings").hidden) closeSettings();
}

function settingsOpen() {
  return !$("settings-view").hidden;
}

function openSettings(tab) {
  admin.tab = tab || admin.tab;
  $("chat").hidden = true;
  $("settings-view").hidden = false;
  renderSettings();
}

function closeSettings() {
  $("settings-view").hidden = true;
  $("chat").hidden = false;
}

async function renderSettings() {
  if (!state.group) return closeSettings();
  $("settings-title").textContent = `${state.group.name} – settings`;
  const tabs = [
    ["overview", "Overview", canAdmin("MANAGE_GROUP") || isOwner()],
    ["roles", "Roles", canAdmin("MANAGE_ROLES")],
    ["members", "Members", canAdmin("MANAGE_ROLES") || canAdmin("KICK_MEMBERS")],
    ["channels", "Channels", canAdmin("MANAGE_CHANNELS")],
    ["invites", "Invites", canAdmin("MANAGE_GROUP")],
  ].filter(([, , allowed]) => allowed);
  if (!tabs.length) return closeSettings();
  if (!tabs.some(([id]) => id === admin.tab)) admin.tab = tabs[0][0];
  $("settings-tabs").replaceChildren(...tabs.map(([id, label]) => el("button", {
    type: "button", class: `small${id === admin.tab ? " active" : ""}`, textContent: label,
    onclick: () => { admin.tab = id; renderSettings(); },
  })));
  const builders = {
    overview: async () => settingsOverview(),
    roles: settingsRoles,
    members: settingsMembers,
    channels: async () => settingsChannels(),
    invites: settingsInvites,
  };
  const tab = admin.tab;
  const nodes = await builders[tab]();
  if (admin.tab === tab) $("settings-body").replaceChildren(...nodes);
}

/** Reloads group data after a change; the open tab redraws with it. */
async function settingsChanged(message) {
  if (message) setStatus(message);
  const id = state.group.id;
  const [group, members, channels] = await Promise.all([
    run(() => api("GET", `/groups/${id}`)),
    run(() => api("GET", `/groups/${id}/members`)),
    run(() => api("GET", `/groups/${id}/channels`)),
  ]);
  if (!group || state.group?.id !== id) return;
  state.group = group;
  if (state.view === "group") $("group-name").textContent = group.name;
  setMembers(members || []);
  setChannels(channels);
  loadGroups();
  updateSettingsButton();
  if (settingsOpen()) renderSettings();
}

function section(title, ...children) {
  return el("section", { class: "settings-section" }, el("h3", { textContent: title }), ...children);
}

const saveButton = (label, onclick) => el("button", { type: "button", class: "primary small", textContent: label, onclick });

// Overview ------------------------------------------------------------------

function settingsOverview() {
  const g = state.group;
  const parts = [];
  if (canAdmin("MANAGE_GROUP")) {
    const name = el("input", { value: g.name, maxLength: 64 });
    const limit = el("input", { type: "number", min: 0, max: 50, value: g.user_channel_limit ?? 0 });
    parts.push(section("Group",
      // Not a <label>: clicking the caption would open the file picker.
      el("div", { class: "icon-field" }, "Icon", iconPicker(g.icon_id, initials(g.name), `/groups/${g.id}/icon`, "Group icon")),
      el("label", {}, "Name", name),
      el("label", {}, "Own rooms per member (0–50)", limit),
      saveButton("Save", async () => {
        const body = { name: name.value.trim(), user_channel_limit: Number(limit.value) };
        if (await run(() => api("PATCH", `/groups/${g.id}`, body))) settingsChanged("Group saved");
      })));
  }
  if (isOwner()) {
    parts.push(section("Delete group",
      el("p", { class: "muted", textContent: "Deletes all channels, messages and files of this group. This cannot be undone." }),
      el("button", {
        type: "button", class: "danger", textContent: "Delete group…",
        onclick: async () => {
          const done = await ask({
            title: "Delete group?",
            text: `Type the group name "${g.name}" to confirm.`,
            fields: [{ id: "name", label: "Group name" }],
            ok: "Delete",
            submit: (v) => v.name !== g.name ? "The name does not match."
              : tryText(() => api("DELETE", `/groups/${g.id}`)),
          });
          if (!done) return;
          closeSettings();
          await loadGroups();
          if (state.groups[0]) selectGroup(state.groups[0].id);
          else renderEmptyGroup();
          setStatus("Group deleted");
        },
      })));
  }
  return parts;
}

// Roles -----------------------------------------------------------------------

async function loadRoles() {
  admin.roles = ((await run(() => api("GET", `/groups/${state.group.id}/roles`))) || [])
    .sort((a, b) => b.position - a.position);
  if (!admin.roles.some((r) => r.id === admin.roleId)) admin.roleId = admin.roles[0]?.id || null;
}

function roleDot(role) {
  return el("span", { class: "role-color", style: `background: ${colorToHex(role.color)}` });
}

async function settingsRoles() {
  await loadRoles();
  const list = el("div", { class: "role-list" },
    ...admin.roles.map((r) => el("button", {
      type: "button", class: `role-item${r.id === admin.roleId ? " active" : ""}`,
      onclick: () => { admin.roleId = r.id; renderSettings(); },
    }, roleDot(r), ` ${r.name}`, belowMe(r) ? null : el("span", { class: "muted", textContent: " (locked)" }))),
    el("button", {
      type: "button", class: "small", textContent: "+ New role",
      onclick: async () => {
        const v = await ask({ title: "New role", fields: [{ id: "name", label: "Name" }], submit: (x) => (x.name ? null : "Enter a name.") });
        if (!v) return;
        const role = await run(() => api("POST", `/groups/${state.group.id}/roles`, { name: v.name, permissions: "" }));
        if (role) {
          admin.roleId = role.id;
          settingsChanged(`Role ${role.name} created`);
        }
      },
    }));
  const role = admin.roles.find((r) => r.id === admin.roleId);
  return [el("div", { class: "settings-split" }, list,
    role ? roleEditor(role) : el("p", { class: "muted", textContent: "No roles." }))];
}

function roleEditor(role) {
  const locked = !belowMe(role);
  const name = el("input", { value: role.name, maxLength: 64, disabled: locked || role.everyone });
  const color = el("input", { type: "color", value: colorToHex(role.color), disabled: locked || role.everyone });
  const position = el("input", { type: "number", value: role.position, min: 1, disabled: locked || role.everyone });
  const granted = flagSet(role.permissions);
  const boxes = PERMISSIONS.map(([flag, label]) => {
    const box = el("input", { type: "checkbox", checked: granted.has(flag), disabled: locked || !canGrant(flag) });
    box.dataset.flag = flag;
    return el("label", { class: "inline perm", title: canGrant(flag) ? "" : "You do not have this permission yourself" }, box, ` ${label}`);
  });
  return el("div", { class: "role-editor" },
    locked ? el("p", { class: "muted", textContent: "This role is at or above your highest role; you cannot change it." }) : null,
    role.everyone ? el("p", { class: "muted", textContent: "@everyone applies to all members; only its permissions can change." }) : null,
    el("div", { class: "row" },
      el("label", {}, "Name", name),
      el("label", { class: "narrow" }, "Color", color),
      el("label", { class: "narrow" }, "Position", position)),
    el("div", { class: "perm-grid" }, ...boxes),
    locked ? null : el("div", { class: "row" },
      saveButton("Save", async () => {
        const perms = new Set(boxes.map((b) => b.querySelector("input")).filter((b) => b.checked).map((b) => b.dataset.flag));
        const body = { permissions: joinFlags(perms) };
        if (!role.everyone) {
          body.name = name.value.trim();
          body.color = hexToColor(color.value);
          body.position = Number(position.value);
        }
        if (await run(() => api("PATCH", `/groups/${state.group.id}/roles/${role.id}`, body))) settingsChanged(`Role ${body.name || role.name} saved`);
      }),
      role.everyone ? null : el("button", {
        type: "button", class: "danger small", textContent: "Delete",
        onclick: async () => {
          if (!(await ask({ title: `Delete role ${role.name}?`, text: "Members lose the permissions of this role.", ok: "Delete" }))) return;
          if ((await run(() => api("DELETE", `/groups/${state.group.id}/roles/${role.id}`))) !== undefined) {
            admin.roleId = null;
            settingsChanged("Role deleted");
          }
        },
      })));
}

// Members ---------------------------------------------------------------------

async function settingsMembers() {
  await loadRoles();
  const roles = admin.roles.filter((r) => !r.everyone);
  const position = (m) => Math.max(0, ...roles.filter((r) => (m.role_ids || []).includes(r.id)).map((r) => r.position));
  const members = [...state.members.values()].sort((a, b) => (a.display_name || a.name).localeCompare(b.display_name || b.name));
  const rows = members.map((m) => {
    const owner = m.user_id === state.group.owner_id;
    const chips = roles.map((r) => {
      const assignable = canAdmin("MANAGE_ROLES") && belowMe(r);
      const box = el("input", { type: "checkbox", checked: (m.role_ids || []).includes(r.id), disabled: !assignable });
      box.addEventListener("change", async () => {
        const path = `/groups/${state.group.id}/members/${m.user_id}/roles/${r.id}`;
        const ok = await run(() => api(box.checked ? "PUT" : "DELETE", path));
        if (ok === undefined) box.checked = !box.checked;
        else settingsChanged(`${m.display_name || m.name}: ${box.checked ? "added to" : "removed from"} ${r.name}`);
      });
      return el("label", { class: "inline chip" }, box, " ", roleDot(r), ` ${r.name}`);
    });
    const kickable = canAdmin("KICK_MEMBERS") && !owner && m.user_id !== myId() && position(m) < myPosition();
    return el("div", { class: "member-row" },
      el("span", { class: `dot${m.online ? " online" : ""}` }),
      el("span", { class: "member-name", textContent: m.display_name || m.name, title: `@${m.name}` }),
      owner ? el("span", { class: "badge", textContent: "owner" }) : null,
      el("span", { class: "chips" }, ...chips),
      kickable ? el("button", {
        type: "button", class: "small danger", textContent: "Kick",
        onclick: async () => {
          if (!(await ask({ title: `Remove ${m.display_name || m.name} from the group?`, text: "They can come back with a new invite.", ok: "Kick" }))) return;
          if ((await run(() => api("DELETE", `/groups/${state.group.id}/members/${m.user_id}`))) !== undefined) settingsChanged("Member removed");
        },
      }) : null);
  });
  return [section("Members",
    roles.length ? null : el("p", { class: "muted", textContent: "Create roles in the Roles tab to give members extra permissions." }),
    ...rows)];
}

// Channels --------------------------------------------------------------------

function categoryOptions(selected) {
  return [el("option", { value: "", textContent: "No category", selected: !selected }),
    ...state.categories.map((c) => el("option", { value: c.id, textContent: c.name, selected: c.id === selected }))];
}

function settingsChannels() {
  const g = state.group;
  const categoryRows = state.categories.map((c) => {
    const name = el("input", { value: c.name, maxLength: 64 });
    const position = el("input", { type: "number", class: "pos", value: c.position });
    const rooms = el("input", { type: "checkbox", checked: c.allow_user_channels });
    return el("div", { class: "channel-row" },
      name, el("label", { class: "inline" }, "Pos ", position),
      el("label", { class: "inline" }, rooms, " Members may create rooms here"),
      saveButton("Save", async () => {
        const body = { name: name.value.trim(), position: Number(position.value), allow_user_channels: rooms.checked };
        if (await run(() => api("PATCH", `/categories/${c.id}`, body))) settingsChanged(`Category ${body.name} saved`);
      }),
      el("button", {
        type: "button", class: "small danger", textContent: "Delete",
        onclick: async () => {
          if (!(await ask({ title: `Delete category ${c.name}?`, text: "Its channels stay and move out of the category.", ok: "Delete" }))) return;
          if ((await run(() => api("DELETE", `/categories/${c.id}`))) !== undefined) settingsChanged(`Category ${c.name} deleted`);
        },
      }));
  });
  const channelRows = state.channels.map((ch) => {
    const name = el("input", { value: ch.name, maxLength: 64 });
    const position = el("input", { type: "number", class: "pos", value: ch.position });
    const voice = el("input", { type: "checkbox", checked: ch.voice_enabled });
    const category = el("select", {}, ...categoryOptions(ch.category_id));
    const emoji = el("input", {
      value: ch.emoji || "", maxLength: 8, class: "emoji", placeholder: "#",
      title: "Emoji shown before the name (used when there is no icon image)",
    });
    const row = el("div", { class: "channel-row" },
      iconPicker(ch.icon_id, ch.emoji || "#", `/channels/${ch.id}/icon`, "Channel icon"),
      emoji, name, el("label", { class: "inline" }, "Pos ", position), category,
      el("label", { class: "inline" }, voice, " Voice"),
      saveButton("Save", async () => {
        const body = {
          name: name.value.trim(), position: Number(position.value), voice_enabled: voice.checked,
          category_id: category.value || null, emoji: emoji.value.trim() || null,
        };
        if (await run(() => api("PATCH", `/channels/${ch.id}`, body))) settingsChanged(`#${body.name} saved`);
      }),
      el("button", {
        type: "button", class: `small${admin.overwriteChannel === ch.id ? " active" : ""}`, textContent: "Permissions",
        onclick: () => {
          admin.overwriteChannel = admin.overwriteChannel === ch.id ? null : ch.id;
          admin.overwriteTarget = null;
          renderSettings();
        },
      }),
      el("button", {
        type: "button", class: "small danger", textContent: "Delete",
        onclick: async () => {
          if (!(await ask({ title: `Delete #${ch.name}?`, text: "All messages and files in it are deleted.", ok: "Delete" }))) return;
          if ((await run(() => api("DELETE", `/channels/${ch.id}`))) !== undefined) settingsChanged(`#${ch.name} deleted`);
        },
      }));
    if (admin.overwriteChannel !== ch.id) return row;
    const editor = el("div", { class: "overwrite-editor" }, el("p", { class: "muted", textContent: "Loading…" }));
    overwriteEditor(ch, editor);
    return el("div", {}, row, editor);
  });
  const newName = el("input", { placeholder: "Channel name", maxLength: 64 });
  const newCategory = el("select", {}, ...categoryOptions(null));
  const newVoice = el("input", { type: "checkbox", checked: true });
  const catName = el("input", { placeholder: "Category name", maxLength: 64 });
  const catRooms = el("input", { type: "checkbox" });
  return [
    section("Categories", ...categoryRows, el("div", { class: "row new-row" },
      catName, el("label", { class: "inline" }, catRooms, " Members may create rooms here"),
      saveButton("Create category", async () => {
        const body = { name: catName.value.trim(), allow_user_channels: catRooms.checked, position: state.categories.length + 1 };
        if (!body.name) return setStatus("Enter a category name", true);
        if (await run(() => api("POST", `/groups/${g.id}/categories`, body))) settingsChanged(`Category ${body.name} created`);
      }))),
    section("Channels", ...channelRows, el("div", { class: "row new-row" },
      newName, newCategory, el("label", { class: "inline" }, newVoice, " Voice"),
      saveButton("Create channel", async () => {
        const body = { name: newName.value.trim(), category_id: newCategory.value || null, voice_enabled: newVoice.checked };
        if (!body.name) return setStatus("Enter a channel name", true);
        if (await run(() => api("POST", `/groups/${g.id}/channels`, body))) settingsChanged(`#${body.name} created`);
      }))),
  ];
}

/** Allow/deny per permission for roles and members in one channel. */
async function overwriteEditor(ch, container) {
  const list = (await run(() => api("GET", `/channels/${ch.id}/overwrites`))) || [];
  await loadRoles();
  const targets = [
    // @everyone first: the most common place to restrict a channel.
    ...[...admin.roles].sort((a, b) => b.everyone - a.everyone)
      .map((r) => ({ kind: "role", id: r.id, label: `Role: ${r.name}` })),
    ...[...state.members.values()].map((m) => ({ kind: "member", id: m.user_id, label: `Member: ${m.display_name || m.name}` })),
  ];
  const key = (t) => `${t.kind}:${t.id}`;
  const entryFor = (t) => list.find((o) => o.target.kind === t.kind && o.target.id === t.id);
  const entries = targets.filter((t) => entryFor(t) || key(t) === admin.overwriteTarget);
  if (!entries.some((t) => key(t) === admin.overwriteTarget)) admin.overwriteTarget = entries[0] ? key(entries[0]) : null;
  const current = targets.find((t) => key(t) === admin.overwriteTarget);
  const myChannelPerms = ch.permissions;

  const entryList = el("div", { class: "overwrite-entries" },
    ...entries.map((t) => el("button", {
      type: "button", class: `small${key(t) === admin.overwriteTarget ? " active" : ""}`,
      textContent: t.label + (entryFor(t) ? "" : " (new)"),
      onclick: () => { admin.overwriteTarget = key(t); overwriteEditor(ch, container); },
    })));
  const addable = targets.filter((t) => !entries.includes(t));
  const addSelect = el("select", {}, ...addable.map((t) => el("option", { value: key(t), textContent: t.label })));
  const add = el("div", { class: "row" }, addSelect, el("button", {
    type: "button", class: "small", textContent: "Add entry", disabled: !addable.length,
    onclick: () => { admin.overwriteTarget = addSelect.value; overwriteEditor(ch, container); },
  }));

  if (!current) {
    container.replaceChildren(
      el("p", { class: "muted", textContent: "No channel-specific permissions yet. Add an entry for a role or member." }),
      add);
    return;
  }
  const o = entryFor(current);
  const allow = flagSet(o?.allow);
  const deny = flagSet(o?.deny);
  const grid = el("div", { class: "overwrite-grid" }, ...CHANNEL_PERMISSIONS.map(([flag, label]) => {
    const value = allow.has(flag) ? "allow" : deny.has(flag) ? "deny" : "inherit";
    const allowed = canGrant(flag, myChannelPerms);
    return el("div", { class: "overwrite-row", dataset: { flag }, title: allowed ? "" : "You do not have this permission in this channel" },
      el("span", { class: allowed ? "" : "muted", textContent: label }),
      ...["inherit", "allow", "deny"].map((v) => el("label", { class: `inline ow-${v}` },
        el("input", { type: "radio", name: `ow-${flag}`, value: v, checked: v === value, disabled: !allowed }), ` ${v}`)));
  }));
  const path = `/channels/${ch.id}/overwrites/${current.kind}/${current.id}`;
  container.replaceChildren(
    el("p", { class: "muted", textContent: "Allow or deny permissions in this channel; inherit uses the roles." }),
    entryList, grid,
    el("div", { class: "row" },
      saveButton("Save", async () => {
        const a = new Set();
        const d = new Set();
        for (const row of grid.querySelectorAll(".overwrite-row")) {
          const v = row.querySelector("input:checked").value;
          if (v === "allow") a.add(row.dataset.flag);
          if (v === "deny") d.add(row.dataset.flag);
        }
        const ok = a.size || d.size
          ? await run(() => api("PUT", path, { allow: joinFlags(a), deny: joinFlags(d) }))
          : await run(() => api("DELETE", path));
        if (ok !== undefined) settingsChanged(`Permissions of #${ch.name} saved`);
      }),
      o ? el("button", {
        type: "button", class: "small danger", textContent: "Remove entry",
        onclick: async () => {
          if ((await run(() => api("DELETE", path))) !== undefined) {
            admin.overwriteTarget = null;
            settingsChanged(`Entry removed from #${ch.name}`);
          }
        },
      }) : null),
    add);
}

// Invites ---------------------------------------------------------------------

async function settingsInvites() {
  const g = state.group;
  const invites = (await run(() => api("GET", `/groups/${g.id}/invites`))) || [];
  const rows = invites.map((inv) => el("div", { class: "member-row" },
    el("span", { class: "code invite-code", textContent: inv.code }),
    el("span", { class: "muted", textContent: `${inv.uses}${inv.max_uses ? ` / ${inv.max_uses}` : ""} uses` }),
    el("span", { class: "muted", textContent: inv.expires_at ? `expires ${new Date(inv.expires_at).toLocaleString()}` : "never expires" }),
    el("span", { class: "chips" }),
    el("button", {
      type: "button", class: "small", textContent: "Copy",
      onclick: async () => {
        try {
          await navigator.clipboard.writeText(inv.code);
          setStatus("Invite code copied");
        } catch {
          setStatus(`Invite code: ${inv.code}`);
        }
      },
    }),
    el("button", {
      type: "button", class: "small danger", textContent: "Revoke",
      onclick: async () => {
        if ((await run(() => api("DELETE", `/groups/${g.id}/invites/${encodeURIComponent(inv.code)}`))) !== undefined) {
          setStatus("Invite revoked");
          renderSettings();
        }
      },
    })));
  const maxUses = el("input", { type: "number", min: 1, placeholder: "unlimited", class: "pos" });
  const hours = el("input", { type: "number", min: 1, placeholder: "never", class: "pos" });
  return [
    section("Invites", ...(rows.length ? rows : [el("p", { class: "muted", textContent: "No active invites." })])),
    section("New invite", el("div", { class: "row new-row" },
      el("label", { class: "inline" }, "Max uses ", maxUses),
      el("label", { class: "inline" }, "Expires after (hours) ", hours),
      saveButton("Create invite", async () => {
        const body = {
          max_uses: maxUses.value ? Number(maxUses.value) : null,
          expires_in_hours: hours.value ? Number(hours.value) : null,
        };
        const inv = await run(() => api("POST", `/groups/${g.id}/invites`, body));
        if (inv) {
          setStatus(`Invite ${inv.code} created`);
          renderSettings();
        }
      }))),
  ];
}

$("group-settings").addEventListener("click", () => openSettings());
$("settings-close").addEventListener("click", closeSettings);

// ---------------------------------------------------------------- gateway

listen("gateway", (ev) => {
  const e = ev.payload;
  const inGroup = state.group && e.group_id === state.group.id;
  switch (e.type) {
    case "message_created":
      addMessage(e.data);
      trackNewMessage(e);
      break;
    case "message_updated":
      replaceMessage(e.data);
      break;
    case "presence_changed": {
      const member = inGroup ? state.members.get(e.user_id) : null;
      if (member) member.online = e.data.online;
      for (const dm of state.dms) if (dm.user.id === e.user_id) dm.user.online = e.data.online;
      renderTree();
      break;
    }
    case "user_updated":
      onUserUpdated(e.data);
      break;
    case "typing":
      if (e.channel_id === state.channel?.id && e.user_id !== myId()) {
        typingUsers.set(e.user_id, { name: displayNameOf(e.user_id, e.data.name), until: Date.now() + 5000 });
        renderTyping();
      }
      break;
    case "message_deleted":
      removeMessage(e.data.id);
      break;
    case "voice_joined":
      if (inGroup && !state.voice.some((v) => v.channel_id === e.channel_id && v.user_id === e.data.user_id)) {
        state.voice.push({ channel_id: e.channel_id, user_id: e.data.user_id });
        renderTree();
      }
      break;
    case "voice_left":
      if (inGroup) {
        state.voice = state.voice.filter((v) => !(v.channel_id === e.channel_id && v.user_id === e.data.user_id));
        renderTree();
      }
      break;
    case "voice_moved":
      // A moderator moved us: the server disconnected us, join the target.
      if (e.user_id === state.account?.user.id) {
        const target = state.channels.find((c) => c.id === e.data.to);
        if (target) joinVoice(target);
      }
      break;
    case "group_deleted":
    case "member_left":
      loadGroups().then(() => {
        if (state.group && !state.groups.some((g) => g.id === state.group.id)) {
          if (state.groups[0]) selectGroup(state.groups[0].id);
          else renderEmptyGroup();
        } else if (inGroup) refreshGroup();
      });
      break;
    case "keys_changed":
      onKeysChanged(e.user_id);
      break;
    case "channel_keys_added":
      onChannelKeysAdded(e.channel_id);
      break;
    case "resync":
      loadGroups();
      loadDms();
      refreshGroup();
      if (state.channel) loadMessages();
      break;
    default:
      // channel_created/updated/deleted, member_joined, group_updated
      if (inGroup) refreshGroup();
      if (e.type === "group_updated") loadGroups();
      if (e.type === "group_updated" || e.type === "member_joined") shareKeysSoon();
  }
});

listen("gateway-status", (ev) => {
  $("gateway-dot").classList.toggle("online", ev.payload.connected);
  $("gateway-dot").title = ev.payload.connected ? "Live updates connected" : "Reconnecting…";
});

// ------------------------------------------------------------------ voice

// Created on a click (join), so the WebView's autoplay policy allows it.
let audioContext = null;
function unlockAudio() {
  try {
    audioContext ??= new AudioContext();
    audioContext.resume();
  } catch {
    // no beeps then
  }
}

/** Two short, quiet tones: "you are talking, but nobody hears you". */
function playMutedBeep() {
  if (!audioContext) return;
  const t = audioContext.currentTime;
  for (const [offset, freq] of [[0, 880], [0.18, 660]]) {
    const osc = audioContext.createOscillator();
    const gain = audioContext.createGain();
    osc.frequency.value = freq;
    gain.gain.setValueAtTime(0, t + offset);
    gain.gain.linearRampToValueAtTime(0.08, t + offset + 0.02);
    gain.gain.linearRampToValueAtTime(0, t + offset + 0.14);
    osc.connect(gain).connect(audioContext.destination);
    osc.start(t + offset);
    osc.stop(t + offset + 0.15);
  }
}

listen("muted-speech", () => {
  playMutedBeep();
  $("muted-hint").hidden = false;
  clearTimeout(playMutedBeep.timer);
  playMutedBeep.timer = setTimeout(() => { $("muted-hint").hidden = true; }, 6000);
});

/** Whether the running call uses channel keys (and needs newer ones). */
let voiceEncrypted = false;

/** Media keys of a channel's epochs for the Rust core, newest last; null when off. */
async function channelMediaKeys(ch) {
  if (settings.load().e2eeMode !== "keys") return null;
  if (!state.keyring?.approved) throw new Error("Approve this device for encryption first (your profile).");
  await state.keyring.current(ch.id);
  return mediaKeysOf(ch.id);
}

async function mediaKeysOf(channelId) {
  const out = [];
  for (const [epoch, key] of state.keyring.known(channelId).slice(-16)) {
    out.push({ index: e2ee.mediaKeyIndex(epoch), key: e2ee.toBase64(await e2ee.mediaKey(key, channelId)) });
  }
  return out;
}

/** Joins a channel's voice; returns whether it worked. */
async function joinVoice(ch, { quiet = false } = {}) {
  unlockAudio();
  if (!quiet) setStatus(`Joining ${ch.name}…`);
  let info;
  try {
    const mediaKeys = await channelMediaKeys(ch);
    info = await invoke("join_voice", { channelId: ch.id, mediaKeys });
    voiceEncrypted = !!mediaKeys;
  } catch (e) {
    if (!quiet) setStatus(String(e), true);
    return false;
  }
  cancelRejoin();
  setStatus(`Connected to voice in ${ch.name}`);
  voiceJoined(ch);
  // Rejoined automatically after a connection loss or app restart.
  settings.save({ lastVoice: { groupId: ch.group_id, channelId: ch.id } });
  return !!info;
}

/** After start-up: back into the voice channel the app was closed in. */
async function rejoinLastVoice() {
  const last = settings.load().lastVoice;
  if (!last || state.voiceChannel) return;
  let channels = state.group?.id === last.groupId ? state.channels : null;
  if (!channels) {
    const list = await api("GET", `/groups/${last.groupId}/channels`).catch(() => null);
    channels = list?.channels || [];
  }
  const ch = channels.find((c) => c.id === last.channelId);
  if (ch && can(ch.permissions, "CONNECT")) {
    setStatus(`Rejoining ${ch.name}…`);
    // The server may still be unreachable (e.g. after a restart to recover).
    if (!(await joinVoice(ch, { quiet: true }))) scheduleRejoin(ch, 0);
  } else {
    settings.save({ lastVoice: null });
  }
}

// The beep needs an audio context, which the WebView only allows after a click.
document.addEventListener("pointerdown", () => unlockAudio(), { once: true });

function voiceJoined(ch) {
  state.voiceChannel = ch;
  $("voice-status").textContent = `Voice: ${ch.dev ? ch.name : `# ${ch.name}`}`;
  $("voice-status").classList.remove("muted");
  $("voice-leave").hidden = false;
  $("share-start").hidden = !(ch.dev || can(ch.permissions, "SHARE_SCREEN"));
  $("share-stop").hidden = true;
  $("share-info").hidden = true;
  $("cam-start").hidden = !(ch.dev || can(ch.permissions, "SHARE_SCREEN"));
  $("cam-stop").hidden = true;
  $("tx").hidden = false;
}

function voiceLeft() {
  state.voiceChannel = null;
  state.snapshot = null;
  $("voice-status").textContent = "Not in voice";
  $("voice-status").classList.add("muted");
  for (const id of ["voice-leave", "share-start", "share-stop", "cam-start", "cam-stop", "cams-show", "share-info", "tx", "muted-hint"]) $(id).hidden = true;
  $("participants").replaceChildren();
  participantRows.clear();
  $("meter-fill").style.width = "0";
  updateSpeaking();
}

$("voice-leave").addEventListener("click", async () => {
  cancelRejoin();
  settings.save({ lastVoice: null });
  await run(() => invoke("leave"));
  voiceLeft();
});

function shareSettings() {
  const height = $("share-height").value;
  return {
    maxHeight: height === "native" ? null : Number(height),
    fps: Number($("share-fps").value),
    capture: $("share-capture").value,
  };
}

async function startShare() {
  $("share-start").disabled = true;
  const id = $("screens").value;
  const info = await run(() => invoke("start_share", {
    sourceId: id ? Number(id) : null,
    settings: shareSettings(),
  }));
  $("share-start").disabled = false;
  if (!info) return false;
  $("share-start").hidden = true;
  $("share-stop").hidden = false;
  const encoder = info.encoder
    ? `${info.encoder} (${info.capture === "gpu" ? "GPU" : "CPU"} capture)`
    : "software H.264 (no hardware encoder found)";
  const mbit = (info.bitrateBps / 1e6).toFixed(1);
  $("share-info").textContent = `Sharing ${info.width}×${info.height} @ ${info.fps} fps · up to ${mbit} Mbit/s · ${encoder}`;
  $("share-info").hidden = false;
  return true;
}

$("share-start").addEventListener("click", startShare);

// Changing the quality while sharing restarts the share with the new values.
for (const id of ["share-height", "share-fps", "share-capture"]) {
  $(id).addEventListener("change", async () => {
    settings.save({
      shareHeight: $("share-height").value,
      shareFps: $("share-fps").value,
      shareCapture: $("share-capture").value,
    });
    if ($("share-stop").hidden) return;
    await run(() => invoke("stop_share"));
    if (!(await startShare())) {
      $("share-start").hidden = false;
      $("share-stop").hidden = true;
      $("share-info").hidden = true;
    }
  });
}

$("share-stop").addEventListener("click", async () => {
  await run(() => invoke("stop_share"));
  $("share-start").hidden = false;
  $("share-stop").hidden = true;
  $("share-info").hidden = true;
  setStatus("Stopped sharing");
});

// ---------------------------------------------------------------- cameras

function cameraSettings() {
  return { height: Number($("camera-quality").value) };
}

async function startCamera() {
  $("cam-start").disabled = true;
  const ok = await run(() => invoke("start_camera", {
    device: $("camera-device").value || null,
    settings: cameraSettings(),
  }).then(() => true));
  $("cam-start").disabled = false;
  if (!ok) return false;
  $("cam-start").hidden = true;
  $("cam-stop").hidden = false;
  return true;
}

$("cam-start").addEventListener("click", startCamera);

$("cam-stop").addEventListener("click", async () => {
  await run(() => invoke("stop_camera"));
  $("cam-stop").hidden = true;
  $("cam-start").hidden = !state.voiceChannel;
});

$("cams-show").addEventListener("click", () => run(() => invoke("show_cameras")));

// Another camera or quality while the camera is on restarts it.
for (const id of ["camera-device", "camera-quality"]) {
  $(id).addEventListener("change", async () => {
    settings.save({ cameraDevice: $("camera-device").value, cameraQuality: $("camera-quality").value });
    if ($("cam-stop").hidden) return;
    if (!(await startCamera())) {
      $("cam-start").hidden = false;
      $("cam-stop").hidden = true;
    }
  });
}

async function loadCameras() {
  const cameras = (await run(() => invoke("list_cameras"))) || [];
  const saved = settings.load();
  $("camera-device").replaceChildren(
    el("option", { value: "", textContent: cameras.length ? "Default" : "No camera found" }),
    ...cameras.map((c) => el("option", { value: c.id, textContent: c.name })),
  );
  if (cameras.some((c) => c.id === saved.cameraDevice)) $("camera-device").value = saved.cameraDevice;
}

function fmt(value, unit, digits = 1) {
  return value == null ? "-" : `${value.toFixed(digits)} ${unit}`;
}

function renderStats(s) {
  if (!s) return "";
  const parts = [`rtt ${fmt(s.rttMs, "ms")}`, `jitter ${fmt(s.jitterMs, "ms")}`, `loss ${fmt(s.lossPct, "%")}`];
  if (s.jitterBufferMs != null) parts.push(`jb ${fmt(s.jitterBufferMs, "ms")}`);
  if (s.videoSize) {
    parts.push(`video ${s.videoSize[0]}x${s.videoSize[1]} @ ${fmt(s.videoFps, "fps", 0)}`);
    parts.push(`decode ${fmt(s.decodeMs, "ms")}`);
  }
  return parts.join(" · ");
}

/** Participant rows by identity, updated in place so their buttons keep working. */
const participantRows = new Map();

function participantRow(p) {
  const row = {
    dot: el("span", { class: "dot" }),
    name: el("span", { title: p.identity }),
    muted: el("span", { class: "badge", textContent: "muted" }),
    screen: el("span", { class: "badge", textContent: "screen" }),
    camera: el("span", { class: "badge", textContent: "cam" }),
    watch: el("button", {
      type: "button", class: "small", textContent: "Watch", title: "Open the shared screen",
      onclick: () => run(() => invoke("watch_screen", { identity: p.identity })),
    }),
    mute: el("button", {
      type: "button", class: "small", title: "Mute or unmute for you only",
      onclick: () => toggleLocalMute(p.identity),
    }),
    stats: el("div", { class: "stats" }),
  };
  row.li = el("li", {},
    el("div", { class: "who" }, row.dot, row.name, row.muted, row.screen, row.camera,
      p.isLocal ? null : el("span", { class: "actions" }, row.watch, row.mute)),
    p.isLocal ? null : listenControls(p.identity),
    row.stats);
  return row;
}

function listenPrefs(identity) {
  const saved = settings.load();
  return { volume: saved.volumes?.[identity] ?? 1, compressor: saved.compressors?.[identity] || "off" };
}

async function saveListenPrefs(identity, prefs) {
  const saved = settings.load();
  settings.save({
    volumes: { ...saved.volumes, [identity]: prefs.volume },
    compressors: { ...saved.compressors, [identity]: prefs.compressor },
  });
  await run(() => invoke("set_listen_prefs", { identity, prefs }));
}

/**
 * The second line of a remote participant: volume (up to 200 %) and
 * compressor, for you only. Changing either plays this person through the
 * app's own output instead of WebRTC's.
 */
function listenControls(identity) {
  const prefs = listenPrefs(identity);
  const pct = el("span", { class: "pct" });
  const showPct = (v) => { pct.textContent = `${Math.round(v * 100)} %`; };
  showPct(prefs.volume);
  const volume = el("input", {
    type: "range", class: "volume", min: "0", max: "2", step: "0.05", value: String(prefs.volume),
    title: "Volume (for you only), up to 200 %. Double-click: 100 %",
  });
  const compressor = el("select", {
    class: "compressor", title: "Compressor: evens out loud and quiet speech (for you only)",
  },
  el("option", { value: "off", textContent: "off" }),
  el("option", { value: "light", textContent: "light" }),
  el("option", { value: "strong", textContent: "strong" }));
  compressor.value = prefs.compressor;
  const apply = () => {
    showPct(Number(volume.value));
    saveListenPrefs(identity, { volume: Number(volume.value), compressor: compressor.value });
  };
  volume.addEventListener("input", apply);
  volume.addEventListener("dblclick", () => {
    volume.value = "1";
    apply();
  });
  compressor.addEventListener("change", apply);
  return el("div", { class: "listen" },
    volume, pct, el("label", { title: compressor.title }, "Comp.", compressor));
}

async function toggleLocalMute(identity) {
  const mutes = new Set(settings.load().localMutes || []);
  const muted = !mutes.has(identity);
  if (muted) mutes.add(identity); else mutes.delete(identity);
  settings.save({ localMutes: [...mutes] });
  await run(() => invoke("set_local_mute", { identity, muted }));
}

listen("room-state", (ev) => {
  const snapshot = ev.payload;
  state.snapshot = snapshot;
  const seen = new Set();
  const list = $("participants");
  snapshot.participants.forEach((p, i) => {
    seen.add(p.identity);
    let row = participantRows.get(p.identity);
    if (!row) {
      row = participantRow(p);
      participantRows.set(p.identity, row);
    }
    row.dot.className = `dot${p.speaking ? " speaking" : ""}`;
    row.name.textContent = p.isLocal ? `${p.name} (you)` : p.name;
    row.muted.hidden = !p.micMuted;
    row.screen.hidden = !p.sharingScreen || !p.isLocal;
    row.watch.hidden = !p.sharingScreen;
    row.camera.hidden = !p.camera;
    row.mute.textContent = p.locallyMuted ? "Unmute" : "Mute";
    row.mute.classList.toggle("on", p.locallyMuted);
    row.stats.textContent = renderStats(p.stats);
    if (list.children[i] !== row.li) list.insertBefore(row.li, list.children[i] || null);
    if (p.isLocal) {
      $("tx").textContent = p.transmitting ? "transmitting" : "not transmitting";
      $("tx").classList.toggle("on", p.transmitting);
    }
  });
  $("cams-show").hidden = !snapshot.participants.some((p) => p.camera);
  for (const [identity, row] of participantRows) {
    if (!seen.has(identity)) {
      row.li.remove();
      participantRows.delete(identity);
    }
  }
  updateSpeaking();
});

/** Lights up speakers in the channel tree (identities are "name@domain"). */
function updateSpeaking() {
  const speaking = new Set((state.snapshot?.participants || [])
    .filter((p) => p.speaking)
    .map((p) => p.identity.split("@")[0]));
  for (const li of document.querySelectorAll(".voice-users li")) {
    li.querySelector(".dot").classList.toggle("speaking", speaking.has(li.dataset.user));
  }
}

listen("disconnected", (ev) => {
  const { reason, rejoin } = ev.payload;
  const ch = state.voiceChannel;
  voiceLeft();
  if (rejoin && ch && !ch.dev) {
    recoverVoice(ch);
  } else {
    // Removed by a moderator, room deleted, ...: do not come back by itself.
    // (A move is followed by voice_moved, which joins the new channel.)
    settings.save({ lastVoice: null });
    setStatus(`Voice disconnected (${reason})`, true);
  }
});

listen("voice-reconnecting", () => {
  if (state.voiceChannel) $("voice-status").textContent = `Voice: # ${state.voiceChannel.name} (reconnecting…)`;
});
listen("voice-reconnected", () => {
  if (state.voiceChannel) $("voice-status").textContent = `Voice: # ${state.voiceChannel.name}`;
});

/**
 * After a lost connection: when the LiveKit SDK gave up reconnecting, its
 * audio engine can be stuck (device calls time out). Only a new process
 * recovers; the remembered channel is joined again after the restart.
 */
async function recoverVoice(ch) {
  // The engine gets stuck shortly after the SDK gives up, not at once.
  await new Promise((r) => setTimeout(r, 4000));
  if (await restartIfAudioStuck()) return;
  scheduleRejoin(ch, 0);
}

/** Restarts the app if WebRTC's audio engine no longer responds. */
async function restartIfAudioStuck() {
  try {
    await invoke("audio_devices");
    return false;
  } catch (e) {
    if (!String(e).includes("not responding")) return false;
  }
  setStatus("Audio engine stuck after the connection loss, restarting Tinlink…", true);
  await new Promise((r) => setTimeout(r, 1500));
  await invoke("restart_app");
  return true;
}

/** Retries joining after the connection was lost, with growing pauses. */
let rejoinTimer = null;
const REJOIN_DELAYS = [2, 5, 10, 20, 30, 30, 30, 30, 30, 30];
function scheduleRejoin(ch, attempt) {
  clearTimeout(rejoinTimer);
  if (attempt >= REJOIN_DELAYS.length) {
    setStatus(`Could not reconnect to ${ch.name}`, true);
    return;
  }
  const delay = REJOIN_DELAYS[attempt];
  setStatus(`Voice connection lost, reconnecting in ${delay} s…`, true);
  rejoinTimer = setTimeout(async () => {
    if (state.voiceChannel) return; // joined something else meanwhile
    const ok = await joinVoice(ch, { quiet: true });
    if (!ok) scheduleRejoin(ch, attempt + 1);
    // A join on a stuck engine connects but carries no audio.
    else await restartIfAudioStuck();
  }, delay * 1000);
}

function cancelRejoin() {
  clearTimeout(rejoinTimer);
  rejoinTimer = null;
}

// --------------------------------------------------------- voice settings

for (const radio of document.querySelectorAll("input[name=mode]")) {
  radio.addEventListener("change", () => {
    invoke("set_voice_mode", { mode: radio.value });
    settings.save({ mode: radio.value });
  });
}

$("ptt-set").addEventListener("click", async () => {
  const key = await run(() => invoke("set_ptt_key", { key: $("ptt-key").value.trim() }));
  if (key) {
    setStatus(`Push-to-talk key: ${key}`);
    settings.save({ pttKey: $("ptt-key").value.trim() });
  }
});

$("ptt-record").addEventListener("click", async () => {
  $("ptt-record").disabled = true;
  $("ptt-record").textContent = "Press a key…";
  const key = await run(() => invoke("record_ptt_key"));
  $("ptt-record").disabled = false;
  $("ptt-record").textContent = "Record";
  if (key) {
    $("ptt-key").value = key;
    settings.save({ pttKey: key });
    setStatus(`Push-to-talk key: ${key}`);
  }
});

// Voice activation level: slider from -80 to -10 dBFS, meter over the same range.
const METER_MIN = -80;
const METER_MAX = -10;
const meterPercent = (db) => Math.max(0, Math.min(100, ((db - METER_MIN) / (METER_MAX - METER_MIN)) * 100));

function showThreshold(db) {
  $("va-value").textContent = `${db} dB`;
  $("meter-mark").style.left = `${meterPercent(db)}%`;
}

$("va-threshold").addEventListener("input", () => {
  const db = Number($("va-threshold").value);
  showThreshold(db);
  invoke("set_va_threshold", { db });
  settings.save({ vaThreshold: db });
});

listen("mic-level", (ev) => {
  const db = ev.payload;
  $("meter-fill").style.width = `${meterPercent(db)}%`;
  $("meter-fill").classList.toggle("open", db >= Number($("va-threshold").value));
});

async function loadAudioDevices(saved) {
  const devices = await run(() => invoke("audio_devices"));
  if (!devices) return;
  for (const [kind, list] of [["input", devices.inputs], ["output", devices.outputs]]) {
    const select = $(`${kind}-device`);
    select.replaceChildren(
      el("option", { value: "", textContent: "System default" }),
      ...list.map((d) => el("option", { value: d.id, textContent: d.name })),
    );
    const wanted = saved[`${kind}Device`];
    if (wanted && list.some((d) => d.id === wanted)) {
      select.value = wanted;
      await run(() => invoke("set_audio_device", { kind, id: wanted }));
    }
    select.addEventListener("change", async () => {
      await run(() => invoke("set_audio_device", { kind, id: select.value || null }));
      settings.save({ [`${kind}Device`]: select.value });
    });
  }
}

async function loadScreens() {
  const screens = (await run(() => invoke("list_screens"))) || [];
  $("screens").replaceChildren(...screens.map((s) => el("option", { value: String(s.id), textContent: s.title })));
}

// ------------------------------------------------------------------- init

async function init() {
  const saved = settings.load();
  const [build, opts] = await Promise.all([invoke("build_info"), invoke("startup_options")]);
  $("dev-panel").hidden = !build.debug;

  if (saved.pttKey) $("ptt-key").value = saved.pttKey;
  const mode = opts.voiceActivation ? "voiceActivation" : saved.mode || "pushToTalk";
  document.querySelector(`input[name=mode][value=${mode}]`).checked = true;
  await invoke("set_voice_mode", { mode });
  if (saved.pttKey) await run(() => invoke("set_ptt_key", { key: saved.pttKey }));
  if (saved.shareHeight) $("share-height").value = saved.shareHeight;
  if (saved.shareFps) $("share-fps").value = saved.shareFps;
  if (saved.shareCapture) $("share-capture").value = saved.shareCapture;
  const early = await invoke("early_prefs");
  $("e2ee-mode").value = settings.load().e2eeMode || "off";
  $("e2ee-mode").addEventListener("change", () => settings.save({ e2eeMode: $("e2ee-mode").value }));
  $("low-latency").checked = early.lowLatencyPlayback;
  $("low-latency").addEventListener("change", async () => {
    const lowLatencyPlayback = $("low-latency").checked;
    await run(() => invoke("set_early_prefs", { prefs: { ...early, lowLatencyPlayback } }));
    $("low-latency-hint").hidden = lowLatencyPlayback === early.lowLatencyPlayback;
  });
  if (saved.cameraQuality) $("camera-quality").value = saved.cameraQuality;
  const threshold = saved.vaThreshold ?? -50;
  $("va-threshold").value = String(threshold);
  showThreshold(threshold);
  await invoke("set_va_threshold", { db: threshold });
  for (const identity of saved.localMutes || []) {
    await run(() => invoke("set_local_mute", { identity, muted: true }));
  }
  for (const identity of new Set([...Object.keys(saved.volumes || {}), ...Object.keys(saved.compressors || {})])) {
    await run(() => invoke("set_listen_prefs", { identity, prefs: listenPrefs(identity) }));
  }
  await loadAudioDevices(saved);

  // Command line options are for development: they use the dev token flow.
  if (opts.server) $("dev-server").value = opts.server;
  if (opts.name) $("dev-name").value = opts.name;
  if (opts.room) $("dev-room").value = opts.room;
  if (opts.join) {
    $("login-view").hidden = false;
    $("dev-panel").hidden = false;
    $("dev-panel").open = true;
    $("dev-form").requestSubmit();
    if (opts.share) {
      const started = setInterval(() => {
        if (!$("share-start").hidden) {
          clearInterval(started);
          $("share-start").click();
        }
      }, 200);
    }
    return;
  }

  try {
    const account = await invoke("restore");
    if (account) await showMain(account);
    else showLogin();
  } catch (e) {
    showLogin(String(e));
  }
}

init();
