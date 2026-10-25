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
      const input = f.type === "checkbox"
        ? el("input", { type: "checkbox", checked: !!f.value })
        : el("input", { type: f.type || "text", value: f.value || "", placeholder: f.placeholder || "" });
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
        values[id] = input.type === "checkbox" ? input.checked : input.value.trim();
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
    await showMain(null);
    voiceJoined({ id: room, name: room, dev: true });
  }
});

$("logout").addEventListener("click", async () => {
  await run(() => invoke("logout"));
  voiceLeft();
  state.account = null;
  showLogin();
});

// ------------------------------------------------------------------ main

async function showMain(account) {
  state.account = account;
  $("login-view").hidden = true;
  $("main-view").hidden = false;
  $("me").textContent = account
    ? `${account.user.display_name || account.user.name} @ ${new URL(account.server).host}`
    : "Developer mode";
  $("groups").hidden = !account;
  $("logout").textContent = account ? "Log out" : "Back";
  await loadScreens();
  if (!account) return;
  await loadGroups();
  const saved = settings.load();
  const groupId = state.groups.some((g) => g.id === saved.groupId) ? saved.groupId : state.groups[0]?.id;
  if (groupId) await selectGroup(groupId);
  else renderEmptyGroup();
}

function renderEmptyGroup() {
  state.group = null;
  $("group-name").textContent = "No group yet";
  $("group-invite").hidden = true;
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
    el("button", {
      type: "button",
      class: g.id === state.group?.id ? "active" : "",
      title: g.name,
      textContent: initials(g.name),
      onclick: () => selectGroup(g.id),
    }))));
}

