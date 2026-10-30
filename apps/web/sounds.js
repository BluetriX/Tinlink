// Notification sounds, in packs: sounds/<pack>/<sound>.wav. The "retro"
// pack is synthesized by tools/sounds/generate.py (no samples). This file is
// shared verbatim by the web and the desktop client; CI checks both copies
// are the same.

/** Available packs; a pack may leave sounds out, those come from DEFAULT_PACK. */
export const PACKS = [
  { id: "retro", label: "Retro" },
];
const DEFAULT_PACK = "retro";

export const SOUNDS = [
  { id: "connected", label: "You joined a voice channel" },
  { id: "disconnected", label: "You left voice" },
  { id: "user-joined", label: "Someone joined your channel" },
  { id: "user-left", label: "Someone left your channel" },
  { id: "moved", label: "You were moved to another channel" },
  { id: "removed", label: "You were removed from voice" },
  { id: "connection-lost", label: "Voice connection lost" },
  { id: "message", label: "Direct message or mention" },
  { id: "talking-muted", label: "Talking while nobody hears you" },
];

const DEFAULTS = { on: true, volume: 0.5, off: [], pack: DEFAULT_PACK };

/**
 * `load`/`save` read and patch the client's settings object; the sound
 * settings live under its `sounds` key.
 */
export function createSounds({ load, save }) {
  const cache = new Map();
  let sinkId = "";

  const current = () => {
    const s = { ...DEFAULTS, ...(load().sounds || {}) };
    if (!PACKS.some((p) => p.id === s.pack)) s.pack = DEFAULT_PACK;
    return s;
  };
  const update = (patch) => save({ sounds: { ...current(), ...patch } });

  /** The loaded file of a sound; one the pack lacks comes from the default pack. */
  function audio(pack, id) {
    const key = `${pack}/${id}`;
    let base = cache.get(key);
    if (!base) {
      base = new Audio(`sounds/${key}.wav`);
      base.preload = "auto";
      if (pack !== DEFAULT_PACK) {
        base.addEventListener("error", () => cache.set(key, audio(DEFAULT_PACK, id)), { once: true });
      }
      cache.set(key, base);
    }
    return base;
  }

  /** Plays a sound unless sounds or this one are turned off. */
  function play(id, { force = false } = {}) {
    const s = current();
    if (!force && (!s.on || s.off.includes(id))) return;
    if (s.volume <= 0) return;
    // A clone, so the same sound can overlap itself.
    const node = audio(s.pack, id).cloneNode();
    node.volume = Math.min(1, s.volume);
    if (sinkId && node.setSinkId) node.setSinkId(sinkId).catch(() => {});
    node.play().catch(() => {}); // autoplay not yet allowed: stay silent
  }

  /** Output device for sounds (browsers that support choosing one). */
  function setSink(id) {
    sinkId = id || "";
  }

  /** Builds the settings block: on/off, pack, volume and a list with previews. */
  function renderSettings(el) {
    const s = current();
    const pack = el("select", {
      onchange: () => {
        update({ pack: pack.value });
        play("connected", { force: true });
      },
    }, ...PACKS.map((p) => el("option", { value: p.id, textContent: p.label, selected: p.id === s.pack })));
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
    return el("div", { class: "sound-settings" },
      el("label", { class: "inline" }, on, " Sounds"),
      el("label", {}, "Sound pack ", pack),
      el("label", {}, "Sound volume ", pct, volume),
      list);
  }

  return { play, setSink, renderSettings };
}
