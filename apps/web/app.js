// Tinlink web client. Talks to the server it is served from; media runs in
// the browser through the vendored LiveKit client. No framework, no build step.
import {
  Gate, MutedSpeechDetector, TypingTracker, can, cameraGrid, cameraSize, highlightRanges, searchTerms, inputName, isUnread, mentionCandidates, mentionQuery, shareBitrate,
  shareSize, sortMembers, typingText,
} from "./logic.js";
import { createSettings, hasSettings } from "./admin.js";
import { avatar, chooseAndUpload, imageUrl, initials } from "./images.js";

const LK = window.LivekitClient;

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

/** Runs `fn` and reports failures in the status line. */
async function run(fn) {
  try {
    return await fn();
  } catch (e) {
    setStatus(e.message || String(e), true);
    return undefined;
  }
}

// Remembered settings and the session. Storage may be unavailable (private
// mode); the page then works until it is closed.
const store = {
  get(key) {
    try {
      return JSON.parse(localStorage.getItem(key));
    } catch {
      return null;
    }
  },
  set(key, value) {
    try {
      if (value === null) localStorage.removeItem(key);
      else localStorage.setItem(key, JSON.stringify(value));
    } catch {
      // ignore
    }
  },
};
const settings = {
  load: () => store.get("tinlink.settings") || {},
  save: (patch) => store.set("tinlink.settings", { ...settings.load(), ...patch }),
};

const state = {
  session: null, // { token, user }
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
  pendingFiles: [],
  view: "group", // "group" or "dms"
  dms: [], // direct message channels from GET /dms
  typing: new TypingTracker(),
};

/** What both sides may do in a direct message channel. */
const DM_PERMISSIONS = "VIEW_CHANNEL | SEND_MESSAGES | ATTACH_FILES";

// ------------------------------------------------------------------- api

class ApiError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

async function request(method, path, body, isForm = false) {
  const headers = {};
  if (state.session) headers.authorization = `Bearer ${state.session.token}`;
  if (body !== undefined && !isForm) headers["content-type"] = "application/json";
  const res = await fetch(`/api${path}`, {
    method,
    headers,
    body: body === undefined ? undefined : isForm ? body : JSON.stringify(body),
  });
  const text = await res.text();
  let data = null;
  try {
    data = text ? JSON.parse(text) : null;
  } catch {
    data = text;
  }
  if (!res.ok) {
    if (res.status === 401 && state.session && path !== "/users/me/password") {
      sessionExpired();
    }
    const message = data?.error || (typeof data === "string" && data) || res.statusText;
    throw new ApiError(res.status, message);
  }
  return data;
}

const api = (method, path, body) => request(method, path, body);

function sessionExpired() {
  store.set("tinlink.session", null);
  state.session = null;
  leaveVoice();
  closeGateway();
  showLogin("Your session has expired, please log in again.");
}

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
      } else if (f.type === "textarea") {
        input = el("textarea", { rows: 4, value: f.value || "", maxLength: 4000 });
      } else {
        input = el("input", {
          type: f.type || "text", value: f.value || "", placeholder: f.placeholder || "",
          autocomplete: f.autocomplete || "off",
        });
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
        values[id] = input.type === "checkbox" ? input.checked
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
    return e.message || String(e);
  }
}

// ----------------------------------------------------------------- login

let registering = false;

function showLogin(error = "") {
  $("main-view").hidden = true;
  $("login-view").hidden = false;
  $("login-error").textContent = error;
  const saved = settings.load();
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
  const name = $("login-name").value.trim();
  const password = $("login-password").value;
  try {
    const body = { name, password };
    if (registering) body.display_name = $("login-display").value.trim() || null;
    const session = await api("POST", registering ? "/auth/register" : "/auth/login", body);
    state.session = { token: session.token, user: session.user };
    store.set("tinlink.session", state.session);
    settings.save({ userName: name });
    await showMain();
  } catch (e) {
    $("login-error").textContent = e.status === 401 ? "Wrong user name or password." : e.message;
  } finally {
    $("login-submit").disabled = false;
  }
});

$("logout").addEventListener("click", async () => {
  cancelRejoin();
  settings.save({ lastVoice: null });
  await leaveVoice();
  await api("POST", "/auth/logout").catch(() => {});
  closeGateway();
  store.set("tinlink.session", null);
  state.session = null;
  showLogin();
});

function renderMe() {
  const user = state.session.user;
  $("me").replaceChildren(avatar(user), el("span", { class: "ellipsis", textContent: `${user.display_name || user.name} @ ${location.host}` }));
}

$("me").addEventListener("click", async () => {
  const user = state.session.user;
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
  const values = await ask({
    title: "Your profile",
    text: el("div", {},
      el("div", { class: "row profile-avatar" },
        preview,
        el("button", {
          type: "button", class: "small", textContent: "Choose picture…",
          onclick: () => run(() => chooseAndUpload(setAvatar)),
        }),
        removeButton),
      el("p", { class: "hint", textContent: `User name: @${user.name} (cannot be changed)` }),
      el("button", {
        type: "button", class: "link", textContent: "Change password…",
        onclick: () => {
          wantsPassword = true;
          $("dialog-cancel").click();
        },
      })),
    fields: [{ id: "display_name", label: "Display name", value: user.display_name }],
    ok: "Save",
    submit: (v) => {
      if (!v.display_name) return "Enter a display name.";
      if (v.display_name === state.session.user.display_name) return null;
      return tryText(async () => onUserUpdated(await api("PATCH", "/users/me", { display_name: v.display_name })));
    },
  });
  if (values) setStatus("Profile saved");
  if (wantsPassword) changePassword();
});

async function changePassword() {
  await ask({
    title: "Change password",
    fields: [
      { id: "current", label: "Current password", type: "password", autocomplete: "current-password" },
      { id: "next", label: "New password (at least 8 characters)", type: "password", autocomplete: "new-password" },
      { id: "repeat", label: "Repeat new password", type: "password", autocomplete: "new-password" },
    ],
    submit: (v) => {
      if (v.next !== v.repeat) return "The new passwords differ.";
      return tryText(() => api("PUT", "/users/me/password", {
        current_password: v.current, new_password: v.next,
      }));
    },
  }) && setStatus("Password changed; other sessions were signed out.");
}

// ------------------------------------------------------------------ main

async function showMain() {
  $("login-view").hidden = true;
  $("main-view").hidden = false;
  const user = state.session.user;
  renderMe();
  state.view = "group";
  state.dms = [];
  $("member-panel").hidden = false;
  openGateway();
  await Promise.all([loadGroups(), loadDms()]);
  const saved = settings.load();
  const groupId = state.groups.some((g) => g.id === saved.groupId) ? saved.groupId : state.groups[0]?.id;
  if (groupId) await selectGroup(groupId);
  else renderEmptyGroup();
  rejoinLastVoice();
}

function renderEmptyGroup() {
  state.group = null;
  $("group-name").textContent = "No group yet";
  $("group-invite").hidden = true;
  $("group-settings").hidden = true;
  $("channel-tree").replaceChildren(
    el("p", { class: "muted", style: "padding: 0 12px", textContent: "Create a group or join one with an invite code (+ on the left)." }),
  );
  openChannel(null);
}

async function loadGroups() {
  state.groups = (await run(() => api("GET", "/groups"))) || [];
  renderGroups();
}

/** Reloads the group list soon (unread markers of other groups). */
function refreshGroups() {
  clearTimeout(refreshGroups.timer);
  refreshGroups.timer = setTimeout(loadGroups, 300);
}

