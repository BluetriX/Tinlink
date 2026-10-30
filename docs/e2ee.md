# End-to-end encryption

Status: the server stores keys; the web and the desktop client set up
devices, the recovery key and channel keys, and can encrypt voice, webcams
and screen shares with channel keys (opt-in in the voice settings for now).
The desktop UI runs the web client's `e2ee.js` and `keyring.js` (copied by
its build script) and hands only the derived media keys to the Rust core.
Messages and files are not encrypted yet.

The server stores public keys and wrapped channel keys and decides who may
fetch them. It never sees a private key, a channel key, a message or a media
frame in the clear. Clients check every signature themselves, so a hostile
server can withhold keys or add fake members (visible in the member list),
but cannot forge keys of existing users.

This document is the contract between the web and the desktop client. Both
must produce and accept exactly these bytes: `crates/e2ee` (Rust, used by
the desktop client) and `apps/web/e2ee.js` (WebCrypto only) are both tested
against `crates/e2ee/tests/vectors.json`, which also holds a key wrapped by
the web code that the Rust tests open.

## Encoding

- Binary values in JSON are standard base64 with padding.
- In signed or derived data, a UUID is its 16 bytes in RFC 9562 order, an
  epoch is a big-endian u32, strings are UTF-8 without terminator, and `||`
  is concatenation.

## Keys

| Key | Type | Where the private part lives |
| --- | --- | --- |
| Identity key, one per user | Ed25519 | Every approved device of the user, and encrypted with the recovery key on the server (`identity_backup`) |
| Device signing key | Ed25519 | That device only (browser: IndexedDB of the server's origin) |
| Device key agreement key | X25519 | That device only |
| Backup key, one per user | X25519, derived from the recovery key | Nowhere; derived again when the recovery key is entered |
| Channel key, one per channel and epoch | 32 random bytes | Wrapped to every device that may read the channel |

### Device certificate

A device is trusted when the identity key of its user signs it:

```
certificate = Ed25519-Sign(identity, "tinlink-device-v1" || user_id || device_id || kind || sign_key || dh_key)
```

`kind` is one byte, 0 for a device and 1 for the backup. The backup has no
signing key; `sign_key` is then empty. A new device registers without a
certificate and gets one from an existing device of the same user (after the
user compares a code shown on both) or from the recovery key.

The first identity key a client sees for a user is remembered. A different
key later is shown as a warning in every shared channel.

### Recovery key

32 random bytes, shown once to the user as Crockford base32 in groups of
four characters. From it:

```
backup_dh_private = HKDF-SHA256(ikm = recovery, salt = "", info = "tinlink-backup-dh-v1", 32)
backup_aead_key   = HKDF-SHA256(ikm = recovery, salt = "", info = "tinlink-identity-backup-v1", 32)
identity_backup   = nonce(12) || AES-256-GCM(backup_aead_key, nonce, identity_seed(32),
                                             aad = "tinlink-identity-backup-v1" || user_id)
```

The backup key is registered as a device of kind `backup`. Every device
shares channel keys with it like with any other device, so the recovery key
alone restores the identity and the whole history.

## Channel keys

A channel (group channel or direct message) has numbered epochs, starting at
1. A device sending in a channel with no epoch, or when the server reports
`rotate: true` (someone holding the newest key lost access or a device was
revoked), creates the next epoch: a new random key wrapped to every approved
device of everyone who can read the channel, including its own and the
backups.

Wrapping uses HPKE (RFC 9180) in base mode with DHKEM(X25519, HKDF-SHA256),
HKDF-SHA256 and AES-256-GCM (suite 0x0020, 0x0001, 0x0002):

```
info      = "tinlink-channel-key-v1" || channel_id || epoch
wrapped   = enc(32) || Seal(recipient_dh_key, info, aad = recipient_device_id, channel_key)   (80 bytes)
signature = Ed25519-Sign(sender_sign_key, "tinlink-channel-key-v1" || channel_id || epoch
                                          || recipient_device_id || wrapped)
```

A recipient accepts a wrapped key only if the sending device carries a valid
certificate of its user's identity key.

New members and new devices see the whole history: any device that holds
older epochs shares them when `GET /channels/{id}/keys/missing` lists a
device without them.

### Derived keys

```
message_key = HKDF-SHA256(ikm = channel_key, salt = channel_id, info = "tinlink-message-v1", 32)
media_key   = HKDF-SHA256(ikm = channel_key, salt = channel_id, info = "tinlink-media-v1", 32)
```

Voice, webcams and screen shares use LiveKit frame encryption (AES-GCM) with
the media key of the newest epoch, passed to both SDKs as the base64 string
of `media_key` (both run PBKDF2 over a string key the same way) at key index
`epoch mod 16`, so frames of the previous epoch still decrypt while everyone
switches.

## Server API

| Request | Purpose |
| --- | --- |
| `PUT /api/users/me/identity` | Set the identity key and its backup; `reset: true` replaces a different key and revokes all devices |
| `GET /api/users/me/identity-backup` | Fetch the encrypted identity key |
| `GET /api/users/{id}/keys` | Identity key and approved devices of a user who shares a group or a DM |
| `POST /api/devices` | Register a device (`id` chosen by the client) or the backup |
| `PUT /api/devices/{id}/certificate` | Approve one of the caller's devices |
| `DELETE /api/devices/{id}` | Revoke one of the caller's devices |
| `GET /api/channels/{id}/keys?device=` | Newest epoch, `rotate` flag and all keys wrapped to that device |
| `GET /api/channels/{id}/keys/devices` | Identity keys and approved devices of everyone who can read the channel |
| `GET /api/channels/{id}/keys/missing?device=` | Devices of readers lacking epochs this device holds |
| `POST /api/channels/{id}/keys` | Start the next epoch or share an existing one |

Gateway events: `keys_changed` (a user's identity key or devices changed)
and `channel_keys_added` (new wrapped keys in a channel).

## What the server still sees

Who is in which group and channel, who writes when and how much, who is in
a voice channel, online status, typing, names, avatars, channel names and
roles. Mentions are sent as user ids next to the encrypted text so unread
markers keep working. Message search moves into the clients.