function initials(name) {
  return name.split(/\s+/).filter(Boolean).slice(0, 2).map((w) => w[0].toUpperCase()).join("") || "?";
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
  state.group = group;
  settings.save({ groupId: id });
  setMembers(members || []);
  setChannels(channels);
  renderGroups();
  $("group-name").textContent = group.name;
  $("group-invite").hidden = !can(group.permissions, "MANAGE_GROUP");
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
    $("group-name").textContent = group.name;
    setMembers(members || []);
    setChannels(channels);
    if (state.channel && !state.channels.some((c) => c.id === state.channel.id)) {
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
  const row = el("div", { class: `channel${ch.id === state.channel?.id ? " active" : ""}` },
    el("button", {
      type: "button", class: "name", title: ch.name,
      textContent: `# ${ch.name}${ch.temporary ? " ⏱" : ""}`,
      onclick: () => openChannel(ch),
    }),
    canJoin ? el("button", {
      type: "button", class: "join", title: "Join voice", textContent: "🔊",
      onclick: () => joinVoice(ch),
    }) : null);
  const users = state.voice.filter((v) => v.channel_id === ch.id);
  const list = users.length ? el("ul", { class: "voice-users" }, ...users.map((v) => {
    const m = state.members.get(v.user_id);
    return el("li", { dataset: { user: m?.name || v.user_id } },
      el("span", { class: "dot" }),
      el("span", { textContent: memberName(v.user_id) }));
  })) : null;
  return [row, list].filter(Boolean);
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
  if (state.group) {
    const channels = settings.load().channels || {};
    settings.save({ channels: { ...channels, [state.group.id]: ch.id } });
  }
  $("channel-name").textContent = `# ${ch.name}`;
  $("composer").hidden = !can(ch.permissions, "SEND_MESSAGES");
  $("attach").hidden = !can(ch.permissions, "ATTACH_FILES");
  loadMessages();
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
  const node = el("div", { class: "message", dataset: { id: m.id } },
    el("div", { class: "meta" },
      el("strong", { textContent: m.author_name || "deleted user" }),
      el("span", { textContent: formatTime(m.created_at) + (m.edited_at ? " (edited)" : "") })),
    m.content ? el("div", { class: "text", textContent: m.content }) : null,
    ...(m.attachments || []).map(renderAttachment));
  return node;
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

$("message-input").addEventListener("keydown", (ev) => {
  if (ev.key === "Enter" && !ev.shiftKey) {
    ev.preventDefault();
    $("composer").requestSubmit();
  }
});

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

// ---------------------------------------------------------------- gateway

listen("gateway", (ev) => {
  const e = ev.payload;
  const inGroup = state.group && e.group_id === state.group.id;
  switch (e.type) {
    case "message_created":
      addMessage(e.data);
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
    case "group_deleted":
    case "member_left":
      loadGroups().then(() => {
        if (state.group && !state.groups.some((g) => g.id === state.group.id)) {
          if (state.groups[0]) selectGroup(state.groups[0].id);
          else renderEmptyGroup();
        } else if (inGroup) refreshGroup();
      });
      break;
    case "resync":
      loadGroups();
      refreshGroup();
      if (state.channel) loadMessages();
      break;
    default:
      // channel_created/updated/deleted, member_joined, group_updated
      if (inGroup) refreshGroup();
      if (e.type === "group_updated") loadGroups();
  }
});

listen("gateway-status", (ev) => {
  $("gateway-dot").classList.toggle("online", ev.payload.connected);
  $("gateway-dot").title = ev.payload.connected ? "Live updates connected" : "Reconnecting…";
});

// ------------------------------------------------------------------ voice

async function joinVoice(ch) {
  setStatus(`Joining ${ch.name}…`);
  const info = await run(() => invoke("join_voice", { channelId: ch.id }));
  if (!info) return;
  setStatus(`Connected to voice in ${ch.name}`);
  voiceJoined(ch);
}

function voiceJoined(ch) {
  state.voiceChannel = ch;
  $("voice-status").textContent = `Voice: ${ch.dev ? ch.name : `# ${ch.name}`}`;
  $("voice-status").classList.remove("muted");
  $("voice-leave").hidden = false;
  $("share-start").hidden = !(ch.dev || can(ch.permissions, "SHARE_SCREEN"));
  $("share-stop").hidden = true;
  $("tx").hidden = false;
}

function voiceLeft() {
  state.voiceChannel = null;
  state.snapshot = null;
  $("voice-status").textContent = "Not in voice";
  $("voice-status").classList.add("muted");
  for (const id of ["voice-leave", "share-start", "share-stop", "tx"]) $(id).hidden = true;
  $("participants").replaceChildren();
  updateSpeaking();
}

$("voice-leave").addEventListener("click", async () => {
  await run(() => invoke("leave"));
  voiceLeft();
});

$("share-start").addEventListener("click", async () => {
  $("share-start").disabled = true;
  const id = $("screens").value;
  const info = await run(() => invoke("start_share", { sourceId: id ? Number(id) : null }));
  $("share-start").disabled = false;
  if (!info) return;
  $("share-start").hidden = true;
  $("share-stop").hidden = false;
  const encoder = info.encoder ?? "software H.264";
  setStatus(`Sharing ${info.width}x${info.height} @ ${info.fps} fps (${encoder})`);
});

$("share-stop").addEventListener("click", async () => {
  await run(() => invoke("stop_share"));
  $("share-start").hidden = false;
  $("share-stop").hidden = true;
  setStatus("Stopped sharing");
});

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

listen("room-state", (ev) => {
  const snapshot = ev.payload;
  state.snapshot = snapshot;
  $("participants").replaceChildren(...snapshot.participants.map((p) => {
    const who = el("div", { class: "who" },
      el("span", { class: `dot${p.speaking ? " speaking" : ""}` }),
      el("span", { textContent: p.isLocal ? `${p.name} (you)` : p.name, title: p.identity }),
      p.micMuted ? el("span", { class: "badge", textContent: "muted" }) : null,
      p.sharingScreen ? el("span", { class: "badge", textContent: "screen" }) : null);
    if (p.isLocal) {
      $("tx").textContent = p.transmitting ? "transmitting" : "not transmitting";
      $("tx").classList.toggle("on", p.transmitting);
    }
    return el("li", {}, who, el("div", { class: "stats", textContent: renderStats(p.stats) }));
  }));
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
  voiceLeft();
  setStatus(`Voice disconnected: ${ev.payload}`, true);
});

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
  // The core registers the default key at startup; registering again here
  // applies a saved key and surfaces conflicts (e.g. a second instance).
  $("ptt-set").click();
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
