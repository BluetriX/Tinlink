// Pure helpers of the web client, kept free of DOM and LiveKit so they can be
// tested with `node --test apps/web/tests/*.test.js`.

/** Permission sets arrive as "VIEW_CHANNEL | SEND_MESSAGES". */
export function can(perms, flag) {
  if (!perms) return false;
  const set = perms.split("|").map((p) => p.trim());
  return set.includes(flag) || set.includes("ADMINISTRATOR");
}

/**
 * Whether a channel has messages newer than the read marker. Message ids are
 * UUIDv7 strings, which sort by creation time.
 */
export function isUnread(ch) {
  return !!ch.last_message_id && (!ch.last_read_id || ch.last_message_id > ch.last_read_id);
}

/**
 * The partial `@name` being typed before the caret, or null. Returns the
 * start index of the `@` and the typed prefix.
 */
export function mentionQuery(text, caret) {
  const before = text.slice(0, caret);
  const match = /(^|[^A-Za-z0-9_.\-])@([A-Za-z0-9_.\-]{0,32})$/.exec(before);
  if (!match) return null;
  return { start: caret - match[2].length - 1, prefix: match[2].toLowerCase() };
}

/** Members whose name or display name starts with the prefix, best first. */
export function mentionCandidates(members, prefix, limit = 8) {
  const p = prefix.toLowerCase();
  return members
    .filter((m) => m.name.startsWith(p) || (m.display_name || "").toLowerCase().startsWith(p))
    .sort((a, b) => a.name.localeCompare(b.name))
    .slice(0, limit);
}

/**
 * Voice activation gate: opens when the level reaches the threshold and
 * stays open for `hangoverMs` after it drops, so word endings are not cut.
 */
export class Gate {
  constructor(thresholdDb = -50, hangoverMs = 500) {
    this.thresholdDb = thresholdDb;
    this.hangoverMs = hangoverMs;
    this.lastLoud = -Infinity;
  }

  /** Feeds one level sample; returns whether the gate is open. */
  update(levelDb, nowMs) {
    if (levelDb >= this.thresholdDb) this.lastLoud = nowMs;
    return nowMs - this.lastLoud <= this.hangoverMs;
  }
}

/**
 * Who is audible right now, measured on the audio itself: a participant
 * counts as speaking while their level reached `audibleDb` within the last
 * `holdMs`. Reacts within one level sample, unlike the server's active
 * speaker updates.
 */
export class SpeakingTracker {
  constructor(audibleDb = -60, holdMs = 250) {
    this.audibleDb = audibleDb;
    this.holdMs = holdMs;
    this.lastLoud = new Map();
    this.speaking = new Set();
  }

  /** Feeds one level sample; returns whether the speaking set changed. */
  update(id, levelDb, nowMs) {
    if (levelDb >= this.audibleDb) this.lastLoud.set(id, nowMs);
    const on = nowMs - (this.lastLoud.get(id) ?? -Infinity) <= this.holdMs;
    if (on === this.speaking.has(id)) return false;
    if (on) this.speaking.add(id);
    else this.speaking.delete(id);
    return true;
  }

  /** Forgets a participant; returns whether the speaking set changed. */
  remove(id) {
    this.lastLoud.delete(id);
    return this.speaking.delete(id);
  }

  clear() {
    this.lastLoud.clear();
    this.speaking.clear();
  }
}

/** Per-person compressor presets for what you hear (DynamicsCompressorNode values). */
export const COMPRESSORS = {
  light: { threshold: -30, knee: 10, ratio: 3, attack: 0.005, release: 0.2 },
  strong: { threshold: -45, knee: 6, ratio: 8, attack: 0.003, release: 0.25 },
};

/**
 * Detects talking while nothing is transmitted: fires after `talkMs` of
 * speech (pauses up to `gapMs` count as speech), then not again for
 * `cooldownMs`.
 */
export class MutedSpeechDetector {
  constructor({ talkMs = 5_000, gapMs = 2_000, cooldownMs = 30_000 } = {}) {
    Object.assign(this, { talkMs, gapMs, cooldownMs });
    this.start = null;
    this.lastSpeech = -Infinity;
    this.lastFired = -Infinity;
  }

