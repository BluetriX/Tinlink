// Notification sounds, in packs: sounds/<pack>/<sound>.wav. The built-in
// packs are made by tools/sounds/generate.py (synthesizer and speech
// synthesis, no samples). Packs can also come from this computer (a folder
// of <sound>.wav files, see the `local` option); those never leave it.
// This file is shared verbatim by the web and the desktop client; CI checks
// both copies are the same.

/** Built-in packs; a pack may leave sounds out, those come from DEFAULT_PACK. */
export const PACKS = [
  { id: "retro", label: "Retro" },
  { id: "announcer", label: "Announcer (voice)" },
];
const DEFAULT_PACK = "retro";

/** Every sound; a pack folder names its files after these ids. */
export const SOUNDS = [
  { id: "logged-in", label: "Logged in" },
  { id: "connected", label: "You joined a voice channel" },
  { id: "disconnected", label: "You left voice" },
  { id: "channel-switched", label: "You switched to another voice channel" },
  { id: "user-joined", label: "Someone joined your channel" },
  { id: "user-left", label: "Someone left your channel" },
  { id: "moved", label: "You were moved to another channel" },
  { id: "removed", label: "You were removed from voice" },
  { id: "connection-lost", label: "Voice connection lost" },
  { id: "reconnected", label: "Voice connection back" },
  { id: "mic-muted", label: "Microphone muted" },
  { id: "mic-unmuted", label: "Microphone on again" },
  { id: "sound-muted", label: "Sound muted" },
  { id: "sound-unmuted", label: "Sound on again" },
  { id: "talking-muted", label: "Talking while nobody hears you" },
  { id: "message", label: "Direct message or mention" },
  { id: "role-granted", label: "You got a role" },
  { id: "role-revoked", label: "A role was taken from you" },
  { id: "admin-granted", label: "You became administrator" },
  { id: "admin-revoked", label: "You are no longer administrator" },
  { id: "registered", label: "Account created" },
  { id: "registration-failed", label: "Account could not be created" },
  { id: "wrong-password", label: "Wrong user name or password" },
  { id: "error", label: "Something went wrong" },
];

const DEFAULTS = { on: true, volume: 0.5, off: [], pack: DEFAULT_PACK };

/**
 * `load`/`save` read and patch the client's settings object; the sound
 * settings live under its `sounds` key. `local`, if given, supplies packs
 * from this computer:
 *   list(): Promise<[{ id, label, sounds: [sound id] }]>
 *   url(packId, soundId): Promise<string>  (a blob: URL)
 *   renderAdd(el, refresh): Node           (controls to add or remove packs)
 */
export function createSounds({ load, save, local = null }) {
  const cache = new Map(); // "pack/sound" -> Audio
  let localPacks = [];
  let sinkId = "";

  const current = () => ({ ...DEFAULTS, ...(load().sounds || {}) });
  const update = (patch) => save({ sounds: { ...current(), ...patch } });
  const isBuiltin = (id) => PACKS.some((p) => p.id === id);
  const localPack = (id) => localPacks.find((p) => p.id === id);

  function builtin(pack, id) {
    const key = `${pack}/${id}`;
    let base = cache.get(key);
    if (!base) {
      base = new Audio(`sounds/${key}.wav`);
      base.preload = "auto";
      if (pack !== DEFAULT_PACK) {
        base.addEventListener("error", () => cache.set(key, builtin(DEFAULT_PACK, id)), { once: true });
      }
      cache.set(key, base);
    }
    return base;
  }

  /** The Audio for a sound in a pack; null while a local file is loading. */
  function audio(pack, id) {
    if (isBuiltin(pack)) return builtin(pack, id);
    const lp = localPack(pack);
    if (!lp || !lp.sounds.includes(id)) return builtin(DEFAULT_PACK, id);
    const key = `${pack}/${id}`;
    const base = cache.get(key);
    if (base) return base;
    cache.set(key, null); // loading
    local.url(pack, id)
      .then((url) => {
        const a = new Audio(url);
        a.preload = "auto";
        cache.set(key, a);
      })
      .catch(() => cache.set(key, builtin(DEFAULT_PACK, id)));
    return null;
  }

  /** Loads every file of the chosen local pack, so the first play is instant. */
  function warm() {
    const s = current();
    const lp = localPack(s.pack);
    if (lp) for (const id of lp.sounds) audio(lp.id, id);
  }

  async function refresh() {
    if (!local) return;
    try {
      localPacks = await local.list();
    } catch {
      localPacks = [];
    }
    warm();
  }
  const ready = refresh();

  function start(base, volume) {
    const node = base.cloneNode();
    node.volume = Math.min(1, volume);
    if (sinkId && node.setSinkId) node.setSinkId(sinkId).catch(() => {});
    node.play().catch(() => {}); // autoplay not yet allowed: stay silent
  }

  /** Plays a sound unless sounds or this one are turned off. */
  function play(id, { force = false, pack = null } = {}) {
    const s = current();
    if (!force && (!s.on || s.off.includes(id))) return;
    if (s.volume <= 0) return;
    const chosen = pack || s.pack;
    const base = audio(chosen, id);
    if (base) {
      start(base, s.volume);
      return;
    }
    // A local file that is still loading: play it once it is there.
    const key = `${chosen}/${id}`;
    const wait = setInterval(() => {
      const b = cache.get(key);
      if (b) {
        clearInterval(wait);
        start(b, s.volume);
      }
    }, 20);
    setTimeout(() => clearInterval(wait), 2000);
  }

  /** Output device for sounds (browsers that support choosing one). */
  function setSink(id) {
    sinkId = id || "";
  }

  /** Builds the settings block: on/off, pack, volume and a list with previews. */
  function renderSettings(el) {
    const s = current();
    const pack = el("select", {
      id: "sound-pack",
      onchange: () => {
        update({ pack: pack.value });
        warm();
        play("connected", { force: true });
      },
    });
    const fillPacks = () => {
      const chosen = current().pack;
      pack.replaceChildren(
        ...PACKS.map((p) => el("option", { value: p.id, textContent: p.label })),
        ...localPacks.map((p) => el("option", { value: p.id, textContent: `${p.label} (this computer)` })),
      );
      if (![...pack.options].some((o) => o.value === chosen)) {
        pack.append(el("option", { value: chosen, textContent: `${chosen} (not found)` }));
      }
      pack.value = chosen;
    };
    fillPacks();
    ready.then(fillPacks);

    const on = el("input", {
      type: "checkbox", checked: s.on,
      onchange: () => {
        update({ on: on.checked });
        list.classList.toggle("disabled", !on.checked);
      },
    });
    const pct = el("span", { class: "muted", textContent: `${Math.round(s.volume * 100)} %` });
    const volume = el("input", {
      type: "range", min: 0, max: 100, step: 1, value: String(Math.round(s.volume * 100)),
      oninput: () => {
        update({ volume: Number(volume.value) / 100 });
        pct.textContent = `${volume.value} %`;
      },
      onchange: () => play("user-joined", { force: true }),
    });
    const list = el("ul", { class: `sound-list${s.on ? "" : " disabled"}` },
      ...SOUNDS.map(({ id, label }) => {
        const box = el("input", {
          type: "checkbox", checked: !s.off.includes(id),
          onchange: () => {
            const off = new Set(current().off);
            if (box.checked) off.delete(id); else off.add(id);
            update({ off: [...off] });
          },
        });
        return el("li", {},
          el("label", { class: "inline" }, box, ` ${label}`),
          el("button", {
            type: "button", class: "small", textContent: "▶", title: "Play",
            onclick: () => play(id, { force: true }),
          }));
      }));
    const add = local?.renderAdd
      ? local.renderAdd(el, async () => {
        await refresh();
        fillPacks();
      })
      : null;
    return el("div", { class: "sound-settings" },
      el("label", { class: "inline" }, on, " Sounds"),
      el("label", {}, "Sound pack ", pack),
      add,
      el("label", {}, "Sound volume ", pct, volume),
      list);
  }

  return { play, setSink, renderSettings };
}