function renderGroups() {
  $("group-list").replaceChildren(...state.groups.map((g) => {
    const current = state.view === "group" && g.id === state.group?.id;
    return el("li", {},
      !current && g.unread ? el("span", { class: "unread-pip" }) : null,
      el("button", {
        type: "button",
        class: current ? "active" : "",
        title: g.name,
        onclick: () => selectGroup(g.id),
      }, g.icon_id ? el("img", { src: imageUrl(g.icon_id), alt: "" }) : initials(g.name)),
      !current && g.mention_count ? el("span", { class: "badge-count", textContent: String(g.mention_count) }) : null);
  }));
  updateTitle();
}

function updateTitle() {
  const mentions = state.groups.reduce((n, g) => n + (g.mention_count || 0), 0)
    + state.dms.reduce((n, d) => n + (d.unread_count || 0), 0);
  document.title = mentions ? `(${mentions}) Tinlink` : "Tinlink";
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
      el("p", { textContent: `Share this code and the address ${location.origin}; others enter the code under + on the left.` }),
      el("div", { class: "code", textContent: invite.code })),
    cancel: false,
  });
});

async function fetchGroup(id) {
  return Promise.all([
    run(() => api("GET", `/groups/${id}`)),
    run(() => api("GET", `/groups/${id}/members`)),
    run(() => api("GET", `/groups/${id}/channels`)),
  ]);
}

async function selectGroup(id) {
  const [group, members, channels] = await fetchGroup(id);
  if (!group) return;
  state.view = "group";
  state.group = group;
  settings.save({ groupId: id });
  $("member-panel").hidden = false;
  renderDmButton();
  setMembers(members || []);
  setChannels(channels);
  renderGroups();
  $("group-name").textContent = group.name;
  $("group-invite").hidden = !can(group.permissions, "MANAGE_GROUP");
  $("group-settings").hidden = !hasSettings(group.permissions);
  const saved = settings.load().channels || {};
  const channel = state.channels.find((c) => c.id === saved[id]) || state.channels[0] || null;
  openChannel(channel);
}

function setMembers(members) {
  state.members = new Map(members.map((m) => [m.user_id, m]));
  renderMembers();
}

function renderMembers() {
  const me = state.session?.user.id;
  $("member-list").replaceChildren(...sortMembers([...state.members.values()]).map((m) => {
    const other = m.user_id !== me;
    return el("li", {
      class: `${m.online ? "" : "offline"}${other ? " clickable" : ""}`,
      title: other ? "Send a direct message" : "",
      dataset: { user: m.name },
      onclick: other ? () => openDm(m.user_id) : null,
    },
    el("span", { class: `dot${m.online ? " online" : ""}` }),
    avatar(m, "medium"),
    el("span", { class: "ellipsis", textContent: m.display_name || m.name }));
  }));
}

function setChannels(list) {
  state.categories = (list?.categories || []).sort((a, b) => a.position - b.position);
  state.channels = (list?.channels || []).sort((a, b) => a.position - b.position || a.name.localeCompare(b.name));
  state.voice = list?.voice || [];
  renderTree();
}

/** Reloads channels and members of the current group (debounced). */
function refreshGroup() {
  clearTimeout(refreshGroup.timer);
  refreshGroup.timer = setTimeout(async () => {
    if (!state.group) return;
    const id = state.group.id;
    const [group, members, channels] = await fetchGroup(id);
    if (state.group?.id !== id || !group) return;
    state.group = group;
    setMembers(members || []);
    setChannels(channels);
    if (state.view !== "group") return;
    $("group-name").textContent = group.name;
    $("group-invite").hidden = !can(group.permissions, "MANAGE_GROUP");
    $("group-settings").hidden = !hasSettings(group.permissions);
    if (settingsView.isOpen) {
      $("channel-name").textContent = `Settings · ${group.name}`;
      settingsView.refresh();
      return;
    }
    if (state.channel) {
      const fresh = state.channels.find((c) => c.id === state.channel.id);
      if (!fresh) openChannel(state.channels[0] || null);
      else {
        state.channel = fresh;
        // Permissions may have changed.
        $("composer").hidden = !can(fresh.permissions, "SEND_MESSAGES");
        $("attach").hidden = !can(fresh.permissions, "ATTACH_FILES");
      }
    }
  }, 150);
}

// --------------------------------------------------------------- settings

const settingsView = createSettings({
  api, el, ask, tryText, setStatus, state,
  // The open channel may have been deleted meanwhile.
  onClose: () => openChannel(state.channels.find((c) => c.id === state.channel?.id) || state.channels[0] || null),
  onGroupDeleted: () => onGatewayEvent({ type: "group_deleted", group_id: state.group?.id }),
});

$("group-settings").addEventListener("click", () => {
  if (settingsView.isOpen) {
    settingsView.close();
    openChannel(state.channels.find((c) => c.id === state.channel?.id) || state.channels[0] || null);
    return;
  }
  closeSearch();
  for (const id of ["messages", "typing", "composer", "pending-files"]) $(id).hidden = true;
  $("channel-name").textContent = `Settings · ${state.group.name}`;
  settingsView.open();
});

// --------------------------------------------------------------- channels

function memberName(userId) {
  const m = state.members.get(userId);
  return m ? m.display_name || m.name : "unknown";
}

function renderTree() {
  if (state.view === "dms") {
    renderDmList();
    return;
  }
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
  tree.replaceChildren(...nodes);
  updateSpeaking();
}

