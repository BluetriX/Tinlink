// This browser's E2EE keys and the channel keys it can read (docs/e2ee.md).
// The server only relays; everything it returns is checked here before use.
import * as e2ee from "./e2ee.js";

const { fromBase64: b64d, toBase64: b64e } = e2ee;

/** Tiny IndexedDB key-value store; falls back to memory when blocked. */
function openStore(name) {
  const memory = new Map();
  const db = new Promise((resolve) => {
    try {
      const req = indexedDB.open(name, 1);
      req.onupgradeneeded = () => req.result.createObjectStore("kv");
      req.onsuccess = () => resolve(req.result);
      req.onerror = () => resolve(null);
    } catch {
      resolve(null);
    }
  });
  const tx = async (mode, fn) => {
    const d = await db;
    if (!d) return fn(null);
    return new Promise((resolve, reject) => {
      const t = d.transaction("kv", mode);
      const req = fn(t.objectStore("kv"));
      t.oncomplete = () => resolve(req?.result);
      t.onerror = () => reject(t.error);
    });
  };
  return {
    get: (key) => tx("readonly", (s) => (s ? s.get(key) : { result: memory.get(key) })),
    set: (key, value) => tx("readwrite", (s) => (s ? s.put(value, key) : memory.set(key, value))),
  };
}

/** Short code both devices show while one approves the other. */
export async function deviceCode(signKey, dhKey) {
  const hash = new Uint8Array(await crypto.subtle.digest("SHA-256", e2ee.concat(signKey, dhKey)));
  const digits = Array.from(hash.subarray(0, 6), (b) => String(b % 100).padStart(2, "0")).join("");
  return digits.match(/.{4}/g).join(" ");
}

/**
 * api(method, path, body) talks to the server; onWarning(text) reports
 * keys that changed or did not verify.
 */
