//! Fixed test vectors shared with the web client (`apps/web/tests`).
//!
//! `vectors.json` was written by `cargo test -p tinlink-e2ee -- --ignored
//! write_vectors`; both implementations must reproduce or open every value.
//! `wrapped_by_web` was written by the web client and must open here.

use serde_json::{Value, json};
use tinlink_e2ee::*;
use uuid::Uuid;

const PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vectors.json");

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(v: &Value) -> Vec<u8> {
    let s = v.as_str().expect("hex string");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
        .collect()
}

fn key(v: &Value) -> Key {
    unhex(v).try_into().expect("32 bytes")
}

fn uuid(v: &Value) -> Uuid {
    v.as_str().expect("uuid").parse().expect("uuid")
}

fn fixed(fill: u8) -> Key {
    std::array::from_fn(|i| fill.wrapping_add(i as u8))
}

#[test]
#[ignore = "writes tests/vectors.json"]
fn write_vectors() {
    let user_id: Uuid = "0192f0c4-7a1e-7c3d-9b2a-5e6f708192a3".parse().unwrap();
    let device_id: Uuid = "6f1c2b3a-4d5e-4f60-8a7b-9c0d1e2f3a4b".parse().unwrap();
    let channel_id: Uuid = "0192f0c4-8b2f-7d4e-a3b4-c5d6e7f80912".parse().unwrap();
    let identity_seed = fixed(0x10);
    let sign_seed = fixed(0x40);
    let dh_private = fixed(0x70);
    let sign_public = signing_public(&sign_seed);
    let dh_pub = dh_public(&dh_private).unwrap();
    let payload = certificate_payload(
        user_id,
        device_id,
        DeviceKind::Device,
        Some(&sign_public),
        &dh_pub,
    );
    let recovery = fixed(0xa0);
    let backup_private = backup_dh_private(&recovery);
    let backup_public = dh_public(&backup_private).unwrap();
    let backup_id: Uuid = "1a2b3c4d-5e6f-4a1b-8c2d-3e4f5a6b7c8d".parse().unwrap();
    let backup_payload =
        certificate_payload(user_id, backup_id, DeviceKind::Backup, None, &backup_public);
    let channel_key = fixed(0xd0);
    let epoch = 3;
    let wrapped = wrap_channel_key(&dh_pub, device_id, channel_id, epoch, &channel_key).unwrap();
    let wrap_payload = wrapped_key_payload(channel_id, epoch, device_id, &wrapped);

    let mut vectors = json!({
        "user_id": user_id,
        "identity_seed": hex(&identity_seed),
        "identity_public": hex(&signing_public(&identity_seed)),
        "device_id": device_id,
        "device_sign_seed": hex(&sign_seed),
        "device_sign_public": hex(&sign_public),
        "device_dh_private": hex(&dh_private),
        "device_dh_public": hex(&dh_pub),
        "certificate_payload": hex(&payload),
        "certificate": hex(&sign(&identity_seed, &payload)),
        "recovery_key": hex(&recovery),
        "recovery_key_text": encode_recovery_key(&recovery),
        "backup_id": backup_id,
        "backup_dh_private": hex(&backup_private),
        "backup_dh_public": hex(&backup_public),
        "backup_certificate_payload": hex(&backup_payload),
        "identity_backup": hex(&seal_identity_backup(&recovery, user_id, &identity_seed)),
        "channel_id": channel_id,
        "epoch": epoch,
        "channel_key": hex(&channel_key),
        "wrapped": hex(&wrapped),
        "wrapped_payload": hex(&wrap_payload),
        "wrapped_signature": hex(&sign(&sign_seed, &wrap_payload)),
        "message_key": hex(&message_key(&channel_key, channel_id)),
        "media_key": hex(&media_key(&channel_key, channel_id)),
        "media_key_index": media_key_index(epoch),
    });
    // Keep what the web client wrote.
    if let Ok(old) = std::fs::read_to_string(PATH) {
        let old: Value = serde_json::from_str(&old).unwrap();
        if let Some(w) = old.get("wrapped_by_web") {
            vectors["wrapped_by_web"] = w.clone();
        }
    }
    std::fs::write(PATH, serde_json::to_string_pretty(&vectors).unwrap() + "\n").unwrap();
}

