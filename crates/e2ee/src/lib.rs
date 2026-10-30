//! The end-to-end encryption formats of Tinlink, as described in
//! `docs/e2ee.md`. The web client implements the same in `apps/web/e2ee.js`;
//! `tests/vectors.json` keeps both byte-compatible.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use hkdf::Hkdf;
use hpke::kem::X25519HkdfSha256;
use hpke::{Deserializable, Kem as _, OpModeR, OpModeS, Serializable};
use sha2::Sha256;
use uuid::Uuid;

pub const KEY_LEN: usize = 32;
pub const SIGNATURE_LEN: usize = 64;
/// HPKE encapsulated key, encrypted channel key and tag.
pub const WRAPPED_LEN: usize = 32 + KEY_LEN + 16;
const NONCE_LEN: usize = 12;

pub type Key = [u8; KEY_LEN];
pub type Signature = [u8; SIGNATURE_LEN];

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("invalid key")]
    InvalidKey,
    #[error("invalid signature")]
    InvalidSignature,
    #[error("decryption failed")]
    Decrypt,
    #[error("invalid recovery key")]
    InvalidRecoveryKey,
}

pub type Result<T> = std::result::Result<T, Error>;

type HpkeAead = hpke::aead::AesGcm256;
type Kdf = hpke::kdf::HkdfSha256;

/// What a device certificate certifies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    Device,
    /// The recovery key; it receives channel keys but never signs.
    Backup,
}

/// Fresh random bytes, e.g. a channel key, a recovery key or a private key.
pub fn random_key() -> Key {
    let mut key = [0; KEY_LEN];
    getrandom::fill(&mut key).expect("the system random generator works");
    key
}

/// Public Ed25519 key of a private key (its 32 byte seed).
pub fn signing_public(seed: &Key) -> Key {
    SigningKey::from_bytes(seed).verifying_key().to_bytes()
}

/// Public X25519 key of a private key.
pub fn dh_public(private: &Key) -> Result<Key> {
    let sk = <X25519HkdfSha256 as hpke::Kem>::PrivateKey::from_bytes(private)
        .map_err(|_| Error::InvalidKey)?;
    Ok(X25519HkdfSha256::sk_to_pk(&sk).to_bytes().into())
}

pub fn sign(seed: &Key, message: &[u8]) -> Signature {
    SigningKey::from_bytes(seed).sign(message).to_bytes()
}

pub fn verify(public: &Key, message: &[u8], signature: &Signature) -> Result<()> {
    let key = VerifyingKey::from_bytes(public).map_err(|_| Error::InvalidKey)?;
    key.verify_strict(message, &ed25519_dalek::Signature::from_bytes(signature))
        .map_err(|_| Error::InvalidSignature)
}

/// The bytes an identity key signs to certify a device.
pub fn certificate_payload(
    user_id: Uuid,
    device_id: Uuid,
    kind: DeviceKind,
    sign_key: Option<&Key>,
    dh_key: &Key,
) -> Vec<u8> {
    let mut out = b"tinlink-device-v1".to_vec();
    out.extend_from_slice(user_id.as_bytes());
    out.extend_from_slice(device_id.as_bytes());
    out.push(match kind {
        DeviceKind::Device => 0,
        DeviceKind::Backup => 1,
    });
    if let Some(key) = sign_key {
        out.extend_from_slice(key);
    }
    out.extend_from_slice(dh_key);
    out
}

fn channel_key_info(channel_id: Uuid, epoch: u32) -> Vec<u8> {
    let mut out = b"tinlink-channel-key-v1".to_vec();
    out.extend_from_slice(channel_id.as_bytes());
    out.extend_from_slice(&epoch.to_be_bytes());
    out
}

