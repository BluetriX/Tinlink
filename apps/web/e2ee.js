// End-to-end encryption formats (docs/e2ee.md), with WebCrypto only.
// The desktop client uses crates/e2ee; crates/e2ee/tests/vectors.json keeps
// both byte-compatible (tests/e2ee.test.js).

const subtle = globalThis.crypto.subtle;
const enc = new TextEncoder();

export const KEY_LEN = 32;
export const WRAPPED_LEN = 32 + KEY_LEN + 16;

export function concat(...parts) {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let at = 0;
  for (const p of parts) {
    out.set(p, at);
    at += p.length;
  }
  return out;
}

const text = (s) => enc.encode(s);

function u32(n) {
  const out = new Uint8Array(4);
  new DataView(out.buffer).setUint32(0, n);
  return out;
}

function u16(n) {
  return new Uint8Array([n >> 8, n & 255]);
}

export function uuidBytes(id) {
  const hex = id.replace(/-/g, "");
  if (!/^[0-9a-f]{32}$/i.test(hex)) throw new Error("invalid uuid");
  return fromHex(hex);
}

export function toHex(bytes) {
  return Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
}

export function fromHex(hex) {
  return new Uint8Array(hex.match(/../g)?.map((h) => parseInt(h, 16)) || []);
}

export function toBase64(bytes) {
  let s = "";
  for (const b of bytes) s += String.fromCharCode(b);
  return btoa(s);
}

export function fromBase64(b64) {
  return Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
}

function equal(a, b) {
  if (a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i++) diff |= a[i] ^ b[i];
  return diff === 0;
}

export function randomKey() {
  return crypto.getRandomValues(new Uint8Array(KEY_LEN));
}

// --- HKDF-SHA256 (RFC 5869) from HMAC, so extract and expand can be used
// separately as HPKE needs.

async function hmac(key, data) {
  // An empty key is the same as 32 zero bytes for HMAC; WebCrypto refuses
  // empty keys.
  const k = await subtle.importKey("raw", key.length ? key : new Uint8Array(32), { name: "HMAC", hash: "SHA-256" }, false, ["sign"]);
  return new Uint8Array(await subtle.sign("HMAC", k, data));
}

const extract = (salt, ikm) => hmac(salt, ikm);

async function expand(prk, info, length) {
  const out = new Uint8Array(length);
  let t = new Uint8Array(0);
  for (let i = 1, at = 0; at < length; i++) {
    t = await hmac(prk, concat(t, info, new Uint8Array([i])));
    out.set(t.subarray(0, Math.min(t.length, length - at)), at);
    at += t.length;
  }
  return out;
}

export async function hkdf(ikm, salt, info, length = KEY_LEN) {
  return expand(await extract(salt, ikm), info, length);
}

// --- Ed25519 and X25519 keys as raw 32 byte values.

const PKCS8 = {
  Ed25519: fromHex("302e020100300506032b657004220420"),
  X25519: fromHex("302e020100300506032b656e04220420"),
};

function importPrivate(alg, raw, usages) {
  return subtle.importKey("pkcs8", concat(PKCS8[alg], raw), { name: alg }, true, usages);
}

async function publicOf(alg, raw, usages) {
  const jwk = await subtle.exportKey("jwk", await importPrivate(alg, raw, usages));
  return fromBase64(jwk.x.replace(/-/g, "+").replace(/_/g, "/"));
}

/** Public Ed25519 key of a private key (its 32 byte seed). */
export const signingPublic = (seed) => publicOf("Ed25519", seed, ["sign"]);

/** Public X25519 key of a private key. */
export const dhPublic = (priv) => publicOf("X25519", priv, ["deriveBits"]);

export async function sign(seed, message) {
  const key = await importPrivate("Ed25519", seed, ["sign"]);
  return new Uint8Array(await subtle.sign("Ed25519", key, message));
}