#[test]
fn vectors_match() {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(PATH).unwrap()).unwrap();
    let user_id = uuid(&v["user_id"]);
    let device_id = uuid(&v["device_id"]);
    let channel_id = uuid(&v["channel_id"]);
    let epoch = v["epoch"].as_u64().unwrap() as u32;

    let identity_seed = key(&v["identity_seed"]);
    let identity_public = key(&v["identity_public"]);
    assert_eq!(signing_public(&identity_seed), identity_public);
    let sign_public = signing_public(&key(&v["device_sign_seed"]));
    assert_eq!(sign_public, key(&v["device_sign_public"]));
    let dh_private = key(&v["device_dh_private"]);
    assert_eq!(dh_public(&dh_private).unwrap(), key(&v["device_dh_public"]));

    let payload = certificate_payload(
        user_id,
        device_id,
        DeviceKind::Device,
        Some(&sign_public),
        &key(&v["device_dh_public"]),
    );
    assert_eq!(payload, unhex(&v["certificate_payload"]));
    let certificate: Signature = unhex(&v["certificate"]).try_into().unwrap();
    assert_eq!(sign(&identity_seed, &payload), certificate);
    verify(&identity_public, &payload, &certificate).unwrap();
    assert_eq!(
        verify(&identity_public, &payload[1..], &certificate),
        Err(Error::InvalidSignature)
    );

    let recovery = key(&v["recovery_key"]);
    let text = v["recovery_key_text"].as_str().unwrap();
    assert_eq!(encode_recovery_key(&recovery), text);
    assert_eq!(decode_recovery_key(text).unwrap(), recovery);
    assert_eq!(
        decode_recovery_key(&text.to_lowercase().replace(' ', "-")).unwrap(),
        recovery
    );
    assert_eq!(
        decode_recovery_key(&text[..text.len() - 1]),
        Err(Error::InvalidRecoveryKey)
    );
    let backup_private = backup_dh_private(&recovery);
    assert_eq!(backup_private, key(&v["backup_dh_private"]));
    let backup_public = dh_public(&backup_private).unwrap();
    assert_eq!(backup_public, key(&v["backup_dh_public"]));
    assert_eq!(
        certificate_payload(
            user_id,
            uuid(&v["backup_id"]),
            DeviceKind::Backup,
            None,
            &backup_public
        ),
        unhex(&v["backup_certificate_payload"])
    );
    let backup = unhex(&v["identity_backup"]);
    assert_eq!(
        open_identity_backup(&recovery, user_id, &backup).unwrap(),
        identity_seed
    );
    assert_eq!(
        open_identity_backup(&fixed(0), user_id, &backup),
        Err(Error::Decrypt)
    );

    let channel_key = key(&v["channel_key"]);
    let wrapped = unhex(&v["wrapped"]);
    assert_eq!(wrapped.len(), WRAPPED_LEN);
    assert_eq!(
        unwrap_channel_key(&dh_private, device_id, channel_id, epoch, &wrapped).unwrap(),
        channel_key
    );
    assert_eq!(
        unwrap_channel_key(&dh_private, device_id, channel_id, epoch + 1, &wrapped),
        Err(Error::Decrypt),
        "bound to the epoch"
    );
    assert_eq!(
        unwrap_channel_key(&dh_private, user_id, channel_id, epoch, &wrapped),
        Err(Error::Decrypt),
        "bound to the device"
    );
    let wrap_payload = wrapped_key_payload(channel_id, epoch, device_id, &wrapped);
    assert_eq!(wrap_payload, unhex(&v["wrapped_payload"]));
    let wrap_sig: Signature = unhex(&v["wrapped_signature"]).try_into().unwrap();
    verify(&sign_public, &wrap_payload, &wrap_sig).unwrap();

    assert_eq!(
        message_key(&channel_key, channel_id),
        key(&v["message_key"])
    );
    assert_eq!(media_key(&channel_key, channel_id), key(&v["media_key"]));
    assert_eq!(
        media_key_index(epoch) as i64,
        v["media_key_index"].as_i64().unwrap()
    );

    let web = unhex(&v["wrapped_by_web"]);
    assert_eq!(
        unwrap_channel_key(&dh_private, device_id, channel_id, epoch, &web).unwrap(),
        channel_key
    );
}

#[test]
fn fresh_keys_round_trip() {
    let (device, channel) = (Uuid::new_v4(), Uuid::new_v4());
    let private = random_key();
    let public = dh_public(&private).unwrap();
    let channel_key = random_key();
    let wrapped = wrap_channel_key(&public, device, channel, 1, &channel_key).unwrap();
    assert_eq!(
        unwrap_channel_key(&private, device, channel, 1, &wrapped).unwrap(),
        channel_key
    );
    let recovery = random_key();
    assert_eq!(
        decode_recovery_key(&encode_recovery_key(&recovery)).unwrap(),
        recovery
    );
}