export function createKeyring({ api, userId, onWarning = () => {} }) {
  const store = openStore("tinlink-e2ee");
  const recordKey = `user:${userId}`;
  // { deviceId, signSeed, dhPrivate, identitySeed? } as base64
  let record = null;
  let device = null; // { id, signSeed, dhPrivate, signPublic, dhPublic }
  let identitySeed = null;
  let approved = false;
  const channelKeys = new Map(); // channel id -> Map(epoch -> key bytes)
  const pinned = new Map(); // user id -> identity key (base64), remembered first sight

  async function save() {
    record.identitySeed = identitySeed ? b64e(identitySeed) : null;
    record.pinned = Object.fromEntries(pinned);
    await store.set(recordKey, record);
  }

  async function certify(id, kind, signKey, dhKey) {
    return e2ee.sign(identitySeed, e2ee.certificatePayload(userId, id, kind, signKey, dhKey));
  }

  /** Checks a user's keys from the server; returns their approved devices. */
  async function verifiedDevices(user) {
    if (!user.identity_key) return [];
    const pin = pinned.get(user.user_id);
    if (pin && pin !== user.identity_key) {
      onWarning(user.user_id);
      return [];
    }
    if (!pin) {
      pinned.set(user.user_id, user.identity_key);
      await save();
    }
    const identity = b64d(user.identity_key);
    const out = [];
    for (const d of user.devices) {
      if (!d.certificate) continue;
      const signKey = d.sign_key ? b64d(d.sign_key) : null;
      const dhKey = b64d(d.dh_key);
      const payload = e2ee.certificatePayload(user.user_id, d.id, d.kind, signKey, dhKey);
      if (await e2ee.verify(identity, payload, b64d(d.certificate))) {
        out.push({ id: d.id, userId: user.user_id, kind: d.kind, signKey, dhKey });
      }
    }
    return out;
  }

  /**
   * Loads or creates this browser's device. Returns "ready", "new_identity"
   * (first device of the account: call createIdentity) or "pending" (needs
   * the recovery key or another device's approval).
   */
  async function setup(deviceName) {
    record = (await store.get(recordKey)) || null;
    if (!record) {
      const signSeed = e2ee.randomKey();
      const dhPrivate = e2ee.randomKey();
      record = { deviceId: crypto.randomUUID(), signSeed: b64e(signSeed), dhPrivate: b64e(dhPrivate), registered: false };
      await store.set(recordKey, record);
    }
    device = {
      id: record.deviceId,
      signSeed: b64d(record.signSeed),
      dhPrivate: b64d(record.dhPrivate),
    };
    device.signPublic = await e2ee.signingPublic(device.signSeed);
    device.dhPublic = await e2ee.dhPublic(device.dhPrivate);
    identitySeed = record.identitySeed ? b64d(record.identitySeed) : null;
    for (const [id, key] of Object.entries(record.pinned || {})) pinned.set(id, key);

    if (!record.registered) {
      await api("POST", "/devices", {
        id: device.id, kind: "device", name: deviceName,
        sign_key: b64e(device.signPublic), dh_key: b64e(device.dhPublic),
      }).catch((e) => {
        if (e.status !== 409) throw e;
      });
      record.registered = true;
      await save();
    }
    return refresh();
  }

  /** Re-reads whether this device is approved. */
  async function refresh() {
    const me = await api("GET", `/users/${userId}/keys`);
    const mine = me.devices.find((d) => d.id === device.id);
    if (!mine) {
      // Revoked, e.g. by an identity reset elsewhere: start over.
      await store.set(recordKey, null);
      approved = false;
      return "revoked";
    }
    if (!me.identity_key) return "new_identity";
    pinned.set(userId, me.identity_key);
    approved = (await verifiedDevices(me)).some((d) => d.id === device.id);
    return approved ? "ready" : "pending";
  }

  /** Creates the account's identity and recovery key; returns the recovery key text. */
  async function createIdentity() {
    identitySeed = e2ee.randomKey();
    const recovery = e2ee.randomKey();
    const identityKey = await e2ee.signingPublic(identitySeed);
    await api("PUT", "/users/me/identity", {
      identity_key: b64e(identityKey),
      identity_backup: b64e(await e2ee.sealIdentityBackup(recovery, userId, identitySeed)),
    });
    pinned.set(userId, b64e(identityKey));
    const cert = await certify(device.id, "device", device.signPublic, device.dhPublic);
    await api("PUT", `/devices/${device.id}/certificate`, { certificate: b64e(cert) });
    await addBackupDevice(recovery);
    await save();
    approved = true;
    return e2ee.encodeRecoveryKey(recovery);
  }

  async function addBackupDevice(recovery) {
    const id = crypto.randomUUID();
    const dhKey = await e2ee.dhPublic(await e2ee.backupDhPrivate(recovery));
    await api("POST", "/devices", {
      id, kind: "backup", name: "Recovery key", dh_key: b64e(dhKey),
      certificate: b64e(await certify(id, "backup", null, dhKey)),
    });
  }

  /** Approves this device with the recovery key; throws for a wrong key. */
  async function useRecoveryKey(text) {
    const recovery = e2ee.decodeRecoveryKey(text);
    const backup = await api("GET", "/users/me/identity-backup");
    try {
      identitySeed = await e2ee.openIdentityBackup(recovery, userId, b64d(backup.identity_backup));
    } catch {
      throw new Error("This recovery key does not fit this account.");
    }
    const cert = await certify(device.id, "device", device.signPublic, device.dhPublic);
    await api("PUT", `/devices/${device.id}/certificate`, { certificate: b64e(cert) });
    // Channel keys wrapped to the backup become ours too.
    const backupDevice = (await api("GET", `/users/${userId}/keys`)).devices.find((d) => d.kind === "backup");
    record.backupDhPrivate = backupDevice ? b64e(await e2ee.backupDhPrivate(recovery)) : null;
    record.backupId = backupDevice?.id || null;
    await save();
    approved = true;
  }

  /** Own devices waiting for approval, with their codes; only if this one can approve. */
  async function pendingDevices() {
    if (!identitySeed) return [];
    const me = await api("GET", `/users/${userId}/keys`);
    const out = [];
    for (const d of me.devices) {
      if (d.certificate || d.kind !== "device") continue;
      out.push({ id: d.id, name: d.name, code: await deviceCode(b64d(d.sign_key), b64d(d.dh_key)), raw: d });
    }
    return out;
  }

  async function approveDevice(pending) {
    const cert = await certify(pending.id, "device", b64d(pending.raw.sign_key), b64d(pending.raw.dh_key));
    await api("PUT", `/devices/${pending.id}/certificate`, { certificate: b64e(cert) });
  }

  /** Opens wrapped keys of one device id with its private key. */
  async function openKeys(channelId, deviceId, dhPrivate, keys, senders) {
    const out = new Map();
    for (const k of keys) {
      const sender = senders.get(k.sender_id);
      const wrapped = b64d(k.wrapped);
      const payload = e2ee.wrappedKeyPayload(channelId, k.epoch, deviceId, wrapped);
      if (!sender?.signKey || !(await e2ee.verify(sender.signKey, payload, b64d(k.signature)))) continue;
      try {
        out.set(k.epoch, await e2ee.unwrapChannelKey(dhPrivate, deviceId, channelId, k.epoch, wrapped));
      } catch {
        // Not for us after all; skip it.
      }
    }
    return out;
  }

  /** Approved devices by id of everyone who can read the channel. */
  async function readerDevices(channelId) {
    const users = await api("GET", `/channels/${channelId}/keys/devices`);
    const byId = new Map();
    for (const u of users) for (const d of await verifiedDevices(u)) byId.set(d.id, d);
    return byId;
  }

  /** Fetches the keys of a channel; returns { epoch, rotate }. */
  async function load(channelId) {
    const res = await api("GET", `/channels/${channelId}/keys?device=${device.id}`);
    const senders = await readerDevices(channelId);
    // Senders that lost access later still signed valid keys back then.
    for (const k of res.keys) {
      if (!senders.has(k.sender_id)) {
        const u = await api("GET", `/users/${k.sender_user_id}/keys`).catch(() => null);
        for (const d of u ? await verifiedDevices(u) : []) senders.set(d.id, d);
      }
    }
    const keys = await openKeys(channelId, device.id, device.dhPrivate, res.keys, senders);
    channelKeys.set(channelId, new Map([...(channelKeys.get(channelId) || []), ...keys]));
    return { epoch: res.epoch, rotate: res.rotate, readers: senders };
  }

  /** Wraps a key to devices and stores it as `epoch`. */
  async function send(channelId, epoch, key, devices) {
    const keys = [];
    for (const d of devices) {
      const wrapped = await e2ee.wrapChannelKey(d.dhKey, d.id, channelId, epoch, key);
      const signature = await e2ee.sign(device.signSeed, e2ee.wrappedKeyPayload(channelId, epoch, d.id, wrapped));
      keys.push({ device_id: d.id, wrapped: b64e(wrapped), signature: b64e(signature) });
    }
    for (let i = 0; i < keys.length; i += 500) {
      await api("POST", `/channels/${channelId}/keys`, { device_id: device.id, epoch, keys: keys.slice(i, i + 500) });
    }
  }

  /**
   * The newest key of a channel, as { epoch, key }; starts a new epoch when
   * there is none or someone lost access.
   */
  async function current(channelId) {
    if (!approved) throw new Error("This device is not approved for encryption yet.");
    for (let attempt = 0; attempt < 3; attempt++) {
      const state = await load(channelId);
      const known = channelKeys.get(channelId);
      if (state.epoch && !state.rotate && known.has(state.epoch)) {
        return { epoch: state.epoch, key: known.get(state.epoch) };
      }
      if (state.epoch && !state.rotate) {
        // Someone else's newest key has not reached us yet.
        throw new Error("Waiting for the channel key from another member.");
      }
      const epoch = (state.epoch || 0) + 1;
      const key = e2ee.randomKey();
      try {
        await send(channelId, epoch, key, [...state.readers.values()]);
        known.set(epoch, key);
        return { epoch, key };
      } catch (e) {
        // Someone else started this epoch first: take theirs.
        if (e.status !== 409) throw e;
      }
    }
    throw new Error("Cannot agree on a channel key.");
  }

  /** All keys of a channel known here, newest last. */
  function known(channelId) {
    return [...(channelKeys.get(channelId) || new Map()).entries()].sort((a, b) => a[0] - b[0]);
  }

  /** Shares keys this device holds with devices of the channel that lack them. */
  async function shareMissing(channelId) {
    if (!approved) return;
    // Keys wrapped to the backup are ours after entering the recovery key.
    if (record.backupDhPrivate) await adoptBackupKeys(channelId);
    const missing = await api("GET", `/channels/${channelId}/keys/missing?device=${device.id}`);
    if (!missing.length) return;
    if (!channelKeys.has(channelId)) await load(channelId);
    const devices = await readerDevices(channelId);
    const keys = channelKeys.get(channelId);
    const byEpoch = new Map();
    for (const m of missing) {
      const d = devices.get(m.device_id);
      if (!d) continue;
      for (const epoch of m.epochs) {
        if (!keys.has(epoch)) continue;
        if (!byEpoch.has(epoch)) byEpoch.set(epoch, []);
        byEpoch.get(epoch).push(d);
      }
    }
    for (const [epoch, list] of byEpoch) await send(channelId, epoch, keys.get(epoch), list);
  }

  async function adoptBackupKeys(channelId) {
    const res = await api("GET", `/channels/${channelId}/keys?device=${record.backupId}`).catch(() => null);
    if (!res?.keys.length) return;
    const senders = await readerDevices(channelId);
    const keys = await openKeys(channelId, record.backupId, b64d(record.backupDhPrivate), res.keys, senders);
    const mine = channelKeys.get(channelId) || new Map();
    const fresh = [...keys].filter(([epoch]) => !mine.has(epoch));
    channelKeys.set(channelId, new Map([...mine, ...keys]));
    for (const [epoch, key] of fresh) await send(channelId, epoch, key, [{ ...device, dhKey: device.dhPublic }]).catch(() => {});
  }

  return {
    setup, refresh, createIdentity, useRecoveryKey, pendingDevices, approveDevice,
    current, known, load, shareMissing,
    get approved() {
      return approved;
    },
    get canApprove() {
      return !!identitySeed;
    },
    get deviceId() {
      return device?.id;
    },
    async code() {
      return deviceCode(device.signPublic, device.dhPublic);
    },
  };
}