export async function verify(publicKey, message, signature) {
  const key = await subtle.importKey("raw", publicKey, { name: "Ed25519" }, false, ["verify"]);
  return subtle.verify("Ed25519", key, signature, message);
}

async function dh(priv, pub) {
  const sk = await importPrivate("X25519", priv, ["deriveBits"]);
  const pk = await subtle.importKey("raw", pub, { name: "X25519" }, false, []);
  return new Uint8Array(await subtle.deriveBits({ name: "X25519", public: pk }, sk, 256));
}

// --- HPKE (RFC 9180), base mode, DHKEM(X25519, HKDF-SHA256), HKDF-SHA256,
// AES-256-GCM, single shot.

const KEM_SUITE = concat(text("KEM"), u16(0x0020));
const HPKE_SUITE = concat(text("HPKE"), u16(0x0020), u16(0x0001), u16(0x0002));

const labeledExtract = (suite, salt, label, ikm) =>
  extract(salt, concat(text("HPKE-v1"), suite, text(label), ikm));
const labeledExpand = (suite, prk, label, info, length) =>
  expand(prk, concat(u16(length), text("HPKE-v1"), suite, text(label), info), length);

async function kemSharedSecret(dhResult, encapped, recipientPublic) {
  const prk = await labeledExtract(KEM_SUITE, new Uint8Array(0), "eae_prk", dhResult);
  return labeledExpand(KEM_SUITE, prk, "shared_secret", concat(encapped, recipientPublic), 32);
}

async function keySchedule(sharedSecret, info) {
  const none = new Uint8Array(0);
  const context = concat(
    new Uint8Array([0]),
    await labeledExtract(HPKE_SUITE, none, "psk_id_hash", none),
    await labeledExtract(HPKE_SUITE, none, "info_hash", info),
  );
  const secret = await labeledExtract(HPKE_SUITE, sharedSecret, "secret", none);
  const key = await labeledExpand(HPKE_SUITE, secret, "key", context, 32);
  const nonce = await labeledExpand(HPKE_SUITE, secret, "base_nonce", context, 12);
  return { key: await subtle.importKey("raw", key, "AES-GCM", false, ["encrypt", "decrypt"]), nonce };
}

async function hpkeSeal(recipientPublic, info, aad, plaintext, ephemeral = randomKey()) {
  const encapped = await dhPublic(ephemeral);
  const shared = await kemSharedSecret(await dh(ephemeral, recipientPublic), encapped, recipientPublic);
  const { key, nonce } = await keySchedule(shared, info);
  const ct = await subtle.encrypt({ name: "AES-GCM", iv: nonce, additionalData: aad }, key, plaintext);
  return concat(encapped, new Uint8Array(ct));
}

async function hpkeOpen(recipientPrivate, info, aad, sealed) {
  const encapped = sealed.subarray(0, 32);
  const recipientPublic = await dhPublic(recipientPrivate);
  const shared = await kemSharedSecret(await dh(recipientPrivate, encapped), encapped, recipientPublic);
  const { key, nonce } = await keySchedule(shared, info);
  const pt = await subtle.decrypt({ name: "AES-GCM", iv: nonce, additionalData: aad }, key, sealed.subarray(32));
  return new Uint8Array(pt);
}

// --- Tinlink formats.

/** The bytes an identity key signs to certify a device (signKey null for the backup). */
export function certificatePayload(userId, deviceId, kind, signKey, dhKey) {
  return concat(
    text("tinlink-device-v1"), uuidBytes(userId), uuidBytes(deviceId),
    new Uint8Array([kind === "backup" ? 1 : 0]), signKey || new Uint8Array(0), dhKey,
  );
}

const channelKeyInfo = (channelId, epoch) =>
  concat(text("tinlink-channel-key-v1"), uuidBytes(channelId), u32(epoch));

