// Minimal UI for the media prototype. All media runs in the Rust core;
// this page only sends commands and renders state snapshots.
"use strict";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);

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
// Test instances started with command line options do not overwrite settings.
let persistForm = true;

function setStatus(text, isError = false) {
  $("status").textContent = text;
  $("status").classList.toggle("error", isError);
}

function setConnected(connected) {
  $("join").disabled = connected;
  $("leave").disabled = !connected;
  $("share").disabled = !connected;
  for (const id of ["server", "name", "room"]) $(id).disabled = connected;
  if (!connected) {
    $("room-title").textContent = "Not connected";
    $("participants").replaceChildren();
    $("share-start").disabled = false;
    $("share-stop").disabled = true;
  }
}

function fmt(value, unit, digits = 1) {
  return value == null ? "-" : `${value.toFixed(digits)} ${unit}`;
}

function renderStats(s) {
  if (!s) return "";
  const parts = [
    `rtt ${fmt(s.rttMs, "ms")}`,
    `jitter ${fmt(s.jitterMs, "ms")}`,
    `loss ${fmt(s.lossPct, "%")}`,
  ];
  if (s.jitterBufferMs != null) parts.push(`jb ${fmt(s.jitterBufferMs, "ms")}`);
  if (s.videoSize) {
    parts.push(`video ${s.videoSize[0]}x${s.videoSize[1]} @ ${fmt(s.videoFps, "fps", 0)}`);
    parts.push(`decode ${fmt(s.decodeMs, "ms")}`);
  }
  return parts.join(" | ");
}

function renderRoom(snapshot) {
  $("room-title").textContent = `Room: ${snapshot.room}`;
  const items = snapshot.participants.map((p) => {
    const li = document.createElement("li");
    const who = document.createElement("div");
    who.className = "who";
    const dot = document.createElement("span");
    dot.className = "dot";
    dot.classList.toggle("speaking", p.speaking);
    const name = document.createElement("span");
    name.textContent = p.isLocal ? `${p.name} (you)` : p.name;
    name.title = p.identity;
    who.append(dot, name);
    if (p.micMuted) who.append(badge("muted"));
    if (p.sharingScreen) who.append(badge("screen"));
    li.append(who);
    const stats = document.createElement("div");
    stats.className = "stats";
    stats.textContent = renderStats(p.stats);
    li.append(stats);
    if (p.isLocal) {
      $("tx").textContent = p.transmitting ? "transmitting" : "not transmitting";
      $("tx").classList.toggle("on", p.transmitting);
    }
    return li;
  });
  $("participants").replaceChildren(...items);
}

function badge(text) {
  const b = document.createElement("span");
  b.className = "badge";
  b.textContent = text;
  return b;
}

async function loadScreens() {
  try {
    const screens = await invoke("list_screens");
    $("screens").replaceChildren(
      ...screens.map((s) => {
        const o = document.createElement("option");
        o.value = String(s.id);
        o.textContent = s.title;
        return o;
      }),
    );
  } catch (e) {
    setStatus(`Cannot list screens: ${e}`, true);
  }
}

let shareAfterJoin = false;

$("join-form").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  $("join").disabled = true;
  setStatus("Connecting...");
  try {
    const info = await invoke("join", {
      server: $("server").value.trim(),
      name: $("name").value.trim(),
      room: $("room").value.trim(),
    });
    setConnected(true);
    setStatus(`Connected as ${info.identity} via ${info.livekitUrl}`);
    if (persistForm) {
      settings.save({
        server: $("server").value.trim(),
        name: $("name").value.trim(),
        room: $("room").value.trim(),
      });
    }
    await loadScreens();
    if (shareAfterJoin) $("share-start").click();
  } catch (e) {
    $("join").disabled = false;
    setStatus(String(e), true);
  }
});

$("leave").addEventListener("click", async () => {
  await invoke("leave");
  setConnected(false);
  setStatus("Left the room");
});

for (const radio of document.querySelectorAll("input[name=mode]")) {
  radio.addEventListener("change", () => {
    invoke("set_voice_mode", { mode: radio.value });
    if (persistForm) settings.save({ mode: radio.value });
  });
}

$("ptt-set").addEventListener("click", async () => {
  try {
    const key = await invoke("set_ptt_key", { key: $("ptt-key").value.trim() });
    setStatus(`Push-to-talk key: ${key}`);
    if (persistForm) settings.save({ pttKey: $("ptt-key").value.trim() });
  } catch (e) {
    setStatus(String(e), true);
  }
});

$("share-start").addEventListener("click", async () => {
  $("share-start").disabled = true;
  try {
    const id = $("screens").value;
    await invoke("start_share", { sourceId: id ? Number(id) : null });
    $("share-stop").disabled = false;
    setStatus("Sharing screen");
  } catch (e) {
    $("share-start").disabled = false;
    setStatus(String(e), true);
  }
});

$("share-stop").addEventListener("click", async () => {
  await invoke("stop_share");
  $("share-start").disabled = false;
  $("share-stop").disabled = true;
  setStatus("Stopped sharing");
});

listen("room-state", (ev) => renderRoom(ev.payload));
listen("disconnected", (ev) => {
  setConnected(false);
  setStatus(`Disconnected: ${ev.payload}`, true);
});
async function loadAudioDevices(saved) {
  try {
    const devices = await invoke("audio_devices");
    for (const [kind, list] of [["input", devices.inputs], ["output", devices.outputs]]) {
      const select = $(`${kind}-device`);
      const def = document.createElement("option");
      def.value = "";
      def.textContent = "System default";
      select.replaceChildren(
        def,
        ...list.map((d) => {
          const o = document.createElement("option");
          o.value = d.id;
          o.textContent = d.name;
          return o;
        }),
      );
      const wanted = saved[`${kind}Device`];
      if (wanted && list.some((d) => d.id === wanted)) {
        select.value = wanted;
        await invoke("set_audio_device", { kind, id: wanted });
      }
      select.addEventListener("change", async () => {
        try {
          await invoke("set_audio_device", { kind, id: select.value || null });
          settings.save({ [`${kind}Device`]: select.value });
        } catch (e) {
          setStatus(String(e), true);
        }
      });
    }
  } catch (e) {
    setStatus(`Cannot list audio devices: ${e}`, true);
  }
}

async function init() {
  const saved = settings.load();
  const opts = await invoke("startup_options");
  persistForm = !(opts.server || opts.name || opts.room);
  if (persistForm) {
    for (const key of ["server", "name", "room"]) if (saved[key]) $(key).value = saved[key];
    if (saved.pttKey) $("ptt-key").value = saved.pttKey;
    if (saved.mode === "voiceActivation") opts.voiceActivation = true;
  }
  // The core registers the default key at startup; registering again from
  // here applies a saved key and surfaces conflicts (e.g. a second instance).
  $("ptt-set").click();
  await loadAudioDevices(saved);

  if (opts.server) $("server").value = opts.server;
  if (opts.name) $("name").value = opts.name;
  if (opts.room) $("room").value = opts.room;
  if (opts.voiceActivation) {
    document.querySelector("input[value=voiceActivation]").checked = true;
    await invoke("set_voice_mode", { mode: "voiceActivation" });
  }
  shareAfterJoin = opts.share;
  if (opts.join) $("join-form").requestSubmit();
}

init();
