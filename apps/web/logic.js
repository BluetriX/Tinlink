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