/** Encrypts a channel key to one device. */
export function wrapChannelKey(recipientDhKey, recipientId, channelId, epoch, channelKey) {
  return hpkeSeal(recipientDhKey, channelKeyInfo(channelId, epoch), uuidBytes(recipientId), channelKey);
}

/** Decrypts a channel key wrapped to this device; throws when it does not open. */
export async function unwrapChannelKey(dhPrivate, deviceId, channelId, epoch, wrapped) {
  if (wrapped.length !== WRAPPED_LEN) throw new Error("wrapped key has a wrong size");
  return hpkeOpen(dhPrivate, channelKeyInfo(channelId, epoch), uuidBytes(deviceId), wrapped);
}

/** The bytes the sending device signs for a wrapped channel key. */
export function wrappedKeyPayload(channelId, epoch, recipientId, wrapped) {
  return concat(channelKeyInfo(channelId, epoch), uuidBytes(recipientId), wrapped);
}

export const messageKey = (channelKey, channelId) =>
  hkdf(channelKey, uuidBytes(channelId), text("tinlink-message-v1"));

export const mediaKey = (channelKey, channelId) =>
  hkdf(channelKey, uuidBytes(channelId), text("tinlink-media-v1"));

/** LiveKit key index of an epoch; the SDKs keep 16 keys. */
export const mediaKeyIndex = (epoch) => epoch % 16;

/** Private X25519 key of the backup device of a recovery key. */
export const backupDhPrivate = (recovery) => hkdf(recovery, new Uint8Array(0), text("tinlink-backup-dh-v1"));

async function backupCipher(recovery) {
  const key = await hkdf(recovery, new Uint8Array(0), text("tinlink-identity-backup-v1"));
  return subtle.importKey("raw", key, "AES-GCM", false, ["encrypt", "decrypt"]);
}

const backupAad = (userId) => concat(text("tinlink-identity-backup-v1"), uuidBytes(userId));

/** Encrypts the identity key's seed with the recovery key. */
export async function sealIdentityBackup(recovery, userId, identitySeed) {
  const nonce = crypto.getRandomValues(new Uint8Array(12));
  const ct = await subtle.encrypt({ name: "AES-GCM", iv: nonce, additionalData: backupAad(userId) }, await backupCipher(recovery), identitySeed);
  return concat(nonce, new Uint8Array(ct));
}

/** Decrypts the identity key's seed; throws for a wrong recovery key. */
export async function openIdentityBackup(recovery, userId, backup) {
  const pt = await subtle.decrypt(
    { name: "AES-GCM", iv: backup.subarray(0, 12), additionalData: backupAad(userId) },
    await backupCipher(recovery), backup.subarray(12),
  );
  return new Uint8Array(pt);
}

const CROCKFORD = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/** The recovery key as shown to the user: Crockford base32 in groups of four. */
export function encodeRecoveryKey(key) {
  let out = "";
  let acc = 0;
  let bits = 0;
  for (const b of key) {
    acc = ((acc << 8) | b) & 0xfff;
    bits += 8;
    while (bits >= 5) {
      bits -= 5;
      out += CROCKFORD[(acc >> bits) & 31];
    }
  }
  if (bits > 0) out += CROCKFORD[(acc << (5 - bits)) & 31];
  return out.match(/.{1,4}/g).join(" ");
}

/** Parses a recovery key typed by the user, ignoring case, spaces and dashes. */
export function decodeRecoveryKey(input) {
  const out = [];
  let acc = 0;
  let bits = 0;
  for (let c of input.replace(/[\s-]/g, "").toUpperCase()) {
    c = c === "O" ? "0" : c === "I" || c === "L" ? "1" : c;
    const v = CROCKFORD.indexOf(c);
    if (v < 0) throw new Error("invalid recovery key");
    acc = ((acc << 5) | v) & 0xfff;
    bits += 5;
    if (bits >= 8) {
      bits -= 8;
      out.push((acc >> bits) & 255);
    }
  }
  if (out.length !== KEY_LEN || (acc & ((1 << bits) - 1)) !== 0) throw new Error("invalid recovery key");
  return new Uint8Array(out);
}

