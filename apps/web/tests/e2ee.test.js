// Checks the web client's E2EE code against the vectors of crates/e2ee.
// WRITE_VECTORS=1 stores a key wrapped here, which the Rust tests open.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, writeFileSync } from "node:fs";
import * as e2ee from "../e2ee.js";

const path = new URL("../../../crates/e2ee/tests/vectors.json", import.meta.url);
const v = JSON.parse(readFileSync(path, "utf8"));
const hex = e2ee.fromHex;
const same = (bytes, expected) => assert.equal(e2ee.toHex(bytes), expected);

test("keys and certificates", async () => {
  same(await e2ee.signingPublic(hex(v.identity_seed)), v.identity_public);
  same(await e2ee.signingPublic(hex(v.device_sign_seed)), v.device_sign_public);
  same(await e2ee.dhPublic(hex(v.device_dh_private)), v.device_dh_public);
  const payload = e2ee.certificatePayload(v.user_id, v.device_id, "device", hex(v.device_sign_public), hex(v.device_dh_public));
  same(payload, v.certificate_payload);
  same(await e2ee.sign(hex(v.identity_seed), payload), v.certificate);
  assert.ok(await e2ee.verify(hex(v.identity_public), payload, hex(v.certificate)));
  assert.ok(!(await e2ee.verify(hex(v.identity_public), payload.subarray(1), hex(v.certificate))));
});

test("recovery key and identity backup", async () => {
  const recovery = hex(v.recovery_key);
  assert.equal(e2ee.encodeRecoveryKey(recovery), v.recovery_key_text);
  same(e2ee.decodeRecoveryKey(v.recovery_key_text), v.recovery_key);
  same(e2ee.decodeRecoveryKey(v.recovery_key_text.toLowerCase().replace(/ /g, "-")), v.recovery_key);
  assert.throws(() => e2ee.decodeRecoveryKey(v.recovery_key_text.slice(0, -1)));
  const backupPrivate = await e2ee.backupDhPrivate(recovery);
  same(backupPrivate, v.backup_dh_private);
  same(await e2ee.dhPublic(backupPrivate), v.backup_dh_public);
  same(e2ee.certificatePayload(v.user_id, v.backup_id, "backup", null, hex(v.backup_dh_public)), v.backup_certificate_payload);
  same(await e2ee.openIdentityBackup(recovery, v.user_id, hex(v.identity_backup)), v.identity_seed);
  await assert.rejects(e2ee.openIdentityBackup(new Uint8Array(32), v.user_id, hex(v.identity_backup)));
  const sealed = await e2ee.sealIdentityBackup(recovery, v.user_id, hex(v.identity_seed));
  same(await e2ee.openIdentityBackup(recovery, v.user_id, sealed), v.identity_seed);
});

test("channel keys", async () => {
  const priv = hex(v.device_dh_private);
  same(await e2ee.unwrapChannelKey(priv, v.device_id, v.channel_id, v.epoch, hex(v.wrapped)), v.channel_key);
  await assert.rejects(e2ee.unwrapChannelKey(priv, v.device_id, v.channel_id, v.epoch + 1, hex(v.wrapped)));
  await assert.rejects(e2ee.unwrapChannelKey(priv, v.user_id, v.channel_id, v.epoch, hex(v.wrapped)));
  const payload = e2ee.wrappedKeyPayload(v.channel_id, v.epoch, v.device_id, hex(v.wrapped));
  same(payload, v.wrapped_payload);
  same(await e2ee.sign(hex(v.device_sign_seed), payload), v.wrapped_signature);

  const wrapped = await e2ee.wrapChannelKey(hex(v.device_dh_public), v.device_id, v.channel_id, v.epoch, hex(v.channel_key));
  assert.equal(wrapped.length, e2ee.WRAPPED_LEN);
  same(await e2ee.unwrapChannelKey(priv, v.device_id, v.channel_id, v.epoch, wrapped), v.channel_key);
  same(await e2ee.unwrapChannelKey(priv, v.device_id, v.channel_id, v.epoch, hex(v.wrapped_by_web)), v.channel_key);
  if (process.env.WRITE_VECTORS) {
    writeFileSync(path, JSON.stringify({ ...v, wrapped_by_web: e2ee.toHex(wrapped) }, null, 2) + "\n");
  }

  same(await e2ee.messageKey(hex(v.channel_key), v.channel_id), v.message_key);
  same(await e2ee.mediaKey(hex(v.channel_key), v.channel_id), v.media_key);
  assert.equal(e2ee.mediaKeyIndex(v.epoch), v.media_key_index);
});