// ------------------------------------------------- packs in the browser

const DB_NAME = "tinlink-sounds";
const isSoundFile = (name) => /^[a-z0-9-]+\.wav$/.test(name);

function openDb() {
  return new Promise((resolve, reject) => {
    const req = indexedDB.open(DB_NAME, 1);
    req.onupgradeneeded = () => req.result.createObjectStore("packs", { keyPath: "name" });
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}

function tx(mode, fn) {
  return openDb().then((db) => new Promise((resolve, reject) => {
    const t = db.transaction("packs", mode);
    const result = fn(t.objectStore("packs"));
    t.oncomplete = () => resolve(result.result ?? result);
    t.onerror = () => reject(t.error);
  }));
}

/**
 * Local packs for the web client: a folder of <sound>.wav files the user
 * picks is copied into this browser's storage (IndexedDB). Nothing is
 * uploaded.
 */
export function browserPacks() {
  const urls = new Map();
  return {
    async list() {
      const packs = await tx("readonly", (store) => store.getAll());
      return packs.map((p) => ({ id: `browser:${p.name}`, label: p.name, sounds: Object.keys(p.files) }));
    },
    async url(packId, soundId) {
      const key = `${packId}/${soundId}`;
      if (urls.has(key)) return urls.get(key);
      const pack = await tx("readonly", (store) => store.get(packId.slice("browser:".length)));
      const blob = pack?.files[soundId];
      if (!blob) throw new Error("not found");
      const url = URL.createObjectURL(blob);
      urls.set(key, url);
      return url;
    },
    renderAdd(el, refresh) {
      const input = el("input", {
        type: "file", multiple: true, webkitdirectory: true, hidden: true,
        onchange: async () => {
          const files = [...input.files].filter((f) => isSoundFile(f.name));
          if (!files.length) {
            status.textContent = "No <sound>.wav files in that folder.";
            return;
          }
          const name = files[0].webkitRelativePath.split("/")[0] || "My sounds";
          const map = {};
          for (const f of files) map[f.name.slice(0, -4)] = f;
          await tx("readwrite", (store) => store.put({ name, files: map }));
          status.textContent = `Added "${name}" with ${files.length} sounds.`;
          input.value = "";
          await refresh();
        },
      });
      const status = el("span", { class: "muted" });
      const remove = el("button", {
        type: "button", class: "small", textContent: "Remove pack",
        title: "Removes the chosen pack from this browser",
        onclick: async () => {
          const id = document.getElementById("sound-pack")?.value || "";
          if (!id.startsWith("browser:")) {
            status.textContent = "Choose a pack from this computer first.";
            return;
          }
          await tx("readwrite", (store) => store.delete(id.slice("browser:".length)));
          status.textContent = "Pack removed.";
          await refresh();
        },
      });
      return el("div", { class: "row" },
        input,
        el("button", { type: "button", class: "small", textContent: "Add pack folder…", onclick: () => input.click() }),
        remove,
        status);
    },
  };
}