  /** Feeds one sample; returns true when the hint should be shown. */
  update(speaking, transmitting, nowMs) {
    if (transmitting) {
      this.start = null;
      return false;
    }
    if (speaking) {
      if (this.start === null || nowMs - this.lastSpeech > this.gapMs) this.start = nowMs;
      this.lastSpeech = nowMs;
    } else if (this.start !== null && nowMs - this.lastSpeech > this.gapMs) {
      this.start = null;
    }
    // Only fire while talking, not in a pause.
    if (speaking && this.start !== null && nowMs - this.start >= this.talkMs && nowMs - this.lastFired >= this.cooldownMs) {
      this.lastFired = nowMs;
      this.start = null;
      return true;
    }
    return false;
  }
}

/** Capture size for a share resolution choice (0 = native). */
export function shareSize(height) {
  const sizes = { 1440: [2560, 1440], 1080: [1920, 1080], 720: [1280, 720] };
  return sizes[height] || null;
}

/** Bitrate in bit/s for a screen share of the given height and frame rate. */
export function shareBitrate(height, fps) {
  const base = height >= 1440 || !height ? 6_000_000 : height >= 1080 ? 4_000_000 : 2_500_000;
  return Math.round(base * (fps > 30 ? 1.5 : fps < 30 ? 0.6 : 1));
}

/** Columns and rows of the webcam grid for `count` tiles: as square as possible, wider than tall. */
export function cameraGrid(count) {
  const cols = Math.max(1, Math.ceil(Math.sqrt(count)));
  return { cols, rows: Math.max(1, Math.ceil(count / cols)) };
}

/** Capture size of a webcam quality choice (by height, 16:9). */
export function cameraSize(height) {
  const sizes = { 720: [1280, 720], 540: [960, 540], 360: [640, 360] };
  return sizes[height] || sizes[720];
}

/** Name of a key or mouse button as used for push-to-talk. */
export function inputName(ev) {
  if (ev.type.startsWith("mouse") || ev.type.startsWith("pointer")) {
    return { 1: "Mouse3", 3: "Mouse4", 4: "Mouse5" }[ev.button] || null;
  }
  return ev.code || null;
}

/** Members sorted for the member list: online first, then by name. */
export function sortMembers(members) {
  const label = (m) => (m.display_name || m.name).toLowerCase();
  return [...members].sort((a, b) => (b.online === true) - (a.online === true) || label(a).localeCompare(label(b)));
}

/**
 * Who is typing where. A notice lasts `ttlMs` unless renewed; a message from
 * that user ends it early.
 */
export class TypingTracker {
  constructor(ttlMs = 5_000) {
    this.ttlMs = ttlMs;
    this.entries = new Map(); // "channel/user" -> { channelId, userId, name, until }
  }

  add(channelId, userId, name, nowMs) {
    this.entries.set(`${channelId}/${userId}`, { channelId, userId, name, until: nowMs + this.ttlMs });
  }

  remove(channelId, userId) {
    this.entries.delete(`${channelId}/${userId}`);
  }

  /** Names typing in a channel right now; drops expired entries. */
  names(channelId, nowMs) {
    const names = [];
    for (const [key, e] of this.entries) {
      if (e.until <= nowMs) this.entries.delete(key);
      else if (e.channelId === channelId) names.push(e.name);
    }
    return names.sort();
  }
}

/** "alice is typing…", "alice and bob are typing…", "Several people are typing…". */
export function typingText(names) {
  if (!names.length) return "";
  if (names.length === 1) return `${names[0]} is typing…`;
  if (names.length <= 3) return `${names.slice(0, -1).join(", ")} and ${names.at(-1)} are typing…`;
  return "Several people are typing…";
}

/** Search words as the server splits them: runs of letters and digits. */
export function searchTerms(query) {
  return query.toLowerCase().split(/[^\p{L}\p{N}]+/u).filter(Boolean);
}

/**
 * Ranges [start, end) in `text` where a word starts with one of the terms,
 * sorted and without overlaps, for highlighting search hits.
 */