/// Encrypts a channel key to one device.
pub fn wrap_channel_key(
    recipient_dh_key: &Key,
    recipient_id: Uuid,
    channel_id: Uuid,
    epoch: u32,
    channel_key: &Key,
) -> Result<Vec<u8>> {
    let pk = <X25519HkdfSha256 as hpke::Kem>::PublicKey::from_bytes(recipient_dh_key)
        .map_err(|_| Error::InvalidKey)?;
    let (enc, ct) = hpke::single_shot_seal::<HpkeAead, Kdf, X25519HkdfSha256>(
        &OpModeS::Base,
        &pk,
        &channel_key_info(channel_id, epoch),
        channel_key,
        recipient_id.as_bytes(),
    )
    .map_err(|_| Error::InvalidKey)?;
    let mut out = enc.to_bytes().to_vec();
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Decrypts a channel key wrapped to this device.
pub fn unwrap_channel_key(
    dh_private: &Key,
    device_id: Uuid,
    channel_id: Uuid,
    epoch: u32,
    wrapped: &[u8],
) -> Result<Key> {
    if wrapped.len() != WRAPPED_LEN {
        return Err(Error::Decrypt);
    }
    let sk = <X25519HkdfSha256 as hpke::Kem>::PrivateKey::from_bytes(dh_private)
        .map_err(|_| Error::InvalidKey)?;
    let enc = <X25519HkdfSha256 as hpke::Kem>::EncappedKey::from_bytes(&wrapped[..32])
        .map_err(|_| Error::Decrypt)?;
    let key = hpke::single_shot_open::<HpkeAead, Kdf, X25519HkdfSha256>(
        &OpModeR::Base,
        &sk,
        &enc,
        &channel_key_info(channel_id, epoch),
        &wrapped[32..],
        device_id.as_bytes(),
    )
    .map_err(|_| Error::Decrypt)?;
    key.try_into().map_err(|_| Error::Decrypt)
}

/// The bytes the sending device signs for a wrapped channel key.
pub fn wrapped_key_payload(
    channel_id: Uuid,
    epoch: u32,
    recipient_id: Uuid,
    wrapped: &[u8],
) -> Vec<u8> {
    let mut out = channel_key_info(channel_id, epoch);
    out.extend_from_slice(recipient_id.as_bytes());
    out.extend_from_slice(wrapped);
    out
}

fn hkdf(ikm: &[u8], salt: &[u8], info: &[u8]) -> Key {
    let mut out = [0; KEY_LEN];
    Hkdf::<Sha256>::new(Some(salt), ikm)
        .expand(info, &mut out)
        .expect("32 bytes is a valid HKDF-SHA256 length");
    out
}

/// Key for the messages of a channel epoch.
pub fn message_key(channel_key: &Key, channel_id: Uuid) -> Key {
    hkdf(channel_key, channel_id.as_bytes(), b"tinlink-message-v1")
}

/// Key for voice and video frames of a channel epoch.
pub fn media_key(channel_key: &Key, channel_id: Uuid) -> Key {
    hkdf(channel_key, channel_id.as_bytes(), b"tinlink-media-v1")
}

/// LiveKit key index of an epoch; the SDKs keep 16 keys.
pub fn media_key_index(epoch: u32) -> i32 {
    (epoch % 16) as i32
}

/// Private X25519 key of the backup device of a recovery key.
pub fn backup_dh_private(recovery: &Key) -> Key {
    hkdf(recovery, b"", b"tinlink-backup-dh-v1")
}

fn backup_aad(user_id: Uuid) -> Vec<u8> {
    let mut aad = b"tinlink-identity-backup-v1".to_vec();
    aad.extend_from_slice(user_id.as_bytes());
    aad
}

fn backup_cipher(recovery: &Key) -> Aes256Gcm {
    Aes256Gcm::new(&hkdf(recovery, b"", b"tinlink-identity-backup-v1").into())
}

/// Encrypts the identity key's seed with the recovery key.
pub fn seal_identity_backup(recovery: &Key, user_id: Uuid, identity_seed: &Key) -> Vec<u8> {
    let mut nonce = [0; NONCE_LEN];
    getrandom::fill(&mut nonce).expect("the system random generator works");
    seal_identity_backup_with_nonce(recovery, user_id, identity_seed, nonce)
}

fn seal_identity_backup_with_nonce(
    recovery: &Key,
    user_id: Uuid,
    identity_seed: &Key,
    nonce: [u8; NONCE_LEN],
) -> Vec<u8> {
    let aad = backup_aad(user_id);
    let ct = backup_cipher(recovery)
        .encrypt(
            &Nonce::from(nonce),
            Payload {
                msg: identity_seed,
                aad: &aad,
            },
        )
        .expect("AES-GCM encrypts 32 bytes");
    let mut out = nonce.to_vec();
    out.extend_from_slice(&ct);
    out
}

/// Decrypts the identity key's seed; fails for a wrong recovery key.
pub fn open_identity_backup(recovery: &Key, user_id: Uuid, backup: &[u8]) -> Result<Key> {
    if backup.len() != NONCE_LEN + KEY_LEN + 16 {
        return Err(Error::Decrypt);
    }
    let nonce: [u8; NONCE_LEN] = backup[..NONCE_LEN].try_into().expect("checked length");
    let aad = backup_aad(user_id);
    let seed = backup_cipher(recovery)
        .decrypt(
            &Nonce::from(nonce),
            Payload {
                msg: &backup[NONCE_LEN..],
                aad: &aad,
            },
        )
        .map_err(|_| Error::Decrypt)?;
    seed.try_into().map_err(|_| Error::Decrypt)
}

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// The recovery key as shown to the user: Crockford base32 in groups of four.
pub fn encode_recovery_key(key: &Key) -> String {
    let mut chars = Vec::with_capacity(52);
    let (mut acc, mut bits) = (0u32, 0);
    for &b in key {
        acc = (acc << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            chars.push(CROCKFORD[((acc >> bits) & 31) as usize]);
        }
    }
    if bits > 0 {
        chars.push(CROCKFORD[((acc << (5 - bits)) & 31) as usize]);
    }
    chars
        .chunks(4)
        .map(|c| std::str::from_utf8(c).expect("ASCII"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Parses a recovery key typed by the user, ignoring case, spaces and dashes.
pub fn decode_recovery_key(text: &str) -> Result<Key> {
    let mut out = Vec::with_capacity(KEY_LEN);
    let (mut acc, mut bits) = (0u32, 0);
    for c in text.chars().filter(|c| !matches!(c, ' ' | '-')) {
        let c = match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        };
        let v = CROCKFORD
            .iter()
            .position(|&a| a as char == c)
            .ok_or(Error::InvalidRecoveryKey)? as u32;
        acc = (acc << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    // 52 characters carry 256 bits and four zero bits.
    if out.len() != KEY_LEN || acc & ((1 << bits) - 1) != 0 {
        return Err(Error::InvalidRecoveryKey);
    }
    out.try_into().map_err(|_| Error::InvalidRecoveryKey)
}