function channelNodes(ch) {
  const canJoin = ch.voice_enabled && can(ch.permissions, "CONNECT");
  const active = ch.id === state.channel?.id;
  const unread = !active && isUnread(ch);
  const row = el("div", { class: `channel${active ? " active" : ""}${unread ? " unread" : ""}` },
    el("button", {
      type: "button", class: "name", title: ch.name,
      onclick: () => openChannel(ch),
    }, channelIcon(ch), el("span", { class: "ellipsis", textContent: `${ch.name}${ch.temporary ? " ⏱" : ""}` })),
    !active && ch.mention_count ? el("span", { class: "mentions", textContent: String(ch.mention_count) }) : null,
    canJoin ? el("button", {
      type: "button", class: "join", title: "Join voice", textContent: "🔊",
      onclick: () => joinVoice(ch),
    }) : null);
  const users = state.voice.filter((v) => v.channel_id === ch.id);
  const list = users.length ? el("ul", { class: "voice-users" }, ...users.map((v) => {
    const m = state.members.get(v.user_id);
    const moderate = canModerate() && v.user_id !== state.session?.user.id;
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

/** The channel's icon image, emoji or a plain `#`. */
function channelIcon(ch) {
  if (ch.icon_id) return el("img", { class: "channel-icon", src: imageUrl(ch.icon_id), alt: "" });
  return el("span", { class: "channel-icon", textContent: ch.emoji || "#" });
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
  closeSearch();
  settingsView.close();
  for (const id of ["messages", "typing", "pending-files"]) $(id).hidden = false;
  state.channel = ch;
  state.messages = [];
  state.noOlder = false;
  state.pendingFiles = [];
  renderPending();
  hideAutocomplete();
  $("messages").replaceChildren();
  if (!ch) {
    $("channel-name").textContent = state.view === "dms" ? "No conversation" : state.group ? "No channel" : "Select a channel";
    $("composer").hidden = true;
    renderTyping();
    renderTree();
    return;
  }
  if (state.group && !ch.direct) {
    const channels = settings.load().channels || {};
    settings.save({ channels: { ...channels, [state.group.id]: ch.id } });
  }
  $("channel-name").textContent = ch.direct ? `@ ${ch.name}` : `# ${ch.name}`;
  renderTyping();
  $("composer").hidden = !can(ch.permissions, "SEND_MESSAGES");
  $("attach").hidden = !can(ch.permissions, "ATTACH_FILES");
  renderTree();
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
    state.messages = page;
    box.replaceChildren(...page.map(renderMessage));
    box.scrollTop = box.scrollHeight;
    markRead();
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

function mentionsMe(m) {
  return (m.mentions || []).includes(state.session?.user.id);
}

function renderMessage(m) {
  const mine = m.author_id === state.session?.user.id;
  const canDelete = mine || can(state.channel?.permissions, "MANAGE_MESSAGES");
  const actions = (mine || canDelete) ? el("span", { class: "actions" },
    mine ? el("button", { type: "button", textContent: "Edit", onclick: () => editMessage(m) }) : null,
    canDelete ? el("button", { type: "button", textContent: "Delete", onclick: () => deleteMessage(m) }) : null) : null;
  const author = userInfo(m.author_id) || { name: m.author_name || "?" };
  return el("div", { class: `message${mentionsMe(m) ? " mention" : ""}`, dataset: { id: m.id } },
    avatar(author, "medium"),
    el("div", { class: "body" },
      el("div", { class: "meta" },
        el("strong", { textContent: author.display_name || m.author_name || "deleted user" }),
        el("span", { textContent: formatTime(m.created_at) + (m.edited_at ? " (edited)" : "") }),
        actions),
      m.content ? el("div", { class: "text", textContent: m.content }) : null,
      ...(m.attachments || []).map(renderAttachment)));
}

/** What we know about a user: name, display name and avatar. */
function userInfo(id) {
  if (!id) return null;
  if (id === state.session?.user.id) return state.session.user;
  return state.members.get(id) || state.dms.find((d) => d.user.id === id)?.user || null;
}

async function editMessage(m) {
  await ask({
    title: "Edit message",
    fields: [{ id: "content", label: "Message", type: "textarea", value: m.content }],
    ok: "Save",
    submit: (v) => {
      if (!v.content) return "A message cannot be empty; delete it instead.";
      return tryText(async () => {
        replaceMessage(await api("PATCH", `/channels/${m.channel_id}/messages/${m.id}`, { content: v.content }));
      });
    },
  });
}

async function deleteMessage(m) {
  const ok = await ask({ title: "Delete message?", text: m.content || "(attachment)", ok: "Delete" });
  if (!ok) return;
  if (await run(() => api("DELETE", `/channels/${m.channel_id}/messages/${m.id}`)) !== undefined) removeMessage(m.id);
}

function renderAttachment(a) {
  if (a.content_type.startsWith("image/")) {
    return el("div", { class: "attachment" },
      el("a", { href: a.url, target: "_blank", rel: "noopener", title: a.filename },
        el("img", { src: a.url, alt: a.filename, loading: "lazy" })));
  }
  return el("div", { class: "attachment" },
    el("a", { href: a.url, download: a.filename, class: "small", textContent: `📎 ${a.filename} (${formatSize(a.size_bytes)})` }));
}

function addMessage(m) {
  if (m.channel_id !== state.channel?.id || state.messages.some((x) => x.id === m.id)) return;
  const box = $("messages");
  const atBottom = box.scrollHeight - box.scrollTop - box.clientHeight < 60;
  state.messages.push(m);
  box.append(renderMessage(m));
  if (atBottom) box.scrollTop = box.scrollHeight;
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

/** The user is looking at the open channel. */
function watching() {
  return document.visibilityState === "visible" && document.hasFocus();
}

/** Moves the read marker of the open channel to its newest message (debounced). */
function markRead() {
  clearTimeout(markRead.timer);
  markRead.timer = setTimeout(async () => {
    const ch = state.channel;
    const last = state.messages[state.messages.length - 1];
    if (!ch || !last || !watching()) return;
    if (ch.last_read_id && ch.last_read_id >= last.id && !ch.mention_count) return;
    try {
      await api("PUT", `/channels/${ch.id}/read`, { message_id: last.id });
    } catch {
      return;
    }
    ch.last_read_id = last.id;
    ch.last_message_id = ch.last_message_id && ch.last_message_id > last.id ? ch.last_message_id : last.id;
    ch.mention_count = 0;
    if (ch.direct) {
      const dm = state.dms.find((d) => d.id === ch.id);
      if (dm) Object.assign(dm, { unread_count: 0, last_read_id: last.id });
      renderDmButton();
    } else {
      refreshGroups();
    }
    renderTree();
  }, 400);
}

window.addEventListener("focus", markRead);
document.addEventListener("visibilitychange", markRead);

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

$("composer").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  const ch = state.channel;
  const content = $("message-input").value.trim();
  if (!ch || (!content && !state.pendingFiles.length)) return;
  const send = $("composer").querySelector("button[type=submit]");
  send.disabled = true;
  try {
    const ids = [];
    for (const file of state.pendingFiles) {
      setStatus(`Uploading ${file.name}…`);
      const form = new FormData();
      form.append("file", file, file.name);
      const info = await request("POST", `/channels/${ch.id}/attachments`, form, true);
      ids.push(info.id);
    }
    const message = await api("POST", `/channels/${ch.id}/messages`, { content, attachment_ids: ids });
    state.pendingFiles = [];
    renderPending();
    $("message-input").value = "";
    typingSentAt = 0;
    setStatus("");
    addMessage(message);
    ch.last_message_id = message.id;
    ch.last_read_id = message.id;
    $("messages").scrollTop = $("messages").scrollHeight;
  } catch (e) {
    setStatus(e.message || String(e), true);
  } finally {
    send.disabled = false;
  }
});

// ---------------------------------------------------- mention autocomplete

const ac = { items: [], index: 0, query: null };

function hideAutocomplete() {
  ac.items = [];
  ac.query = null;
  $("autocomplete").hidden = true;
}

function updateAutocomplete() {
  const input = $("message-input");
  const query = state.channel?.direct ? null : mentionQuery(input.value, input.selectionStart);
  const members = [...state.members.values()].filter((m) => m.user_id !== state.session?.user.id);
  const items = query ? mentionCandidates(members, query.prefix) : [];
  if (!items.length) {
    hideAutocomplete();
    return;
  }
  ac.items = items;
  ac.query = query;
  ac.index = Math.min(ac.index, items.length - 1);
  $("autocomplete").replaceChildren(...items.map((m, i) => el("li", {
    class: i === ac.index ? "active" : "",
    textContent: m.display_name && m.display_name !== m.name ? `${m.display_name} (@${m.name})` : `@${m.name}`,
    onmousedown: (ev) => {
      ev.preventDefault();
      pickMention(i);
    },
  })));
  $("autocomplete").hidden = false;
}

function pickMention(i) {
  const input = $("message-input");
  const m = ac.items[i];
  const { start } = ac.query;
  const end = input.selectionStart;
  input.value = `${input.value.slice(0, start)}@${m.name} ${input.value.slice(end)}`;
  const caret = start + m.name.length + 2;
  input.setSelectionRange(caret, caret);
  hideAutocomplete();
  input.focus();
}

$("message-input").addEventListener("input", () => {
  updateAutocomplete();
  sendTyping();
});
$("message-input").addEventListener("blur", () => setTimeout(hideAutocomplete, 100));
$("message-input").addEventListener("keydown", (ev) => {
  if (ac.items.length) {
    if (ev.key === "ArrowDown" || ev.key === "ArrowUp") {
      ev.preventDefault();
      ac.index = (ac.index + (ev.key === "ArrowDown" ? 1 : ac.items.length - 1)) % ac.items.length;
      updateAutocomplete();
      return;
    }
    if (ev.key === "Enter" || ev.key === "Tab") {
      ev.preventDefault();
      pickMention(ac.index);
      return;
    }
    if (ev.key === "Escape") {
      hideAutocomplete();
      return;
    }
  }
  if (ev.key === "Enter" && !ev.shiftKey) {
    ev.preventDefault();
    $("composer").requestSubmit();
  }
});

// ----------------------------------------------------------------- search

const search = { query: "", hits: [] };

function closeSearch() {
  $("search-results").hidden = true;
  $("messages").hidden = false;
}

async function runSearch(query, more = false) {
  const before = more ? search.hits.at(-1)?.id : null;
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
    ? `${search.hits.length}${page.length === 50 ? "+" : ""} results for “${query}”`
    : `No results for “${query}”`;
  $("search-more").hidden = page.length < 50;
  $("messages").hidden = true;
  $("search-results").hidden = false;
}

/** Text with the matched words wrapped in <mark>, built from DOM nodes. */
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
    let dm = state.dms.find((d) => d.id === h.channel_id);
    if (!dm) {
      await loadDms();
      dm = state.dms.find((d) => d.id === h.channel_id);
    }
    if (!dm) return;
    if (state.view !== "dms") showDms();
    await openChannel(dmChannel(dm));
  } else {
    if (state.view !== "group" || state.group?.id !== h.group_id) await selectGroup(h.group_id);
    const ch = state.channels.find((c) => c.id === h.channel_id);
    if (!ch) return;
    await openChannel(ch);
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

// ------------------------------------------------------------ notifications

function askNotificationPermission() {
  if ("Notification" in window && Notification.permission === "default") {
    Notification.requestPermission().catch(() => {});
  }
}

function notifyMention(m) {
  if (!("Notification" in window) || Notification.permission !== "granted") return;
  const ch = state.channels.find((c) => c.id === m.channel_id);
  const dm = state.dms.find((d) => d.id === m.channel_id);
  if (dm) {
    const n = new Notification(`${m.author_name || "Someone"} (direct message)`, {
      body: m.content.slice(0, 200) || "(attachment)",
      tag: m.id,
    });
    n.onclick = () => {
      window.focus();
      openDm(dm.user.id);
      n.close();
    };
    return;
  }
  const n = new Notification(`${m.author_name || "Someone"}${ch ? ` in #${ch.name}` : ""}`, {
    body: m.content.slice(0, 200),
    tag: m.id,
  });
  n.onclick = () => {
    window.focus();
    const group = state.groups.find((g) => g.id === ch?.group_id);
    if (ch && state.group?.id === ch.group_id) openChannel(ch);
    else if (group) selectGroup(group.id);
    n.close();
  };
}

/** Bookkeeping for a new message: unread markers, mention badge, notification. */
function onMessageCreated(e) {
  const m = e.data;
  const mine = m.author_id === state.session?.user.id;
  const mentioned = mentionsMe(m);
  const open = state.channel?.id === m.channel_id;
  if (m.author_id) state.typing.remove(m.channel_id, m.author_id);
  if (open) renderTyping();
  if (!e.group_id) {
    onDirectMessage(m, mine, open);
    return;
  }
  if (state.group?.id === e.group_id) {
    const ch = state.channels.find((c) => c.id === m.channel_id);
    if (ch) {
      ch.last_message_id = m.id;
      if (mine) ch.last_read_id = m.id;
      else if (mentioned && !(open && watching())) ch.mention_count = (ch.mention_count || 0) + 1;
      renderTree();
    }
  } else if (!mine) {
    refreshGroups();
  }
  if (open) {
    addMessage(m);
    markRead();
  }
  if (mentioned && !mine && !(open && watching())) notifyMention(m);
}

// --------------------------------------------------------- direct messages

async function loadDms() {
  const list = await run(() => api("GET", "/dms"));
  if (!list) return;
  state.dms = list;
  renderDmButton();
  if (state.view === "dms") renderDmList();
}

function renderDmButton() {
  const unread = state.dms.reduce((n, d) => n + (d.unread_count || 0), 0);
  $("dm-button").classList.toggle("active", state.view === "dms");
  $("dm-entry").querySelector(".badge-count")?.remove();
  if (unread && state.view !== "dms") {
    $("dm-entry").append(el("span", { class: "badge-count", textContent: String(unread) }));
  }
  updateTitle();
}

/** The chat view of a direct message channel. */
function dmChannel(dm) {
  return {
    id: dm.id,
    direct: true,
    name: dm.user.display_name || dm.user.name,
    permissions: DM_PERMISSIONS,
    last_message_id: dm.last_message_id,
    last_read_id: dm.last_read_id,
    mention_count: 0,
  };
}

function showDms() {
  state.view = "dms";
  $("group-name").textContent = "Direct messages";
  $("group-invite").hidden = true;
  $("group-settings").hidden = true;
  $("member-panel").hidden = true;
  renderGroups();
  renderDmButton();
  const open = state.channel?.direct ? state.dms.find((d) => d.id === state.channel.id) : null;
  openChannel(open ? dmChannel(open) : state.dms[0] ? dmChannel(state.dms[0]) : null);
}

function renderDmList() {
  const tree = $("channel-tree");
  if (!state.dms.length) {
    tree.replaceChildren(el("p", {
      class: "muted", style: "padding: 0 12px",
      textContent: "No conversations yet. Click a member in a group's member list to write to them.",
    }));
    return;
  }
  tree.replaceChildren(...state.dms.map((dm) => {
    const active = dm.id === state.channel?.id;
    const unread = !active && dm.unread_count > 0;
    return el("div", { class: `channel dm${active ? " active" : ""}${unread ? " unread" : ""}` },
      el("button", {
        type: "button", class: "name", title: `@${dm.user.name}`,
        onclick: () => openChannel(dmChannel(dm)),
      },
      el("span", { class: `dot${dm.user.online ? " online" : ""}` }),
      avatar(dm.user, "medium"),
      el("span", { class: "ellipsis", textContent: dm.user.display_name || dm.user.name })),
      unread ? el("span", { class: "mentions", textContent: String(dm.unread_count) }) : null);
  }));
}

/** Opens (or creates) the conversation with a user and switches to it. */
async function openDm(userId) {
  let dm = state.dms.find((d) => d.user.id === userId);
  if (!dm) {
    dm = await run(() => api("POST", "/dms", { user_id: userId }));
    if (!dm) return;
    state.dms = [dm, ...state.dms.filter((d) => d.id !== dm.id)];
  }
  if (state.view !== "dms") showDms();
  openChannel(dmChannel(dm));
}

$("dm-button").addEventListener("click", () => {
  if (state.view !== "dms") showDms();
});

function onDirectMessage(m, mine, open) {
  const dm = state.dms.find((d) => d.id === m.channel_id);
  if (!dm) {
    // A new conversation.
    loadDms();
  } else {
    dm.last_message_id = m.id;
    if (mine) dm.last_read_id = m.id;
    else if (!(open && watching())) dm.unread_count = (dm.unread_count || 0) + 1;
    state.dms = [dm, ...state.dms.filter((d) => d !== dm)];
    renderDmButton();
    if (state.view === "dms") renderDmList();
  }
  if (open) {
    addMessage(m);
    markRead();
  }
  if (!mine && !(open && watching())) notifyMention(m);
}

function onPresence(userId, online, inGroup) {
  const member = inGroup ? state.members.get(userId) : null;
  if (member && member.online !== online) {
    member.online = online;
    renderMembers();
  }
  const dm = state.dms.find((d) => d.user.id === userId);
  if (dm && dm.user.online !== online) {
    dm.user.online = online;
    if (state.view === "dms") renderDmList();
  }
}

/** Another user (or we, in another session) changed name or avatar. */
function onUserUpdated(user) {
  const profile = { display_name: user.display_name, avatar_id: user.avatar_id };
  if (user.id === state.session?.user.id) {
    Object.assign(state.session.user, profile);
    store.set("tinlink.session", state.session);
    renderMe();
  }
  const member = state.members.get(user.id);
  if (member) {
    Object.assign(member, profile);
    renderMembers();
    renderTree();
  }
  const dm = state.dms.find((d) => d.user.id === user.id);
  if (dm) {
    Object.assign(dm.user, profile);
    if (state.view === "dms") renderDmList();
  }
  // Author names and avatars in the open chat.
  for (const m of state.messages) {
    if (m.author_id === user.id) {
      m.author_name = user.display_name;
      replaceMessage(m);
    }
  }
}

// ----------------------------------------------------------------- typing

let typingSentAt = 0;

/** Tells others we are typing, at most every 3 s. */
function sendTyping() {
  const ch = state.channel;
  if (!ch || !$("message-input").value.trim() || gateway?.readyState !== WebSocket.OPEN) return;
  const now = Date.now();
  if (now - typingSentAt < 3000) return;
  typingSentAt = now;
  gateway.send(JSON.stringify({ type: "typing", channel_id: ch.id }));
}

function renderTyping() {
  clearTimeout(renderTyping.timer);
  const names = state.channel ? state.typing.names(state.channel.id, Date.now()) : [];
  $("typing").textContent = typingText(names);
  // Re-check when the notices expire.
  if (names.length) renderTyping.timer = setTimeout(renderTyping, 1000);
}

// ---------------------------------------------------------------- gateway

let gateway = null;
let gatewayRetry = 0;
let gatewayTimer = null;

function openGateway() {
  closeGateway();
  const url = `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/api/gateway`;
  const ws = new WebSocket(url);
  gateway = ws;
  ws.onopen = () => ws.send(JSON.stringify({ type: "auth", token: state.session.token }));
  ws.onmessage = (msg) => {
    let e;
    try {
      e = JSON.parse(msg.data);
    } catch {
      return;
    }
    if (e.type === "ready") {
      const reconnected = gatewayRetry > 0;
      gatewayRetry = 0;
      setGatewayStatus(true);
      // Catch up on what happened while disconnected.
      if (reconnected) onGatewayEvent({ type: "resync" });
      return;
    }
    if (e.type === "error") {
      ws.close();
      return;
    }
    onGatewayEvent(e);
  };
  ws.onclose = () => {
    if (gateway !== ws) return;
    setGatewayStatus(false);
    const delay = Math.min(30, 2 ** gatewayRetry++) * 1000;
    gatewayTimer = setTimeout(() => {
      if (state.session) openGateway();
    }, delay);
  };
}

function closeGateway() {
  clearTimeout(gatewayTimer);
  if (gateway) {
    const ws = gateway;
    gateway = null;
    ws.close();
  }
  setGatewayStatus(false);
}

function setGatewayStatus(connected) {
  $("gateway-dot").classList.toggle("online", connected);
  $("gateway-dot").title = connected ? "Live updates connected" : "Reconnecting…";
}

function onGatewayEvent(e) {
  const inGroup = state.group && e.group_id === state.group.id;
  switch (e.type) {
    case "message_created":
      onMessageCreated(e);
      break;
    case "message_updated":
      replaceMessage(e.data);
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
      if (e.user_id === state.session?.user.id) {
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
    case "presence_changed":
      onPresence(e.user_id, e.data.online, inGroup);
      break;
    case "user_updated":
      onUserUpdated(e.data);
      break;
    case "typing":
      if (e.user_id !== state.session?.user.id) {
        const name = state.dms.find((d) => d.id === e.channel_id)?.user.display_name
          || (state.members.has(e.user_id) ? memberName(e.user_id) : e.data.name);
        state.typing.add(e.channel_id, e.user_id, name, Date.now());
        if (state.channel?.id === e.channel_id) renderTyping();
      }
      break;
    case "resync":
      loadGroups();
      loadDms();
      refreshGroup();
      if (state.channel) loadMessages();
      break;
    default:
      // channel_created/updated/deleted, member_joined, group_updated, voice_mute_changed
      if (inGroup) refreshGroup();
      if (e.type === "group_updated" || e.type === "member_joined") refreshGroups();
  }
}

// ------------------------------------------------------------------ voice

const voice = {
  room: null,
  channel: null,
  micTrack: null,
  pttDown: false,
  vaOpen: false,
  meter: null, // { node, source, clone }
  gate: new Gate(-50, 500),
  detector: new MutedSpeechDetector(),
  speaking: new Set(), // identities
};

let audioContext = null;

/** Creates the audio context on a user gesture, as browsers require. */
function unlockAudio() {
  try {
    audioContext ??= new AudioContext();
    audioContext.resume();
  } catch {
    // no level meter and no beeps then
  }
}
document.addEventListener("pointerdown", () => {
  unlockAudio();
  askNotificationPermission();
}, { once: true });

function voiceMode() {
  return document.querySelector("input[name=mode]:checked").value;
}

function transmitting() {
  return voiceMode() === "pushToTalk" ? voice.pttDown : voice.vaOpen;
}

/** Mutes or unmutes the published microphone to match PTT / voice activation. */
function applyTransmit() {
  const track = voice.micTrack;
  if (!track) return;
  const on = transmitting();
  if (on === track.isMuted) {
    (on ? track.unmute() : track.mute()).catch(() => {});
  }
  $("tx").textContent = on ? "transmitting" : "not transmitting";
  $("tx").classList.toggle("on", on);
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

/** One microphone level sample from the worklet. */
function onLevel(db) {
  const now = performance.now();
  voice.vaOpen = voice.gate.update(db, now);
  if (voiceMode() === "voiceActivation") applyTransmit();
  // Treat anything at the voice activation level as speech.
  const speaking = db >= voice.gate.thresholdDb;
  const serverMuted = !voice.room?.localParticipant.permissions?.canPublish;
  if (voice.detector.update(speaking, transmitting() && !serverMuted, now)) {
    playMutedBeep();
    $("muted-hint").hidden = false;
    clearTimeout(onLevel.hintTimer);
    onLevel.hintTimer = setTimeout(() => { $("muted-hint").hidden = true; }, 6000);
  }
  drawLevel(db);
}

function drawLevel(db) {
  if (!$("voice-settings").open) return;
  const pct = (v) => `${Math.max(0, Math.min(100, ((v + 80) / 70) * 100))}%`;
  $("level-fill").style.width = pct(db);
  $("level-mark").style.left = pct(voice.gate.thresholdDb);
}

let workletLoaded = null;

/** Measures the microphone with a clone of its track, so muting does not silence the meter. */
async function startMeter(track) {
  stopMeter();
  if (!audioContext) return;
  try {
    workletLoaded ??= audioContext.audioWorklet.addModule("level-worklet.js");
    await workletLoaded;
    const clone = track.mediaStreamTrack.clone();
    clone.enabled = true;
    const source = audioContext.createMediaStreamSource(new MediaStream([clone]));
    const node = new AudioWorkletNode(audioContext, "level-meter");
    node.port.onmessage = (ev) => onLevel(ev.data);
    source.connect(node);
    voice.meter = { node, source, clone };
  } catch (e) {
    // Without a meter voice activation stays open.
    console.warn("microphone level meter unavailable", e);
    voice.vaOpen = true;
  }
}

function stopMeter() {
  if (!voice.meter) return;
  voice.meter.source.disconnect();
  voice.meter.node.port.onmessage = null;
  voice.meter.node.disconnect();
  voice.meter.clone.stop();
  voice.meter = null;
}

/** Joins a channel's voice; returns whether it worked. */
async function joinVoice(ch, { quiet = false } = {}) {
  unlockAudio();
  if (!quiet) setStatus(`Joining ${ch.name}…`);
  try {
    const token = await api("POST", `/channels/${ch.id}/voice-token`, {});
    await leaveVoice({ keepRejoin: true });
    const saved = settings.load();
    const room = new LK.Room({
      adaptiveStream: true,
      dynacast: true,
      audioCaptureDefaults: {
        echoCancellation: true, noiseSuppression: true, autoGainControl: true,
        deviceId: saved.inputDevice || undefined,
      },
      audioOutput: { deviceId: saved.outputDevice || undefined },
    });
    voice.room = room;
    voice.channel = ch;
    wireRoom(room);
    await room.connect(token.url, token.token);
    if (voice.room !== room) return false;
    await publishMic();
  } catch (e) {
    await leaveVoice({ keepRejoin: true });
    if (!quiet) setStatus(`Cannot join voice: ${e.message || e}`, true);
    return false;
  }
  cancelRejoin();
  setStatus(`Connected to voice in ${ch.name}`);
  voiceJoinedUi(ch);
  settings.save({ lastVoice: { groupId: ch.group_id, channelId: ch.id } });
  renderParticipants();
  return true;
}

/** Publishes the microphone muted (PTT) or open, as the mode says. */
async function publishMic() {
  const room = voice.room;
  if (!room) return;
  try {
    voice.micTrack ??= await LK.createLocalAudioTrack({
      echoCancellation: true, noiseSuppression: true, autoGainControl: true,
      deviceId: settings.load().inputDevice || undefined,
    });
    await startMeter(voice.micTrack);
    if (!transmitting()) await voice.micTrack.mute();
    await room.localParticipant.publishTrack(voice.micTrack, {
      source: LK.Track.Source.Microphone, dtx: true, red: true,
    });
    applyTransmit();
  } catch (e) {
    // A member muted by a moderator may not publish; they still listen.
    console.warn("microphone not published", e);
    if (e.name === "NotAllowedError") setStatus("Microphone access was denied by the browser.", true);
  }
}

function micPublished() {
  return !!voice.room?.localParticipant.getTrackPublication(LK.Track.Source.Microphone);
}

function wireRoom(room) {
  const E = LK.RoomEvent;
  room
    .on(E.TrackSubscribed, (track, pub, participant) => {
      if (track.kind === "audio") {
        const audio = track.attach();
        $("audio-sink").append(audio);
        applyVolume(participant);
      } else if (pub.source === LK.Track.Source.ScreenShare) {
        showScreen(participant, track);
      } else if (pub.source === LK.Track.Source.Camera) {
        showCamera(participant, track);
      }
      renderParticipants();
    })
    .on(E.TrackUnsubscribed, (track, pub) => {
      for (const node of track.detach()) node.remove();
      document.querySelector(`.screen[data-sid="${track.sid}"]`)?.remove();
      removeCamera(pub.trackSid);
      renderParticipants();
    })
    .on(E.ActiveSpeakersChanged, (speakers) => {
      voice.speaking = new Set(speakers.map((p) => p.identity));
      renderParticipants();
      updateSpeaking();
    })
    .on(E.ParticipantConnected, renderParticipants)
    .on(E.ParticipantDisconnected, renderParticipants)
    .on(E.TrackMuted, renderParticipants)
    .on(E.TrackUnmuted, renderParticipants)
    .on(E.TrackPublished, renderParticipants)
    .on(E.TrackUnpublished, renderParticipants)
    .on(E.LocalTrackPublished, (pub) => {
      if (pub.source === LK.Track.Source.Camera && pub.track) showCamera(room.localParticipant, pub.track);
      renderParticipants();
    })
    .on(E.LocalTrackUnpublished, (pub) => {
      if (pub.source === LK.Track.Source.ScreenShare) shareStopped();
      if (pub.source === LK.Track.Source.Camera) {
        removeCamera(pub.trackSid);
        cameraStopped();
      }
      renderParticipants();
    })
    .on(E.ParticipantPermissionsChanged, (_prev, participant) => {
      if (!participant?.isLocal) return;
      // A moderator lifted the server mute: publish the microphone again.
      const p = participant.permissions;
      const allowed = p?.canPublish && (!p.canPublishSources?.length
        || p.canPublishSources.includes(LK.TrackSource?.MICROPHONE ?? 2));
      if (allowed && !micPublished()) publishMic();
      renderParticipants();
    })
    .on(E.AudioPlaybackStatusChanged, () => {
      $("audio-start").hidden = room.canPlaybackAudio;
    })
    .on(E.Reconnecting, () => {
      if (voice.channel) $("voice-status").textContent = `Voice: # ${voice.channel.name} (reconnecting…)`;
    })
    .on(E.Reconnected, () => {
      if (voice.channel) $("voice-status").textContent = `Voice: # ${voice.channel.name}`;
    })
    .on(E.Disconnected, (reason) => {
      if (voice.room !== room) return;
      const ch = voice.channel;
      cleanupVoice();
      const R = LK.DisconnectReason;
      const final = [R.CLIENT_INITIATED, R.PARTICIPANT_REMOVED, R.ROOM_DELETED, R.DUPLICATE_IDENTITY].includes(reason);
      if (!final && ch) {
        scheduleRejoin(ch, 0);
      } else {
        // Removed by a moderator, room deleted, ...: do not come back by itself.
        // (A move is followed by voice_moved, which joins the new channel.)
        if (reason !== R.CLIENT_INITIATED) {
          settings.save({ lastVoice: null });
          setStatus("Voice disconnected", true);
        }
      }
    });
}

$("audio-start").addEventListener("click", async () => {
  await voice.room?.startAudio();
  $("audio-start").hidden = !!voice.room?.canPlaybackAudio;
});

function voiceJoinedUi(ch) {
  $("voice-status").textContent = `Voice: # ${ch.name}`;
  $("voice-status").classList.remove("muted");
  $("voice-leave").hidden = false;
  $("share-start").hidden = !can(ch.permissions, "SHARE_SCREEN") || !navigator.mediaDevices?.getDisplayMedia;
  $("share-stop").hidden = true;
  $("share-info").hidden = true;
  cameraStopped();
  $("tx").hidden = false;
  applyTransmit();
}

/** Resets everything voice related after leaving or losing the room. */
function cleanupVoice() {
  stopMeter();
  voice.micTrack?.stop();
  voice.micTrack = null;
  voice.room = null;
  voice.channel = null;
  voice.speaking.clear();
  $("voice-status").textContent = "Not in voice";
  $("voice-status").classList.add("muted");
  for (const id of ["voice-leave", "share-start", "share-stop", "cam-start", "cam-stop", "share-info", "tx", "muted-hint", "audio-start"]) $(id).hidden = true;
  $("participants").replaceChildren();
  $("audio-sink").replaceChildren();
  $("stage").replaceChildren();
  $("cameras").replaceChildren();
  updateSpeaking();
}

async function leaveVoice({ keepRejoin = false } = {}) {
  if (!keepRejoin) cancelRejoin();
  const room = voice.room;
  if (!room) return;
  cleanupVoice();
  await room.disconnect().catch(() => {});
}

$("voice-leave").addEventListener("click", async () => {
  settings.save({ lastVoice: null });
  await leaveVoice();
});

/** After loading: back into the voice channel the page was closed in. */
async function rejoinLastVoice() {
  const last = settings.load().lastVoice;
  if (!last || voice.room) return;
  let channels = state.group?.id === last.groupId ? state.channels : null;
  if (!channels) {
    const list = await api("GET", `/groups/${last.groupId}/channels`).catch(() => null);
    channels = list?.channels || [];
  }
  const ch = channels.find((c) => c.id === last.channelId);
  if (!ch || !can(ch.permissions, "CONNECT")) {
    settings.save({ lastVoice: null });
    return;
  }
  // Browsers block audio until the user interacts with the page.
  const ok = await ask({ title: "Rejoin voice?", text: `You were in # ${ch.name}.`, ok: "Rejoin" });
  if (ok) await joinVoice(ch);
  else settings.save({ lastVoice: null });
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
    if (voice.room) return;
    if (!(await joinVoice(ch, { quiet: true }))) scheduleRejoin(ch, attempt + 1);
  }, delay * 1000);
}

function cancelRejoin() {
  clearTimeout(rejoinTimer);
  rejoinTimer = null;
}

// ------------------------------------------------------- participant list

function localMutes() {
  return new Set(settings.load().localMutes || []);
}

function volumes() {
  return settings.load().volumes || {};
}

/** Applies the local mute and volume of a remote participant. */
function applyVolume(participant) {
  if (participant.isLocal) return;
  const muted = localMutes().has(participant.identity);
  const volume = volumes()[participant.identity] ?? 1;
  participant.setVolume(muted ? 0 : volume);
}

function renderParticipants() {
  const room = voice.room;
  const list = $("participants");
  if (!room) {
    list.replaceChildren();
    return;
  }
  const people = [room.localParticipant, ...room.remoteParticipants.values()];
  const mutes = localMutes();
  list.replaceChildren(...people.map((p) => {
    const mic = p.getTrackPublication(LK.Track.Source.Microphone);
    const screen = p.getTrackPublication(LK.Track.Source.ScreenShare);
    const camera = p.getTrackPublication(LK.Track.Source.Camera);
    const micMuted = !mic || mic.isMuted;
    const name = p.name || p.identity;
    const actions = p.isLocal ? null : el("span", { class: "actions" },
      screen && !document.querySelector(`.screen[data-identity="${CSS.escape(p.identity)}"]`)
        ? el("button", { type: "button", class: "small", textContent: "Watch", onclick: () => watchScreen(p) })
        : null,
      el("button", {
        type: "button", class: `small${mutes.has(p.identity) ? " on" : ""}`,
        textContent: mutes.has(p.identity) ? "Unmute" : "Mute",
        title: "Mute or unmute for you only",
        onclick: () => toggleLocalMute(p),
      }),
      el("input", {
        type: "range", class: "volume", min: "0", max: "1", step: "0.05",
        value: String(volumes()[p.identity] ?? 1), title: "Volume (for you only)",
        oninput: (ev) => setVolume(p, Number(ev.target.value)),
      }));
    return el("li", {},
      el("div", { class: "who" },
        el("span", { class: `dot${voice.speaking.has(p.identity) ? " speaking" : ""}` }),
        el("span", { title: p.identity, textContent: p.isLocal ? `${name} (you)` : name }),
        micMuted ? el("span", { class: "badge", textContent: "muted" }) : null,
        screen ? el("span", { class: "badge", textContent: "screen" }) : null,
        camera ? el("span", { class: "badge", textContent: "cam" }) : null,
        actions),
      el("div", { class: "stats", textContent: p.connectionQuality && p.connectionQuality !== "unknown" ? `connection ${p.connectionQuality}` : "" }));
  }));
}

function toggleLocalMute(p) {
  const mutes = localMutes();
  if (mutes.has(p.identity)) mutes.delete(p.identity);
  else mutes.add(p.identity);
  settings.save({ localMutes: [...mutes] });
  applyVolume(p);
  renderParticipants();
}

function setVolume(p, value) {
  settings.save({ volumes: { ...volumes(), [p.identity]: value } });
  applyVolume(p);
}

/** Lights up speakers in the channel tree (identities are "name@domain"). */
function updateSpeaking() {
  const speaking = new Set([...voice.speaking].map((id) => id.split("@")[0]));
  for (const li of document.querySelectorAll(".voice-users li")) {
    li.querySelector(".dot").classList.toggle("speaking", speaking.has(li.dataset.user));
  }
  for (const tile of document.querySelectorAll(".camera")) {
    tile.classList.toggle("speaking", voice.speaking.has(tile.dataset.identity));
  }
}

// -------------------------------------------------------------- screens

function showScreen(participant, track) {
  const video = track.attach();
  video.muted = true;
  video.playsInline = true;
  video.title = "Double-click for fullscreen";
  video.addEventListener("dblclick", () => {
    if (document.fullscreenElement) document.exitFullscreen();
    else video.requestFullscreen().catch(() => {});
  });
  const tile = el("div", { class: "screen", dataset: { sid: track.sid, identity: participant.identity } },
    video,
    el("span", { class: "label", textContent: `${participant.name || participant.identity} - screen` }),
    el("button", {
      type: "button", class: "small close", textContent: "Close", title: "Close (reopen with Watch)",
      onclick: () => {
        track.detach(video);
        tile.remove();
        renderParticipants();
      },
    }));
  $("stage").append(tile);
}

function watchScreen(p) {
  const pub = p.getTrackPublication(LK.Track.Source.ScreenShare);
  if (pub?.track) showScreen(p, pub.track);
  renderParticipants();
}

$("share-start").addEventListener("click", async () => {
  const room = voice.room;
  if (!room) return;
  $("share-start").disabled = true;
  const height = Number($("share-resolution").value);
  const fps = Number($("share-fps").value);
  const size = shareSize(height);
  try {
    const pub = await room.localParticipant.setScreenShareEnabled(true, {
      audio: true,
      contentHint: fps > 30 ? "motion" : "detail",
      resolution: size ? { width: size[0], height: size[1], frameRate: fps } : { frameRate: fps },
      selfBrowserSurface: "exclude",
      surfaceSwitching: "include",
    }, {
      videoCodec: "h264",
      // One full-resolution layer: text stays sharp for every viewer.
      simulcast: false,
      screenShareEncoding: { maxBitrate: shareBitrate(height, fps), maxFramerate: fps },
    });
    const media = pub?.track?.mediaStreamTrack;
    if (!media) return; // picker cancelled
    // Browsers treat the capture size as a hint; enforce the limits.
    const limits = { frameRate: { max: fps } };
    if (size) Object.assign(limits, { width: { max: size[0] }, height: { max: size[1] } });
    await media.applyConstraints(limits).catch(() => {});
    const s = media.getSettings();
    $("share-info").textContent = `Sharing ${s.width}×${s.height} @ up to ${fps} fps`;
    $("share-info").hidden = false;
    $("share-start").hidden = true;
    $("share-stop").hidden = false;
  } catch (e) {
    if (e.name !== "NotAllowedError") setStatus(`Cannot share the screen: ${e.message || e}`, true);
  } finally {
    $("share-start").disabled = false;
  }
});

$("share-stop").addEventListener("click", async () => {
  await run(() => voice.room?.localParticipant.setScreenShareEnabled(false));
  shareStopped();
});

function shareStopped() {
  if (!voice.room) return;
  $("share-start").hidden = !can(voice.channel?.permissions, "SHARE_SCREEN");
  $("share-stop").hidden = true;
  $("share-info").hidden = true;
}

// -------------------------------------------------------------- cameras

/** Adds a grid tile for a camera track, local (mirrored preview) or remote. */
function showCamera(participant, track) {
  if (!track.sid || document.querySelector(`.camera[data-sid="${CSS.escape(track.sid)}"]`)) return;
  const video = track.attach();
  video.muted = true;
  video.playsInline = true;
  video.title = "Double-click for fullscreen";
  video.addEventListener("dblclick", () => {
    if (document.fullscreenElement) document.exitFullscreen();
    else video.requestFullscreen().catch(() => {});
  });
  const name = participant.name || participant.identity;
  const tile = el("div", {
    class: `camera${participant.isLocal ? " local" : ""}${voice.speaking.has(participant.identity) ? " speaking" : ""}`,
    dataset: { sid: track.sid, identity: participant.identity },
  },
  video,
  el("span", { class: "label", textContent: participant.isLocal ? `${name} (you)` : name }));
  // Own camera first, the others in join order.
  if (participant.isLocal) $("cameras").prepend(tile);
  else $("cameras").append(tile);
  layoutCameras();
}

function removeCamera(sid) {
  if (!sid) return;
  const tile = document.querySelector(`.camera[data-sid="${CSS.escape(sid)}"]`);
  if (!tile) return;
  tile.querySelector("video")?.remove();
  tile.remove();
  layoutCameras();
}

function layoutCameras() {
  const { cols, rows } = cameraGrid($("cameras").children.length);
  $("cameras").style.setProperty("--cols", cols);
  $("cameras").style.setProperty("--rows", rows);
}

function cameraAllowed() {
  return !!voice.room && can(voice.channel?.permissions, "SHARE_SCREEN") && !!navigator.mediaDevices?.getUserMedia;
}

function cameraOn() {
  return !!voice.room?.localParticipant.getTrackPublication(LK.Track.Source.Camera);
}

function cameraStopped() {
  $("cam-start").hidden = !cameraAllowed() || cameraOn();
  $("cam-stop").hidden = !cameraOn();
}

$("cam-start").addEventListener("click", async () => {
  const room = voice.room;
  if (!room || cameraOn()) return;
  $("cam-start").disabled = true;
  const [width, height] = cameraSize(Number($("camera-quality").value));
  let track = null;
  try {
    track = await LK.createLocalVideoTrack({
      deviceId: settings.load().cameraDevice || undefined,
      resolution: { width, height, frameRate: 30 },
    });
    if (voice.room !== room) throw new Error("left the channel");
    // Simulcast: small grid tiles receive a lower layer than fullscreen viewers.
    await room.localParticipant.publishTrack(track, {
      source: LK.Track.Source.Camera, simulcast: true, videoCodec: "vp8",
    });
  } catch (e) {
    track?.stop();
    if (e.name === "NotAllowedError") setStatus("Camera access was denied by the browser.", true);
    else setStatus(`Cannot start the camera: ${e.message || e}`, true);
  } finally {
    $("cam-start").disabled = false;
    cameraStopped();
  }
});

$("cam-stop").addEventListener("click", async () => {
  const pub = voice.room?.localParticipant.getTrackPublication(LK.Track.Source.Camera);
  // Unpublishing also stops the capture, so the camera light goes off.
  if (pub?.track) await run(() => voice.room.localParticipant.unpublishTrack(pub.track));
  cameraStopped();
});

// --------------------------------------------------------- voice settings

for (const radio of document.querySelectorAll("input[name=mode]")) {
  radio.addEventListener("change", () => {
    settings.save({ mode: radio.value });
    applyTransmit();
  });
}

$("va-threshold").addEventListener("input", () => {
  voice.gate.thresholdDb = Number($("va-threshold").value);
  settings.save({ vaThreshold: voice.gate.thresholdDb });
});

// Push-to-talk: only while the page has the focus (browsers have no global hotkeys).
let recordingKey = false;

function pttKey() {
  return $("ptt-key").value;
}

function typingInField(ev) {
  const t = ev.target;
  return t instanceof HTMLElement && (t.tagName === "TEXTAREA" || t.tagName === "INPUT" || t.isContentEditable);
}

function onPttInput(ev, down) {
  const name = inputName(ev);
  if (!name) return;
  if (recordingKey && down) {
    ev.preventDefault();
    recordingKey = false;
    $("ptt-key").value = name;
    $("ptt-record").textContent = "Record";
    settings.save({ pttKey: name });
    setStatus(`Push-to-talk key: ${name}`);
    return;
  }
  if (name !== pttKey()) return;
  // Letters and the like keep working for typing.
  if (ev.type.startsWith("key") && name.length > 3 && !name.startsWith("F") && typingInField(ev)) return;
  if (ev.type.startsWith("key") && name.startsWith("Key") && typingInField(ev)) return;
  if (ev.repeat) return;
  if (voice.pttDown !== down) {
    voice.pttDown = down;
    applyTransmit();
  }
  if (!name.startsWith("Key")) ev.preventDefault();
}

window.addEventListener("keydown", (ev) => onPttInput(ev, true));
window.addEventListener("keyup", (ev) => onPttInput(ev, false));
window.addEventListener("mousedown", (ev) => onPttInput(ev, true));
window.addEventListener("mouseup", (ev) => onPttInput(ev, false));
window.addEventListener("blur", () => {
  if (voice.pttDown) {
    voice.pttDown = false;
    applyTransmit();
  }
});

$("ptt-record").addEventListener("click", (ev) => {
  ev.stopPropagation();
  // Wait for the click to finish, so it is not recorded as Mouse1.
  setTimeout(() => {
    recordingKey = true;
    $("ptt-record").textContent = "Press a key…";
  }, 0);
});

async function loadAudioDevices() {
  if (!navigator.mediaDevices?.enumerateDevices) return;
  const devices = await navigator.mediaDevices.enumerateDevices();
  const saved = settings.load();
  const selects = [["audioinput", $("input-device"), "inputDevice"], ["audiooutput", $("output-device"), "outputDevice"], ["videoinput", $("camera-device"), "cameraDevice"]];
  for (const [kind, select, key] of selects) {
    const list = devices.filter((d) => d.kind === kind && d.deviceId && d.deviceId !== "default");
    select.replaceChildren(
      el("option", { value: "", textContent: "System default" }),
      ...list.map((d, i) => el("option", { value: d.deviceId, textContent: d.label || `Device ${i + 1}` })),
    );
    if (list.some((d) => d.deviceId === saved[key])) select.value = saved[key];
  }
  // Selecting an output needs setSinkId, which not every browser has.
  $("output-device").disabled = !("setSinkId" in HTMLMediaElement.prototype);
}

$("input-device").addEventListener("change", async () => {
  const id = $("input-device").value;
  settings.save({ inputDevice: id });
  const track = voice.micTrack;
  if (!track) return;
  await run(async () => {
    await track.restartTrack({ deviceId: id || undefined });
    await startMeter(track);
    applyTransmit();
  });
});

$("output-device").addEventListener("change", async () => {
  const id = $("output-device").value;
  settings.save({ outputDevice: id });
  await run(() => voice.room?.switchActiveDevice("audiooutput", id || "default"));
});

$("camera-device").addEventListener("change", async () => {
  const id = $("camera-device").value;
  settings.save({ cameraDevice: id });
  const pub = voice.room?.localParticipant.getTrackPublication(LK.Track.Source.Camera);
  if (pub?.track) await run(() => pub.track.restartTrack({ deviceId: id || undefined }));
});

// Device labels are only visible after the first microphone permission.
navigator.mediaDevices?.addEventListener?.("devicechange", loadAudioDevices);
$("voice-settings").addEventListener("toggle", loadAudioDevices);

window.addEventListener("beforeunload", () => {
  voice.room?.disconnect();
});

// ------------------------------------------------------------------- init

async function init() {
  const saved = settings.load();
  $("server-name").textContent = location.host;
  if (saved.pttKey) $("ptt-key").value = saved.pttKey;
  document.querySelector(`input[name=mode][value=${saved.mode === "voiceActivation" ? "voiceActivation" : "pushToTalk"}]`).checked = true;
  if (saved.vaThreshold != null) {
    $("va-threshold").value = String(saved.vaThreshold);
    voice.gate.thresholdDb = saved.vaThreshold;
  }
  if (saved.shareResolution) $("share-resolution").value = saved.shareResolution;
  if (saved.shareFps) $("share-fps").value = saved.shareFps;
  if (saved.cameraQuality) $("camera-quality").value = saved.cameraQuality;
  $("camera-quality").addEventListener("change", () => settings.save({ cameraQuality: $("camera-quality").value }));
  $("share-resolution").addEventListener("change", () => settings.save({ shareResolution: $("share-resolution").value }));
  $("share-fps").addEventListener("change", () => settings.save({ shareFps: $("share-fps").value }));
  api("GET", "/info").then((info) => {
    if (info?.source_url) $("source-link").href = info.source_url;
  }).catch(() => {});

  if (!LK) {
    setStatus("The voice library could not be loaded.", true);
  }
  const session = store.get("tinlink.session");
  if (!session?.token) {
    showLogin();
    return;
  }
  state.session = session;
  try {
    state.session.user = await api("GET", "/users/me");
    store.set("tinlink.session", state.session);
    await showMain();
  } catch (e) {
    if (e.status !== 401) showLogin(e.message);
  }
}

init();