export function highlightRanges(text, terms) {
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
    const last = merged.at(-1);
    if (last && r[0] <= last[1]) last[1] = Math.max(last[1], r[1]);
    else merged.push([...r]);
  }
  return merged;
}

/** Group-wide permissions shown in the role editor, in display order. */
export const PERMISSIONS = [
  ["VIEW_CHANNEL", "View channels"],
  ["SEND_MESSAGES", "Send messages"],
  ["ATTACH_FILES", "Attach files"],
  ["CONNECT", "Join voice"],
  ["SPEAK", "Speak"],
  ["SHARE_SCREEN", "Share screen and camera"],
  ["CREATE_OWN_CHANNELS", "Create own channels"],
  ["MANAGE_CHANNELS", "Manage channels"],
  ["MANAGE_MESSAGES", "Delete others' messages"],
  ["MUTE_MEMBERS", "Mute members"],
  ["MOVE_MEMBERS", "Move and disconnect members"],
  ["KICK_MEMBERS", "Kick members"],
  ["MANAGE_ROLES", "Manage roles"],
  ["MANAGE_GROUP", "Manage group and invites"],
  ["ADMINISTRATOR", "Administrator (everything)"],
];

/** Permissions that make sense per channel, for the overwrite editor. */
export const CHANNEL_PERMISSIONS = PERMISSIONS.filter(([flag]) => [
  "VIEW_CHANNEL", "SEND_MESSAGES", "ATTACH_FILES", "CONNECT", "SPEAK", "SHARE_SCREEN",
  "MANAGE_CHANNELS", "MANAGE_MESSAGES", "MUTE_MEMBERS", "MOVE_MEMBERS",
].includes(flag));

/** "A | B" → Set of flags. */
export function parsePerms(perms) {
  return new Set((perms || "").split("|").map((p) => p.trim()).filter(Boolean));
}

/** Set of flags → "A | B" in display order; unknown flags are kept at the end. */
export function formatPerms(flags) {
  const known = PERMISSIONS.map(([f]) => f).filter((f) => flags.has(f));
  const unknown = [...flags].filter((f) => !PERMISSIONS.some(([k]) => k === f));
  return [...known, ...unknown].join(" | ");
}

/** "allow", "deny" or "inherit" for one flag of an overwrite. */
export function overwriteState(ow, flag) {
  if (parsePerms(ow.allow).has(flag)) return "allow";
  if (parsePerms(ow.deny).has(flag)) return "deny";
  return "inherit";
}

/** The overwrite with one flag set to "allow", "deny" or "inherit". */
export function setOverwriteState(ow, flag, value) {
  const allow = parsePerms(ow.allow);
  const deny = parsePerms(ow.deny);
  allow.delete(flag);
  deny.delete(flag);
  if (value === "allow") allow.add(flag);
  if (value === "deny") deny.add(flag);
  return { ...ow, allow: formatPerms(allow), deny: formatPerms(deny) };
}

/** Whether a role can be edited by someone whose highest role is at `myPosition`. */
export function canEditRole(role, myPosition) {
  return role.everyone || role.position < myPosition;
}

/** Highest role position of a member (0 = only @everyone). */
export function memberPosition(member, roles) {
  return Math.max(0, ...roles.filter((r) => (member.role_ids || []).includes(r.id)).map((r) => r.position));
}

/** CSS color for a role color stored as 0xRRGGBB, or null. */
export function roleColor(color) {
  return color == null ? null : `#${color.toString(16).padStart(6, "0")}`;
}

/**
 * Lower-cased user names mentioned with `@name`, like the server finds them
 * in plain messages; for encrypted ones the client names the users.
 */
export function mentionedNames(text) {
  const nameChar = /[A-Za-z0-9_.-]/;
  const names = [];
  for (let i = 0; i < text.length; i++) {
    if (text[i] !== "@" || (i > 0 && nameChar.test(text[i - 1]))) continue;
    let end = i + 1;
    while (end < text.length && nameChar.test(text[end])) end++;
    const name = text.slice(i + 1, end).replace(/[.-]+$/, "").toLowerCase();
    if (name.length >= 1 && name.length <= 32 && !names.includes(name)) names.push(name);
  }
  return names;
}