// --- Messages.

const MESSAGE_LABEL = "tinlink-message-v1";

export function uuidString(bytes) {
  const h = toHex(bytes);
  return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20)}`;
}

/**
 * Encrypts and signs a message (UTF-8 bytes) with a channel key epoch; returns
 * the base64 envelope stored as the message content.
 */
export async function sealMessage({ channelId, epoch, deviceId, signSeed, channelKey, plaintext }) {
  const key = await subtle.importKey("raw", await messageKey(channelKey, channelId), "AES-GCM", false, ["encrypt"]);
  const nonce = crypto.getRandomValues(new Uint8Array(12));
  const header = concat(new Uint8Array([1]), u32(epoch), uuidBytes(deviceId));
  const aad = concat(text(MESSAGE_LABEL), uuidBytes(channelId), header.subarray(1));
  const ct = new Uint8Array(await subtle.encrypt({ name: "AES-GCM", iv: nonce, additionalData: aad }, key, plaintext));
  const body = concat(header, nonce, ct);
  const signature = await sign(signSeed, concat(text(MESSAGE_LABEL), uuidBytes(channelId), body));
  return toBase64(concat(body, signature));
}

/** Splits an envelope; throws when it is malformed. */
export function parseEnvelope(content) {
  const raw = fromBase64(content);
  if (raw.length < 1 + 4 + 16 + 12 + 16 + 64 || raw[0] !== 1) throw new Error("unknown message format");
  const body = raw.subarray(0, raw.length - 64);
  return {
    epoch: new DataView(raw.buffer, raw.byteOffset + 1, 4).getUint32(0),
    deviceId: uuidString(raw.subarray(5, 21)),
    body,
    signature: raw.subarray(raw.length - 64),
  };
}

/** Checks the sender's signature and decrypts; throws if either fails. */
export async function openMessage({ channelId, envelope, channelKey, signKey }) {
  const { body, signature } = envelope;
  if (!(await verify(signKey, concat(text(MESSAGE_LABEL), uuidBytes(channelId), body), signature))) {
    throw new Error("invalid message signature");
  }
  const key = await subtle.importKey("raw", await messageKey(channelKey, channelId), "AES-GCM", false, ["decrypt"]);
  const aad = concat(text(MESSAGE_LABEL), uuidBytes(channelId), body.subarray(1, 21));
  const pt = await subtle.decrypt({ name: "AES-GCM", iv: body.subarray(21, 33), additionalData: aad }, key, body.subarray(33));
  return new Uint8Array(pt);
}

// ------------------------------------------------------------------ files

const FILE_LABEL = "tinlink-file-v1";

/**
 * Encrypts a file with a fresh key; the key travels inside the encrypted
 * message that carries the file.
 */
export async function sealFile(data) {
  const fileKey = randomKey();
  const key = await subtle.importKey("raw", fileKey, "AES-GCM", false, ["encrypt"]);
  const nonce = crypto.getRandomValues(new Uint8Array(12));
  const ct = new Uint8Array(await subtle.encrypt({ name: "AES-GCM", iv: nonce, additionalData: text(FILE_LABEL) }, key, data));
  return { key: toBase64(fileKey), data: concat(nonce, ct) };
}

/** Decrypts a file sealed with `sealFile`; throws when it was changed. */
export async function openFile(fileKey, data) {
  if (data.length < 12 + 16) throw new Error("file too short");
  const key = await subtle.importKey("raw", fromBase64(fileKey), "AES-GCM", false, ["decrypt"]);
  const pt = await subtle.decrypt({ name: "AES-GCM", iv: data.subarray(0, 12), additionalData: text(FILE_LABEL) }, key, data.subarray(12));
  return new Uint8Array(pt);
}

export { equal as bytesEqual };
